//! Consistent, no-clobber snapshots and lossless JSON exports.

use std::{io::Write, path::Path};

use chrono::Utc;
use serde_json::{Map, Value, json};
use sqlx::{
    Column, Row, SqlitePool, TypeInfo, ValueRef,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tempfile::NamedTempFile;

use crate::{
    error::{AppError, Result},
    store::Store,
};

const APPLICATION_ID: i64 = 1_129_071_960;

fn output_file(path: &Path) -> Result<NamedTempFile> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Err(AppError::invalid("输出路径必须是一个文件"));
    }
    if path.symlink_metadata().is_ok() {
        return Err(AppError::new(
            "FILE_EXISTS",
            "目标文件已存在；请指定一个新文件，现有文件不会被覆盖",
        ));
    }
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(NamedTempFile::new_in(parent)?)
}

fn publish(file: NamedTempFile, path: &Path) -> Result<()> {
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| {
        AppError::new(
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                "FILE_EXISTS"
            } else {
                "IO_ERROR"
            },
            error.error.to_string(),
        )
    })?;
    Ok(())
}

async fn snapshot(pool: &SqlitePool, output: &Path) -> Result<()> {
    crate::paths::ensure_unused_database_path(output)?;
    let file = output_file(output)?;
    let path = file
        .path()
        .to_str()
        .ok_or_else(|| AppError::invalid("备份路径必须是有效的 UTF-8"))?;
    // VACUUM INTO reads a consistent SQLite snapshot, including committed WAL
    // contents. It can write to a pre-created empty temporary file. Publication
    // is atomic and must never replace an existing ledger or backup.
    sqlx::query("VACUUM INTO ?")
        .bind(path)
        .execute(pool)
        .await?;
    crate::paths::ensure_unused_database_path(output)?;
    publish(file, output)
}

pub async fn backup(store: &Store, output: &Path) -> Result<()> {
    snapshot(&store.pool, output).await
}

pub async fn restore(input: &Path, output: &Path) -> Result<()> {
    if !input.is_file() {
        return Err(AppError::invalid("备份文件不存在或不是普通文件"));
    }
    // Check the destination before opening the input; never touch an existing
    // destination database or its WAL/SHM files.
    crate::paths::ensure_unused_database_path(output)?;
    let options = SqliteConnectOptions::new()
        .filename(input)
        .read_only(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let result = async {
        let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&pool)
            .await?;
        if application_id != APPLICATION_ID {
            return Err(AppError::invalid("该文件不是 claw-expense 账本备份"));
        }
        let check: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&pool)
            .await?;
        if check.as_slice() != ["ok"] {
            return Err(AppError::new("INVALID_BACKUP", "备份完整性检查失败"));
        }
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await?
            .is_empty()
        {
            return Err(AppError::new("INVALID_BACKUP", "备份关联关系检查失败"));
        }
        let applied =
            sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await?;
        let migrator = sqlx::migrate!();
        let expected: Vec<_> = migrator
            .iter()
            .filter(|migration| !migration.migration_type.is_down_migration())
            .collect();
        if applied.len() != expected.len() {
            return Err(AppError::new(
                "UNSUPPORTED_BACKUP",
                "备份迁移记录与当前程序不兼容",
            ));
        }
        for (row, migration) in applied.iter().zip(expected) {
            if row.try_get::<i64, _>("version")? != migration.version
                || !row.try_get::<bool, _>("success")?
                || row.try_get::<Vec<u8>, _>("checksum")?.as_slice() != migration.checksum.as_ref()
            {
                return Err(AppError::new(
                    "UNSUPPORTED_BACKUP",
                    "备份包含未完成或校验不匹配的迁移",
                ));
            }
        }
        snapshot(&pool, output).await
    }
    .await;
    pool.close().await;
    result
}

