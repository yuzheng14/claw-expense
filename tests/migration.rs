use std::{fs, path::Path, process::Command};

use claw_expense::{
    archive,
    models::{
        ConfirmPendingExpense, Kind, NewPendingExpense, NewTransaction, UpdateTransaction,
        WriteResult,
    },
    store::Store,
};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool, migrate::Migrator, sqlite::SqliteConnectOptions};

/// Build the same schema and real SQLx checksums used by the published 0.1.0
/// release, not a current database with its migration rows merely deleted.
async fn legacy_store(path: &Path) -> Store {
    let migration = sqlx::migrate!()
        .iter()
        .find(|migration| migration.version == 1)
        .unwrap()
        .clone();
    let checksum = migration
        .checksum
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        checksum,
        "4ae224454d89397fdaa4df34a26e1603462c6ed2f013b8eccab843e890102dab9a25b5559df753f757c24722b4db31fb",
        "published v0.1.0 migration must not be edited"
    );
    let pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    Migrator::with_migrations(vec![migration])
        .run(&pool)
        .await
        .unwrap();
    Store { pool }
}

const LEGACY_TIMESTAMP: &str = "2026-10-04T12:00:00Z";

// These helpers deliberately write only historical columns and construct the
// historical JSON shape explicitly. Calling today's Store write methods here
// would make old-schema tests depend on columns they are meant to migrate.
async fn legacy_request(store: &Store, id: &str, payload: Value, response: Value) {
    assert!(!payload.to_string().contains("occurred_at"));
    assert!(!response.to_string().contains("occurred_at"));
    sqlx::query("INSERT INTO idempotency (request_id, payload, response_json, created_at) VALUES (?, ?, ?, ?)")
        .bind(id).bind(payload.to_string()).bind(response.to_string()).bind(LEGACY_TIMESTAMP)
        .execute(&store.pool).await.unwrap();
}

async fn legacy_audit(store: &Store, action: &str, before: Option<&Value>, after: &Value) {
    let sql = "INSERT INTO audit_log (transaction_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)";
    sqlx::query(sql)
        .bind(after["id"].as_str().unwrap())
        .bind(action)
        .bind(before.map(Value::to_string))
        .bind(after.to_string())
        .bind(LEGACY_TIMESTAMP)
        .execute(&store.pool)
        .await
        .unwrap();
}

async fn legacy_snapshot(store: &Store) -> Value {
    // Keep historical JSON as raw strings: deserializing and rebuilding it could
    // hide an unintended audit or idempotency rewrite during the upgrade.
    let transactions: Vec<String> = sqlx::query_scalar(
        "SELECT json_object('id', id, 'kind', kind, 'amount_minor', amount_minor, 'currency', currency, 'category', category, 'date', date, 'note', note, 'channel', channel, 'original_id', original_id, 'voided', voided, 'created_at', created_at, 'updated_at', updated_at) FROM transactions ORDER BY id",
    ).fetch_all(&store.pool).await.unwrap();
    let audit: Vec<(i64, String, String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT id, transaction_id, action, before_json, after_json, created_at FROM audit_log ORDER BY id",
    ).fetch_all(&store.pool).await.unwrap();
    let requests: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT request_id, payload, response_json, created_at FROM idempotency ORDER BY request_id",
    ).fetch_all(&store.pool).await.unwrap();
    json!({ "transactions": transactions, "audit": audit, "idempotency": requests })
}

async fn legacy_history(store: &Store, id: &str) -> Value {
    let sql = "SELECT * FROM audit_log WHERE transaction_id = ? ORDER BY id";
    Value::Array(sqlx::query(sql).bind(id).fetch_all(&store.pool).await.unwrap().iter().map(|row| {
        let mut item = json!({
            "id": row.get::<i64, _>("id"), "action": row.get::<String, _>("action"),
            "before": row.get::<Option<String>, _>("before_json").map(|text| serde_json::from_str::<Value>(&text).unwrap()),
            "after": serde_json::from_str::<Value>(&row.get::<String, _>("after_json")).unwrap(),
            "created_at": row.get::<String, _>("created_at")
        });
        item["transaction_id"] = json!(id);
        item
    }).collect())
}

