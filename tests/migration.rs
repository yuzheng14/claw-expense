use std::{fs, path::Path, process::Command};

use claw_expense::{
    archive,
    models::{ConfirmPendingExpense, Kind, NewPendingExpense, NewTransaction, UpdateTransaction},
    store::Store,
};
use serde_json::Value;
use sqlx::{SqlitePool, migrate::Migrator, sqlite::SqliteConnectOptions};

/// Build the same schema and real SQLx checksums used by the published 0.1.0
/// release, not a current database with its migration rows merely deleted.
async fn legacy_store(path: &Path) -> Store {
    store_at_version(path, 1).await
}

async fn store_at_version(path: &Path, version: i64) -> Store {
    let pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let migrations = sqlx::migrate!()
        .iter()
        .filter(|migration| migration.version <= version)
        .cloned()
        .collect();
    Migrator::with_migrations(migrations)
        .run(&pool)
        .await
        .unwrap();
    Store { pool }
}

fn legacy_expense() -> NewTransaction {
    NewTransaction {
        kind: Kind::Expense,
        amount: "92233720368547758.07".parse().unwrap(),
        date: "2026-09-22".into(),
        category: Some("购物".into()),
        note: Some("v0.1.0 原有账单".into()),
        channel: Some("信用卡".into()),
        original_id: None,
    }
}

#[tokio::test]
async fn opening_v0_1_0_upgrades_without_losing_cny_audit_or_idempotency() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite");
    let old = legacy_store(&path).await;
    let expense = old
        .add(legacy_expense(), Some("legacy-create"))
        .await
        .unwrap();
    let history =
        serde_json::to_value(old.history(&expense.transaction.id).await.unwrap()).unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&old.pool)
            .await
            .unwrap(),
        1
    );
    old.pool.close().await;

    let upgraded = Store::open(&path, false).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        sqlx::migrate!().iter().count() as i64
    );
    let detail = upgraded.get(&expense.transaction.id).await.unwrap();
    assert_eq!(detail.transaction.amount.minor(), i64::MAX);
    assert_eq!(detail.transaction.category, "购物");
    assert!(detail.foreign_expense.is_none());
    assert_eq!(
        serde_json::to_value(upgraded.history(&expense.transaction.id).await.unwrap()).unwrap(),
        history
    );
    let replay = upgraded
        .add(legacy_expense(), Some("legacy-create"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.transaction.id, expense.transaction.id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM transactions")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_expenses")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        0
    );
    upgraded.pool.close().await;
}