pub async fn export(store: &Store, output: &Path) -> Result<()> {
    let mut file = output_file(output)?;
    let mut transaction = store.pool.begin().await?;
    let mut tables = Map::new();
    // All queries are static. A single read transaction keeps related entries,
    // audit history, and idempotency responses in the same database snapshot.
    for (name, query) in [
        ("categories", "SELECT * FROM categories ORDER BY name"),
        ("transactions", "SELECT * FROM transactions ORDER BY id"),
        ("audit_log", "SELECT * FROM audit_log ORDER BY id"),
        (
            "idempotency",
            "SELECT * FROM idempotency ORDER BY request_id",
        ),
    ] {
        let rows = sqlx::query(query).fetch_all(&mut *transaction).await?;
        let mut values = Vec::with_capacity(rows.len());
        for row in rows {
            let mut object = Map::new();
            for column in row.columns() {
                let raw = row.try_get_raw(column.ordinal())?;
                let value = if raw.is_null() {
                    Value::Null
                } else {
                    match raw.type_info().name() {
                        // Integers are strings in the lossless archive too, so
                        // even clients limited to 53-bit numbers can read it.
                        "INTEGER" => {
                            Value::String(row.try_get::<i64, _>(column.ordinal())?.to_string())
                        }
                        "TEXT" => Value::String(row.try_get::<String, _>(column.ordinal())?),
                        other => {
                            return Err(AppError::new(
                                "INVALID_STORAGE_TYPE",
                                format!("不支持导出的数据类型: {other}"),
                            ));
                        }
                    }
                };
                object.insert(column.name().to_owned(), value);
            }
            values.push(Value::Object(object));
        }
        tables.insert(name.to_owned(), Value::Array(values));
    }
    transaction.commit().await?;
    let document = json!({
        "format": "claw-expense-export",
        "version": 1,
        "currency": "CNY",
        "amount_unit": "fen",
        "integer_encoding": "decimal_string",
        "created_at": Utc::now().to_rfc3339(),
        "tables": tables
    });
    serde_json::to_writer_pretty(file.as_file_mut(), &document)?;
    file.as_file_mut().write_all(b"\n")?;
    publish(file, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Kind, NewTransaction};

    #[tokio::test]
    async fn backup_includes_committed_wal_without_closing_the_live_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.sqlite");
        let store = Store::open(&path, true).await.unwrap();
        let entry = NewTransaction {
            kind: Kind::Expense,
            amount: "98.01".parse().unwrap(),
            date: "2026-09-22".into(),
            category: None,
            note: Some("WAL snapshot".into()),
            channel: None,
            original_id: None,
        };
        let written = store.add(entry, Some("wal-request")).await.unwrap();
        assert!(
            std::fs::metadata(directory.path().join("live.sqlite-wal"))
                .unwrap()
                .len()
                > 0
        );
        let snapshot_path = directory.path().join("backup.sqlite");
        backup(&store, &snapshot_path).await.unwrap();
        let restored_path = directory.path().join("restored.sqlite");
        restore(&snapshot_path, &restored_path).await.unwrap();
        let restored = Store::open(&restored_path, false).await.unwrap();
        let detail = restored.get(&written.transaction.id).await.unwrap();
        assert_eq!(detail.transaction.amount.to_string(), "98.01");
        assert_eq!(
            restored
                .history(&written.transaction.id)
                .await
                .unwrap()
                .len(),
            1
        );
        restored.pool.close().await;
        store.pool.close().await;
    }

    #[tokio::test]
    async fn restore_rejects_mismatched_migration_checksum_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let store = Store::open(&source, true).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        let output = directory.path().join("restored.sqlite");
        assert_eq!(
            restore(&source, &output).await.unwrap_err().code,
            "UNSUPPORTED_BACKUP"
        );
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn restore_rejects_a_failed_migration_even_when_max_successful_version_matches() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let store = Store::open(&source, true).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (2, 'failed', 0, X'00', 0)")
            .execute(&store.pool).await.unwrap();
        store.pool.close().await;
        let output = directory.path().join("restored.sqlite");
        assert_eq!(
            restore(&source, &output).await.unwrap_err().code,
            "UNSUPPORTED_BACKUP"
        );
        assert!(!output.exists());
    }
}