async fn seed_legacy_transaction(
    store: &Store,
    input: NewTransaction,
    id: &str,
    request: Option<&str>,
) -> WriteResult {
    let category = if input.kind == Kind::Refund {
        sqlx::query_scalar::<_, String>("SELECT category FROM transactions WHERE id = ?")
            .bind(&input.original_id)
            .fetch_one(&store.pool)
            .await
            .unwrap()
    } else {
        input.category.clone().unwrap_or_else(|| "其他支出".into())
    };
    let old_input = json!({"kind": input.kind, "amount": input.amount, "date": input.date,
        "category": input.category, "note": input.note, "channel": input.channel, "original_id": input.original_id});
    let record = json!({"id": id, "kind": input.kind, "amount": input.amount, "currency": "CNY",
        "category": category, "date": input.date, "note": input.note, "channel": input.channel,
        "original_id": input.original_id, "voided": false, "created_at": LEGACY_TIMESTAMP, "updated_at": LEGACY_TIMESTAMP});
    sqlx::query("INSERT INTO transactions (id, kind, amount_minor, category, date, note, channel, original_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(input.kind.as_str()).bind(input.amount.minor())
        .bind(if input.kind == Kind::Refund { None } else { Some(category) })
        .bind(&input.date).bind(&input.note).bind(&input.channel).bind(&input.original_id)
        .bind(LEGACY_TIMESTAMP).bind(LEGACY_TIMESTAMP).execute(&store.pool).await.unwrap();
    legacy_audit(store, "create", None, &record).await;
    let response = json!({"transaction": record, "replayed": false, "affected_ids": [id]});
    if let Some(request) = request {
        legacy_request(
            store,
            request,
            json!({"operation": "add", "input": old_input}),
            response.clone(),
        )
        .await;
    }
    serde_json::from_value(response).unwrap()
}

fn legacy_expense() -> NewTransaction {
    NewTransaction {
        kind: Kind::Expense,
        amount: "92233720368547758.07".parse().unwrap(),
        date: "2026-09-22".into(),
        occurred_at: None,
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
    let expense =
        seed_legacy_transaction(&old, legacy_expense(), "txn_legacy", Some("legacy-create")).await;
    let mut refundable_input = legacy_expense();
    refundable_input.amount = "143.29".parse().unwrap();
    let refundable = seed_legacy_transaction(
        &old,
        refundable_input.clone(),
        "txn_refundable",
        Some("refundable-create"),
    )
    .await;
    let mut before_edit = serde_json::to_value(&refundable.transaction).unwrap();
    before_edit.as_object_mut().unwrap().remove("occurred_at");
    let mut after_edit = before_edit.clone();
    after_edit["amount"] = json!("135.79");
    sqlx::query("UPDATE transactions SET amount_minor = 13579 WHERE id = 'txn_refundable'")
        .execute(&old.pool)
        .await
        .unwrap();
    legacy_audit(&old, "update", Some(&before_edit), &after_edit).await;
    legacy_request(
        &old,
        "refundable-edit",
        json!({
            "operation": "update", "id": "txn_refundable",
            "patch": {"amount":"135.79", "date":null, "category":null, "note":null, "channel":null}
        }),
        json!({"transaction":after_edit, "replayed":false, "affected_ids":["txn_refundable"]}),
    )
    .await;
    let refund_input = NewTransaction {
        kind: Kind::Refund,
        amount: "150.00".parse().unwrap(),
        date: "2026-10-04".into(),
        occurred_at: None,
        category: None,
        note: Some("超额返还".into()),
        channel: Some("信用卡".into()),
        original_id: Some(refundable.transaction.id.clone()),
    };
    let refund = seed_legacy_transaction(
        &old,
        refund_input.clone(),
        "txn_refund",
        Some("refund-create"),
    )
    .await;
    let mut income_input = legacy_expense();
    income_input.kind = Kind::Income;
    income_input.category = Some("工资".into());
    income_input.amount = "42.10".parse().unwrap();
    let income = seed_legacy_transaction(
        &old,
        income_input.clone(),
        "txn_income",
        Some("income-create"),
    )
    .await;
    let history = legacy_history(&old, &refundable.transaction.id).await;
    let snapshot = legacy_snapshot(&old).await;
    assert!(!snapshot.to_string().contains("occurred_at"));
    for query in [
        "SELECT COUNT(*) FROM pragma_table_info('transactions') WHERE name = 'occurred_at'",
        "SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('pending_expenses', 'pending_audit_log')",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&old.pool)
                .await
                .unwrap(),
            0
        );
    }
    let original_checksum: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
            .fetch_one(&old.pool)
            .await
            .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&old.pool)
            .await
            .unwrap(),
        1
    );
    old.pool.close().await;

    let upgraded = Store::open(&path, false).await.unwrap();
    let retained_checksum: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap();
    assert_eq!(retained_checksum, original_checksum);
    assert_eq!(legacy_snapshot(&upgraded).await, snapshot);
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
    assert!(detail.transaction.occurred_at.is_none());
    assert!(detail.foreign_expense.is_none());
    assert_eq!(
        serde_json::to_value(upgraded.history(&refundable.transaction.id).await.unwrap()).unwrap(),
        history
    );
    let refunded = upgraded.get(&refundable.transaction.id).await.unwrap();
    assert_eq!(refunded.transaction.amount.to_string(), "135.79");
    assert_eq!(refunded.refund_total, "150.00");
    assert_eq!(refunded.net_expense.as_deref(), Some("-14.21"));
    assert_eq!(
        upgraded
            .get(&refund.transaction.id)
            .await
            .unwrap()
            .transaction
            .category,
        "购物"
    );
    assert_eq!(
        upgraded
            .get(&income.transaction.id)
            .await
            .unwrap()
            .transaction
            .kind,
        Kind::Income
    );
    let replay = upgraded
        .add(legacy_expense(), Some("legacy-create"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.transaction.id, expense.transaction.id);
    assert!(replay.transaction.occurred_at.is_none());
    for (input, request, id) in [
        (
            refundable_input,
            "refundable-create",
            &refundable.transaction.id,
        ),
        (refund_input, "refund-create", &refund.transaction.id),
        (income_input, "income-create", &income.transaction.id),
    ] {
        let replay = upgraded.add(input, Some(request)).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(&replay.transaction.id, id);
        assert!(replay.transaction.occurred_at.is_none());
    }
    let edited = upgraded
        .update(
            &refundable.transaction.id,
            UpdateTransaction {
                amount: Some("135.79".parse().unwrap()),
                ..Default::default()
            },
            Some("refundable-edit"),
        )
        .await
        .unwrap();
    assert!(edited.replayed);
    assert!(edited.transaction.occurred_at.is_none());
    assert_eq!(legacy_snapshot(&upgraded).await, snapshot);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM transactions")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM transactions WHERE occurred_at IS NOT NULL"
        )
        .fetch_one(&upgraded.pool)
        .await
        .unwrap(),
        0
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
async fn upgraded_v0_1_0_supports_twd_and_times_with_indexes_triggers_and_foreign_keys() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgraded.sqlite");
    legacy_store(&path).await.pool.close().await;
    let upgraded = Store::open(&path, false).await.unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&upgraded.pool)
            .await
            .unwrap();
    assert_eq!(versions, [1, 2]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        1
    );
    let objects: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, type FROM sqlite_schema WHERE type IN ('index', 'trigger') AND name LIKE 'pending_%' ORDER BY name",
    ).fetch_all(&upgraded.pool).await.unwrap();
    assert_eq!(
        objects,
        [
            ("pending_audit_log_pending", "index"),
            ("pending_category_insert", "trigger"),
            ("pending_category_update", "trigger"),
            ("pending_closed_immutable", "trigger"),
            ("pending_expenses_date", "index"),
            ("pending_expenses_due", "index"),
            ("pending_linked_transaction_kind", "trigger"),
            ("pending_transaction_insert", "trigger"),
            ("pending_transaction_update", "trigger"),
        ]
        .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT \"table\" FROM pragma_foreign_key_list('pending_audit_log')",
        )
        .fetch_all(&upgraded.pool)
        .await
        .unwrap(),
        ["pending_expenses"]
    );
    let mut cny_input = legacy_expense();
    cny_input.amount = "98.01".parse().unwrap();
    cny_input.occurred_at = Some("2026-09-22T12:35+08:00".into());
    let cny = upgraded.add(cny_input, Some("timed-cny")).await.unwrap();
    assert_eq!(
        cny.transaction.occurred_at.as_deref(),
        Some("2026-09-22T12:35+08:00")
    );
    assert!(
        sqlx::query("UPDATE transactions SET date = '2026-09-23' WHERE id = ?")
            .bind(&cny.transaction.id)
            .execute(&upgraded.pool)
            .await
            .is_err()
    );

    let mut input = pending_expense("TWD", "123.45");
    input.occurred_at = Some("2026-09-28T23:30:05+08:00".into());
    let pending = upgraded
        .add_pending(input, Some("timed-twd"))
        .await
        .unwrap();
    for query in [
        "UPDATE pending_expenses SET currency = 'INVALID' WHERE id = ?",
        "UPDATE pending_expenses SET category = '工资' WHERE id = ?",
        "UPDATE pending_expenses SET occurred_at = '2026-09-29T23:30+08:00' WHERE id = ?",
        "UPDATE pending_expenses SET status = 'confirmed', transaction_id = 'missing', confirmed_amount_minor = 2712, confirmed_at = '2026-10-04T12:00:00Z' WHERE id = ?",
    ] {
        assert!(
            sqlx::query(query)
                .bind(&pending.pending.id)
                .execute(&upgraded.pool)
                .await
                .is_err(),
            "{query}"
        );
    }
    assert!(sqlx::query("INSERT INTO pending_audit_log (pending_id, action, after_json, created_at) VALUES ('missing', 'create', '{}', '2026-10-04T12:00:00Z')")
        .execute(&upgraded.pool).await.is_err());
    let confirmed = upgraded
        .confirm_pending(
            &pending.pending.id,
            ConfirmPendingExpense {
                amount: "27.12".parse().unwrap(),
                posted_date: Some("2026-10-03".into()),
            },
            Some("twd-confirm"),
        )
        .await
        .unwrap();
    assert_eq!(confirmed.pending.currency, "TWD");
    assert_eq!(confirmed.pending.amount, "123.45");
    let transaction = confirmed.transaction.unwrap();
    assert_eq!(transaction.amount.to_string(), "27.12");
    assert_eq!(transaction.occurred_at, confirmed.pending.occurred_at);
    assert_eq!(transaction.date, "2026-09-28");
    assert!(
        sqlx::query("UPDATE pending_expenses SET note = 'forbidden' WHERE id = ?")
            .bind(&pending.pending.id)
            .execute(&upgraded.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE transactions SET kind = 'income' WHERE id = ?")
            .bind(&transaction.id)
            .execute(&upgraded.pool)
            .await
            .is_err()
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

#[tokio::test]
async fn upgraded_v0_1_0_rejects_fractional_jpy_and_krw_storage_without_mutating_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("whole-currency.sqlite");
    let old = legacy_store(&path).await;
    seed_legacy_transaction(&old, legacy_expense(), "txn_legacy", Some("legacy-create")).await;
    let legacy = legacy_snapshot(&old).await;
    old.pool.close().await;
    let upgraded = Store::open(&path, false).await.unwrap();
    assert_eq!(legacy_snapshot(&upgraded).await, legacy);
    let usd = upgraded
        .add_pending(pending_expense("USD", "1.01"), Some("usd-create"))
        .await
        .unwrap();
    let mut whole_currency_ids = Vec::new();
    for currency in ["JPY", "KRW"] {
        let result = upgraded
            .add_pending(
                pending_expense(currency, "1000"),
                Some(&format!("{currency}-create")),
            )
            .await
            .unwrap();
        assert_eq!(result.pending.amount, "1000");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT amount_minor FROM pending_expenses WHERE id = ?")
                .bind(&result.pending.id)
                .fetch_one(&upgraded.pool)
                .await
                .unwrap(),
            100000
        );
        whole_currency_ids.push((currency, result.pending.id));
    }
    let before_path = directory.path().join("before-rejected-writes.json");
    archive::export(&upgraded, &before_path).await.unwrap();
    let before: Value = serde_json::from_slice(&fs::read(&before_path).unwrap()).unwrap();
    for (currency, id) in &whole_currency_ids {
        for invalid_amount in [1_i64, 101, 100001, i64::MAX] {
            let insert = sqlx::query("INSERT INTO pending_expenses (id, currency, amount_minor, date, category, remind_on, created_at, updated_at) VALUES ('invalid-whole-currency', ?, ?, '2026-09-28', '购物', '2026-10-01', '2026-09-28T12:00:00Z', '2026-09-28T12:00:00Z')")
                .bind(currency).bind(invalid_amount).execute(&upgraded.pool).await.unwrap_err();
            assert!(
                insert.to_string().contains("CHECK constraint failed"),
                "{currency} INSERT {invalid_amount}: {insert}"
            );
            let update = sqlx::query("UPDATE pending_expenses SET amount_minor = ? WHERE id = ?")
                .bind(invalid_amount)
                .bind(id)
                .execute(&upgraded.pool)
                .await
                .unwrap_err();
            assert!(
                update.to_string().contains("CHECK constraint failed"),
                "{currency} UPDATE {invalid_amount}: {update}"
            );
        }
        // A currency-only edit must not reinterpret an existing fractional USD
        // amount as whole JPY/KRW, even when the numeric column is unchanged.
        let switch = sqlx::query("UPDATE pending_expenses SET currency = ? WHERE id = ?")
            .bind(currency)
            .bind(&usd.pending.id)
            .execute(&upgraded.pool)
            .await
            .unwrap_err();
        assert!(
            switch.to_string().contains("CHECK constraint failed"),
            "USD -> {currency}: {switch}"
        );
        let simultaneous = sqlx::query(
            "UPDATE pending_expenses SET currency = ?, amount_minor = 199 WHERE id = ?",
        )
        .bind(currency)
        .bind(&usd.pending.id)
        .execute(&upgraded.pool)
        .await
        .unwrap_err();
        assert!(
            simultaneous.to_string().contains("CHECK constraint failed"),
            "USD -> {currency} with amount: {simultaneous}"
        );
    }
    let after_path = directory.path().join("after-rejected-writes.json");
    archive::export(&upgraded, &after_path).await.unwrap();
    let after: Value = serde_json::from_slice(&fs::read(&after_path).unwrap()).unwrap();
    // Covers every row, historical audit JSON and idempotency response, not only
    // the monetary column targeted by the rejected SQL statements.
    assert_eq!(after["tables"], before["tables"]);
    assert_eq!(
        legacy_snapshot(&upgraded).await["transactions"],
        legacy["transactions"]
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

#[tokio::test]
async fn new_cli_restores_a_v0_1_0_backup_without_modifying_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy-backup.sqlite");
    let old = legacy_store(&source).await;
    let expense =
        seed_legacy_transaction(&old, legacy_expense(), "txn_legacy", Some("legacy-create")).await;
    let history = legacy_history(&old, &expense.transaction.id).await;
    old.pool.close().await;
    let source_bytes = fs::read(&source).unwrap();

    let destination = directory.path().join("restored.sqlite");
    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
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
        occurred_at: None,
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
    let jpy_input = pending_expense("JPY", "92233720368547758");
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
    let krw_input = pending_expense("KRW", "1000");
    let krw = store
        .add_pending(krw_input.clone(), Some("krw-create"))
        .await
        .unwrap();
    assert_eq!(jpy.pending.amount, "92233720368547758");
    assert_eq!(krw.pending.amount, "1000");
    let raw_amounts: Vec<(String, i64)> =
        sqlx::query_as("SELECT currency, amount_minor FROM pending_expenses ORDER BY currency")
            .fetch_all(&store.pool)
            .await
            .unwrap();
    assert_eq!(
        raw_amounts,
        [
            ("JPY".into(), 9223372036854775800),
            ("KRW".into(), 100000),
            ("USD".into(), 2000),
        ]
    );
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
        exported["amount_units"]["pending_expenses.amount_minor"],
        json!({
            "currency_column": "currency", "unit": "currency_hundredth", "exponent": 2
        })
    );
    let rows = exported["tables"]["pending_expenses"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    let find = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
    assert_eq!(find(&jpy.pending.id)["amount_minor"], "9223372036854775800");
    assert_eq!(find(&krw.pending.id)["amount_minor"], "100000");
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
        7
    );
    assert_eq!(exported["tables"]["audit_log"].as_array().unwrap().len(), 1);
    assert_eq!(
        exported["tables"]["idempotency"].as_array().unwrap().len(),
        7
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
    assert_eq!(repeated_jpy.pending.amount, "92233720368547758");
    let repeated_krw = restored
        .add_pending(krw_input, Some("krw-create"))
        .await
        .unwrap();
    assert!(repeated_krw.replayed);
    assert_eq!(repeated_krw.pending.id, krw.pending.id);
    assert_eq!(repeated_krw.pending.amount, "1000");
    for (id, amount) in [
        (&jpy.pending.id, "92233720368547758"),
        (&krw.pending.id, "1000"),
    ] {
        assert_eq!(
            restored.get_pending(id).await.unwrap().pending.amount,
            amount
        );
    }
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
        4
    );
    restored.pool.close().await;
    store.pool.close().await;
}