#[tokio::test]
async fn new_cli_restores_a_v0_1_0_backup_without_modifying_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy-backup.sqlite");
    let old = legacy_store(&source).await;
    let expense = old
        .add(legacy_expense(), Some("legacy-create"))
        .await
        .unwrap();
    let history =
        serde_json::to_value(old.history(&expense.transaction.id).await.unwrap()).unwrap();
    old.pool.close().await;
    let source_bytes = fs::read(&source).unwrap();

    let destination = directory.path().join("restored.sqlite");
    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .arg("--db")
        .arg(&destination)
        .args(["--json", "restore", "--input"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(fs::read(&source).unwrap(), source_bytes);

    let restored = Store::open(&destination, false).await.unwrap();
    assert_eq!(
        restored
            .get(&expense.transaction.id)
            .await
            .unwrap()
            .transaction
            .amount
            .minor(),
        i64::MAX
    );
    assert_eq!(
        serde_json::to_value(restored.history(&expense.transaction.id).await.unwrap()).unwrap(),
        history
    );
    assert!(
        restored
            .add(legacy_expense(), Some("legacy-create"))
            .await
            .unwrap()
            .replayed
    );
    restored.pool.close().await;
    assert_eq!(fs::read(&source).unwrap(), source_bytes);
}

#[tokio::test]
async fn restore_rejects_empty_gapped_unknown_failed_or_changed_migration_history() {
    for mutation in [
        "DELETE FROM _sqlx_migrations",
        "DELETE FROM _sqlx_migrations WHERE version = 1",
        "UPDATE _sqlx_migrations SET version = 999 WHERE version = 2",
        "UPDATE _sqlx_migrations SET success = 0 WHERE version = 1",
        "UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("invalid.sqlite");
        let store = Store::open(&source, true).await.unwrap();
        sqlx::query(mutation).execute(&store.pool).await.unwrap();
        store.pool.close().await;
        let before = fs::read(&source).unwrap();
        let destination = directory.path().join("rejected.sqlite");
        let error = archive::restore(&source, &destination).await.unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_BACKUP", "{mutation}");
        assert!(!destination.exists(), "{mutation}");
        assert_eq!(fs::read(&source).unwrap(), before, "{mutation}");
    }
}

fn pending_expense(currency: &str, amount: &str) -> NewPendingExpense {
    NewPendingExpense {
        currency: currency.into(),
        amount: amount.into(),
        date: "2026-09-28".into(),
        category: Some("购物".into()),
        merchant: Some("海外商户".into()),
        note: Some("原币金额等待银行换算".into()),
        channel: Some("信用卡".into()),
    }
}

#[tokio::test]
async fn pending_backup_restore_and_export_preserve_units_states_history_and_replays() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("live.sqlite"), true)
        .await
        .unwrap();
    let jpy_input = pending_expense("JPY", "9223372036854775807");
    let jpy = store
        .add_pending(jpy_input.clone(), Some("jpy-create"))
        .await
        .unwrap();
    store
        .snooze_pending(&jpy.pending.id, "2026-10-06", Some("jpy-snooze"))
        .await
        .unwrap();
    let usd = store
        .add_pending(pending_expense("USD", "20.00"), Some("usd-create"))
        .await
        .unwrap();
    let confirmation = ConfirmPendingExpense {
        amount: "143.29".parse().unwrap(),
        posted_date: Some("2026-10-03".into()),
    };
    let confirmed = store
        .confirm_pending(&usd.pending.id, confirmation.clone(), Some("usd-confirm"))
        .await
        .unwrap();
    let eur = store
        .add_pending(pending_expense("EUR", "1.29"), Some("eur-create"))
        .await
        .unwrap();
    store
        .cancel_pending(&eur.pending.id, Some("eur-cancel"))
        .await
        .unwrap();

    let exported_path = directory.path().join("export.json");
    archive::export(&store, &exported_path).await.unwrap();
    let exported: Value = serde_json::from_slice(&fs::read(&exported_path).unwrap()).unwrap();
    assert_eq!(exported["version"], 2);
    assert_eq!(exported["base_currency"], "CNY");
    assert_eq!(exported["integer_encoding"], "decimal_string");
    assert!(exported.get("amount_unit").is_none());
    assert_eq!(
        exported["amount_units"]["transactions.amount_minor"]["unit"],
        "fen"
    );
    assert_eq!(
        exported["amount_units"]["pending_expenses.confirmed_amount_minor"]["currency"],
        "CNY"
    );
    assert_eq!(
        exported["amount_units"]["pending_expenses.amount_minor"]["exponents"]["JPY"],
        0
    );
    assert_eq!(
        exported["amount_units"]["pending_expenses.amount_minor"]["exponents"]["USD"],
        2
    );
    let rows = exported["tables"]["pending_expenses"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let find = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
    assert_eq!(find(&jpy.pending.id)["amount_minor"], "9223372036854775807");
    assert_eq!(find(&jpy.pending.id)["remind_on"], "2026-10-06");
    assert!(find(&jpy.pending.id)["confirmed_amount_minor"].is_null());
    assert_eq!(find(&usd.pending.id)["amount_minor"], "2000");
    assert_eq!(find(&usd.pending.id)["confirmed_amount_minor"], "14329");
    assert_eq!(find(&usd.pending.id)["date"], "2026-09-28");
    assert_eq!(find(&usd.pending.id)["posted_date"], "2026-10-03");
    assert_eq!(find(&eur.pending.id)["status"], "cancelled");
    assert_eq!(
        exported["tables"]["pending_audit_log"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(exported["tables"]["audit_log"].as_array().unwrap().len(), 1);
    assert_eq!(
        exported["tables"]["idempotency"].as_array().unwrap().len(),
        6
    );

    let backup_path = directory.path().join("backup.sqlite");
    archive::backup(&store, &backup_path).await.unwrap();
    let restored_path = directory.path().join("restored.sqlite");
    archive::restore(&backup_path, &restored_path)
        .await
        .unwrap();
    let restored = Store::open(&restored_path, false).await.unwrap();
    let restored_export_path = directory.path().join("restored-export.json");
    archive::export(&restored, &restored_export_path)
        .await
        .unwrap();
    let restored_export: Value =
        serde_json::from_slice(&fs::read(&restored_export_path).unwrap()).unwrap();
    assert_eq!(restored_export["tables"], exported["tables"]);
    assert_eq!(restored_export["amount_units"], exported["amount_units"]);

    let repeated_jpy = restored
        .add_pending(jpy_input, Some("jpy-create"))
        .await
        .unwrap();
    assert!(repeated_jpy.replayed);
    assert_eq!(repeated_jpy.pending.id, jpy.pending.id);
    let repeated_confirm = restored
        .confirm_pending(&usd.pending.id, confirmation, Some("usd-confirm"))
        .await
        .unwrap();
    assert!(repeated_confirm.replayed);
    assert_eq!(
        repeated_confirm.transaction.unwrap().id,
        confirmed.transaction.unwrap().id
    );
    assert!(
        restored
            .snooze_pending(&jpy.pending.id, "2026-10-06", Some("jpy-snooze"))
            .await
            .unwrap()
            .replayed
    );
    assert!(
        restored
            .cancel_pending(&eur.pending.id, Some("eur-cancel"))
            .await
            .unwrap()
            .replayed
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM transactions")
            .fetch_one(&restored.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_expenses")
            .fetch_one(&restored.pool)
            .await
            .unwrap(),
        3
    );
    restored.pool.close().await;
    store.pool.close().await;
}

#[tokio::test]
async fn v2_currency_expansion_preserves_all_records_and_accepts_twd_with_foreign_keys_enabled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v2.sqlite");
    let old = store_at_version(&path, 2).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&old.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&old.pool)
            .await
            .unwrap(),
        2
    );

    let jpy_input = pending_expense("JPY", "10000");
    let jpy = old
        .add_pending(jpy_input.clone(), Some("v2-jpy-create"))
        .await
        .unwrap();
    old.snooze_pending(&jpy.pending.id, "2026-10-06", Some("v2-jpy-snooze"))
        .await
        .unwrap();
    let usd = old
        .add_pending(pending_expense("USD", "20.00"), Some("v2-usd-create"))
        .await
        .unwrap();
    let confirmation = ConfirmPendingExpense {
        amount: "143.29".parse().unwrap(),
        posted_date: Some("2026-10-03".into()),
    };
    let confirmed = old
        .confirm_pending(
            &usd.pending.id,
            confirmation.clone(),
            Some("v2-usd-confirm"),
        )
        .await
        .unwrap();
    let cny_id = &confirmed.transaction.as_ref().unwrap().id;
    // Confirmation snapshots are immutable; later legitimate CNY edits/refunds
    // must survive a table rebuild without reapplying the original insert trigger.
    old.update(
        cny_id,
        UpdateTransaction {
            amount: Some("135.79".parse().unwrap()),
            ..UpdateTransaction::default()
        },
        Some("v2-cny-edit"),
    )
    .await
    .unwrap();
    let refund_input = NewTransaction {
        kind: Kind::Refund,
        amount: "150.00".parse().unwrap(),
        date: "2026-10-04".into(),
        category: None,
        note: Some("超额返还".into()),
        channel: Some("信用卡".into()),
        original_id: Some(cny_id.clone()),
    };
    let refund = old
        .add(refund_input.clone(), Some("v2-refund"))
        .await
        .unwrap();
    let eur = old
        .add_pending(pending_expense("EUR", "1.29"), Some("v2-eur-create"))
        .await
        .unwrap();
    old.cancel_pending(&eur.pending.id, Some("v2-eur-cancel"))
        .await
        .unwrap();

    // Prove this really is the old CHECK constraint, not a current schema
    // disguised by removing its latest migration row.
    assert!(
        sqlx::query("INSERT INTO pending_expenses (id, currency, amount_minor, date, category, remind_on, created_at, updated_at) SELECT 'unsupported-twd', 'TWD', 100, date, category, remind_on, created_at, updated_at FROM pending_expenses WHERE id = ?")
            .bind(&jpy.pending.id)
            .execute(&old.pool)
            .await
            .is_err()
    );
    let old_schema: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT name, type, sql FROM sqlite_schema WHERE type IN ('index', 'trigger') AND name LIKE 'pending_%' ORDER BY name",
    )
    .fetch_all(&old.pool)
    .await
    .unwrap();
    assert_eq!(old_schema.len(), 9);
    let before_path = directory.path().join("before.json");
    archive::export(&old, &before_path).await.unwrap();
    let before: Value = serde_json::from_slice(&fs::read(&before_path).unwrap()).unwrap();
    let old_detail = serde_json::to_value(old.get(cny_id).await.unwrap()).unwrap();
    let old_pending_history =
        serde_json::to_value(old.pending_history(&usd.pending.id).await.unwrap()).unwrap();
    old.pool.close().await;

    let upgraded = Store::open(&path, false).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        3
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&upgraded.pool)
            .await
            .unwrap()
            .is_empty()
    );
    let new_schema: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT name, type, sql FROM sqlite_schema WHERE type IN ('index', 'trigger') AND name LIKE 'pending_%' ORDER BY name",
    )
    .fetch_all(&upgraded.pool)
    .await
    .unwrap();
    assert_eq!(new_schema, old_schema);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT \"table\" FROM pragma_foreign_key_list('pending_audit_log')",
        )
        .fetch_all(&upgraded.pool)
        .await
        .unwrap(),
        ["pending_expenses"]
    );
    let after_path = directory.path().join("after.json");
    archive::export(&upgraded, &after_path).await.unwrap();
    let after: Value = serde_json::from_slice(&fs::read(&after_path).unwrap()).unwrap();
    assert_eq!(after["tables"], before["tables"]);
    assert_eq!(
        serde_json::to_value(upgraded.get(cny_id).await.unwrap()).unwrap(),
        old_detail
    );
    assert_eq!(
        serde_json::to_value(upgraded.pending_history(&usd.pending.id).await.unwrap()).unwrap(),
        old_pending_history
    );

    assert!(
        upgraded
            .add_pending(jpy_input, Some("v2-jpy-create"))
            .await
            .unwrap()
            .replayed
    );
    assert!(
        upgraded
            .snooze_pending(&jpy.pending.id, "2026-10-06", Some("v2-jpy-snooze"))
            .await
            .unwrap()
            .replayed
    );
    let repeated_confirmation = upgraded
        .confirm_pending(&usd.pending.id, confirmation, Some("v2-usd-confirm"))
        .await
        .unwrap();
    assert!(repeated_confirmation.replayed);
    assert_eq!(
        repeated_confirmation
            .transaction
            .unwrap()
            .amount
            .to_string(),
        "143.29"
    );
    let repeated_refund = upgraded.add(refund_input, Some("v2-refund")).await.unwrap();
    assert!(repeated_refund.replayed);
    assert_eq!(repeated_refund.transaction.id, refund.transaction.id);
    assert!(
        upgraded
            .cancel_pending(&eur.pending.id, Some("v2-eur-cancel"))
            .await
            .unwrap()
            .replayed
    );
    // The reinstalled closed-state trigger must still protect old snapshots.
    assert!(
        sqlx::query("UPDATE pending_expenses SET note = 'forbidden' WHERE id = ?")
            .bind(&usd.pending.id)
            .execute(&upgraded.pool)
            .await
            .is_err()
    );

    let twd = upgraded
        .add_pending(pending_expense("TWD", "123.45"), Some("twd-create"))
        .await
        .unwrap();
    assert_eq!(twd.pending.currency, "TWD");
    assert_eq!(twd.pending.amount, "123.45");
    let twd_confirmed = upgraded
        .confirm_pending(
            &twd.pending.id,
            ConfirmPendingExpense {
                amount: "27.12".parse().unwrap(),
                posted_date: None,
            },
            Some("twd-confirm"),
        )
        .await
        .unwrap();
    assert_eq!(
        twd_confirmed.transaction.unwrap().amount.to_string(),
        "27.12"
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&upgraded.pool)
            .await
            .unwrap()
            .is_empty()
    );
    upgraded.pool.close().await;
}