#[tokio::test]
async fn occurrence_offsets_and_local_dates_survive_confirmation_export_backup_and_restore() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("timed.sqlite"), true)
        .await
        .unwrap();
    let cny_time = "2026-09-30T00:05:06+08:00";
    let foreign_time = "2026-09-30T23:30-04:00";
    let mut input = legacy_expense();
    input.amount = "98.01".parse().unwrap();
    input.date = "2026-09-30".into();
    input.occurred_at = Some(cny_time.into());
    let cny = store
        .add(input.clone(), Some("timed-cny-create"))
        .await
        .unwrap();
    let mut foreign = pending_expense("TWD", "123.45");
    foreign.date = "2026-09-30".into();
    foreign.occurred_at = Some(foreign_time.into());
    let pending = store
        .add_pending(foreign.clone(), Some("timed-pending-create"))
        .await
        .unwrap();
    let confirmation = ConfirmPendingExpense {
        amount: "27.12".parse().unwrap(),
        posted_date: Some("2026-10-03".into()),
    };
    let confirmed = store
        .confirm_pending(
            &pending.pending.id,
            confirmation.clone(),
            Some("timed-confirm"),
        )
        .await
        .unwrap();
    assert_eq!(confirmed.pending.occurred_at.as_deref(), Some(foreign_time));
    let confirmed_tx = confirmed.transaction.unwrap();
    assert_eq!(confirmed_tx.occurred_at.as_deref(), Some(foreign_time));
    assert_eq!(confirmed_tx.date, "2026-09-30");

    let export_path = directory.path().join("timed-export.json");
    archive::export(&store, &export_path).await.unwrap();
    let exported: Value = serde_json::from_slice(&fs::read(&export_path).unwrap()).unwrap();
    let transactions = exported["tables"]["transactions"].as_array().unwrap();
    assert_eq!(
        transactions
            .iter()
            .find(|row| row["id"] == cny.transaction.id)
            .unwrap()["occurred_at"],
        cny_time
    );
    assert_eq!(
        transactions
            .iter()
            .find(|row| row["id"] == confirmed_tx.id)
            .unwrap()["occurred_at"],
        foreign_time
    );
    assert_eq!(
        exported["tables"]["pending_expenses"][0]["occurred_at"],
        foreign_time
    );
    let backup_path = directory.path().join("timed-backup.sqlite");
    archive::backup(&store, &backup_path).await.unwrap();
    let restored_path = directory.path().join("timed-restored.sqlite");
    archive::restore(&backup_path, &restored_path)
        .await
        .unwrap();
    let restored = Store::open(&restored_path, false).await.unwrap();
    let restored_export_path = directory.path().join("timed-restored.json");
    archive::export(&restored, &restored_export_path)
        .await
        .unwrap();
    let restored_export: Value =
        serde_json::from_slice(&fs::read(&restored_export_path).unwrap()).unwrap();
    assert_eq!(restored_export["tables"], exported["tables"]);
    let repeated_cny = restored.add(input, Some("timed-cny-create")).await.unwrap();
    assert!(repeated_cny.replayed);
    assert_eq!(
        repeated_cny.transaction.occurred_at.as_deref(),
        Some(cny_time)
    );
    let repeated_pending = restored
        .add_pending(foreign, Some("timed-pending-create"))
        .await
        .unwrap();
    assert!(repeated_pending.replayed);
    assert_eq!(
        repeated_pending.pending.occurred_at.as_deref(),
        Some(foreign_time)
    );
    let repeated_confirm = restored
        .confirm_pending(&pending.pending.id, confirmation, Some("timed-confirm"))
        .await
        .unwrap();
    assert!(repeated_confirm.replayed);
    assert_eq!(
        repeated_confirm.transaction.unwrap().occurred_at.as_deref(),
        Some(foreign_time)
    );
    let summary = restored
        .summary(&claw_expense::models::Filters {
            month: Some("2026-09".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(summary.expense, "125.13");
    assert_eq!(summary.pending_count, 0);
    restored.pool.close().await;
    store.pool.close().await;
}
