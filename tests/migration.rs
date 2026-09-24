use std::{fs, path::Path, process::Command};

use claw_expense::{
    archive,
    models::{ConfirmPendingExpense, Kind, NewPendingExpense, NewTransaction, WriteResult},
    store::Store,
};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool, migrate::Migrator, sqlite::SqliteConnectOptions};

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

async fn legacy_audit(
    store: &Store,
    pending: bool,
    action: &str,
    before: Option<&Value>,
    after: &Value,
) {
    let sql = if pending {
        "INSERT INTO pending_audit_log (pending_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)"
    } else {
        "INSERT INTO audit_log (transaction_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)"
    };
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

async fn legacy_history(store: &Store, id: &str, pending: bool) -> Value {
    let (sql, key) = if pending {
        (
            "SELECT * FROM pending_audit_log WHERE pending_id = ? ORDER BY id",
            "pending_id",
        )
    } else {
        (
            "SELECT * FROM audit_log WHERE transaction_id = ? ORDER BY id",
            "transaction_id",
        )
    };
    Value::Array(sqlx::query(sql).bind(id).fetch_all(&store.pool).await.unwrap().iter().map(|row| {
        let mut item = json!({
            "id": row.get::<i64, _>("id"), "action": row.get::<String, _>("action"),
            "before": row.get::<Option<String>, _>("before_json").map(|text| serde_json::from_str::<Value>(&text).unwrap()),
            "after": serde_json::from_str::<Value>(&row.get::<String, _>("after_json")).unwrap(),
            "created_at": row.get::<String, _>("created_at")
        });
        item[key] = json!(id);
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
    legacy_audit(store, false, "create", None, &record).await;
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

async fn seed_legacy_pending(
    store: &Store,
    input: NewPendingExpense,
    id: &str,
    request: &str,
) -> Value {
    let record = json!({"id": id, "currency": input.currency, "amount": input.amount, "date": input.date,
        "category": input.category.as_deref().unwrap_or("其他支出"), "merchant": input.merchant,
        "note": input.note, "channel": input.channel, "status": "pending", "remind_on": "2026-10-01",
        "transaction_id": null, "confirmed_at": null, "posted_date": null,
        "created_at": LEGACY_TIMESTAMP, "updated_at": LEGACY_TIMESTAMP});
    let minor = claw_expense::foreign::parse_foreign_minor(&input.currency, &input.amount).unwrap();
    sqlx::query("INSERT INTO pending_expenses (id, currency, amount_minor, date, category, merchant, note, channel, remind_on, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(&input.currency).bind(minor).bind(&input.date)
        .bind(input.category.as_deref().unwrap_or("其他支出")).bind(&input.merchant).bind(&input.note).bind(&input.channel)
        .bind("2026-10-01").bind(LEGACY_TIMESTAMP).bind(LEGACY_TIMESTAMP).execute(&store.pool).await.unwrap();
    legacy_audit(store, true, "create", None, &record).await;
    let old_input = json!({"currency": input.currency, "amount": input.amount, "date": input.date,
        "category": input.category, "merchant": input.merchant, "note": input.note, "channel": input.channel});
    legacy_request(
        store,
        request,
        json!({"operation": "pending.add", "input": old_input}),
        json!({"pending": record, "transaction": null, "replayed": false}),
    )
    .await;
    record
}

struct LegacyForeign {
    jpy_id: String,
    usd_id: String,
    eur_id: String,
    cny_id: String,
    refund: WriteResult,
    refund_input: NewTransaction,
    confirmation: ConfirmPendingExpense,
}

async fn seed_legacy_foreign(store: &Store, prefix: &str) -> LegacyForeign {
    let jpy_id = format!("pending_{prefix}_jpy");
    let usd_id = format!("pending_{prefix}_usd");
    let eur_id = format!("pending_{prefix}_eur");
    let cny_id = format!("txn_{prefix}_cny");
    let jpy = seed_legacy_pending(
        store,
        pending_expense("JPY", "10000"),
        &jpy_id,
        &format!("{prefix}-jpy-create"),
    )
    .await;
    let mut snoozed = jpy.clone();
    snoozed["remind_on"] = json!("2026-10-06");
    sqlx::query("UPDATE pending_expenses SET remind_on = '2026-10-06' WHERE id = ?")
        .bind(&jpy_id)
        .execute(&store.pool)
        .await
        .unwrap();
    legacy_audit(store, true, "snooze", Some(&jpy), &snoozed).await;
    legacy_request(
        store,
        &format!("{prefix}-jpy-snooze"),
        json!({"operation":"pending.snooze","id":jpy_id,"until":"2026-10-06"}),
        json!({"pending":snoozed,"transaction":null,"replayed":false}),
    )
    .await;

    let usd = seed_legacy_pending(
        store,
        pending_expense("USD", "20.00"),
        &usd_id,
        &format!("{prefix}-usd-create"),
    )
    .await;
    let mut cny_input = legacy_expense();
    cny_input.amount = "143.29".parse().unwrap();
    cny_input.date = "2026-09-28".into();
    cny_input.note = Some("原币金额等待银行换算".into());
    let confirmed = seed_legacy_transaction(store, cny_input, &cny_id, None).await;
    let mut cny_record = serde_json::to_value(&confirmed.transaction).unwrap();
    cny_record.as_object_mut().unwrap().remove("occurred_at");
    let confirmation = ConfirmPendingExpense {
        amount: "143.29".parse().unwrap(),
        posted_date: Some("2026-10-03".into()),
    };
    let mut confirmed_pending = usd.clone();
    confirmed_pending["status"] = json!("confirmed");
    confirmed_pending["transaction_id"] = json!(cny_id);
    confirmed_pending["confirmed_at"] = json!(LEGACY_TIMESTAMP);
    confirmed_pending["posted_date"] = json!("2026-10-03");
    sqlx::query("UPDATE pending_expenses SET status = 'confirmed', transaction_id = ?, confirmed_amount_minor = 14329, confirmed_at = ?, posted_date = '2026-10-03' WHERE id = ?")
        .bind(&cny_id).bind(LEGACY_TIMESTAMP).bind(&usd_id).execute(&store.pool).await.unwrap();
    legacy_audit(store, true, "confirm", Some(&usd), &confirmed_pending).await;
    legacy_request(
        store,
        &format!("{prefix}-usd-confirm"),
        json!({"operation":"pending.confirm","id":usd_id,"input":confirmation}),
        json!({"pending":confirmed_pending,"transaction":cny_record,"replayed":false}),
    )
    .await;
    let mut edited = cny_record.clone();
    edited["amount"] = json!("135.79");
    sqlx::query("UPDATE transactions SET amount_minor = 13579 WHERE id = ?")
        .bind(&cny_id)
        .execute(&store.pool)
        .await
        .unwrap();
    legacy_audit(store, false, "update", Some(&cny_record), &edited).await;
    legacy_request(store, &format!("{prefix}-cny-edit"), json!({"operation":"update","id":cny_id,"patch":{"amount":"135.79","date":null,"category":null,"note":null,"channel":null}}),
        json!({"transaction":edited,"replayed":false,"affected_ids":[cny_id]})).await;
    let refund_input = NewTransaction {
        kind: Kind::Refund,
        amount: "150.00".parse().unwrap(),
        date: "2026-10-04".into(),
        occurred_at: None,
        category: None,
        note: Some("超额返还".into()),
        channel: Some("信用卡".into()),
        original_id: Some(cny_id.clone()),
    };
    let refund = seed_legacy_transaction(
        store,
        refund_input.clone(),
        &format!("txn_{prefix}_refund"),
        Some(&format!("{prefix}-refund")),
    )
    .await;

    let eur = seed_legacy_pending(
        store,
        pending_expense("EUR", "1.29"),
        &eur_id,
        &format!("{prefix}-eur-create"),
    )
    .await;
    let mut cancelled = eur.clone();
    cancelled["status"] = json!("cancelled");
    sqlx::query("UPDATE pending_expenses SET status = 'cancelled' WHERE id = ?")
        .bind(&eur_id)
        .execute(&store.pool)
        .await
        .unwrap();
    legacy_audit(store, true, "cancel", Some(&eur), &cancelled).await;
    legacy_request(
        store,
        &format!("{prefix}-eur-cancel"),
        json!({"operation":"pending.cancel","id":eur_id,"until":null}),
        json!({"pending":cancelled,"transaction":null,"replayed":false}),
    )
    .await;
    LegacyForeign {
        jpy_id,
        usd_id,
        eur_id,
        cny_id,
        refund,
        refund_input,
        confirmation,
    }
}

fn strip_null_occurrences(tables: &mut Value) {
    for name in ["transactions", "pending_expenses"] {
        for record in tables[name].as_array_mut().unwrap() {
            assert_eq!(record.get("occurred_at"), Some(&Value::Null));
            record.as_object_mut().unwrap().remove("occurred_at");
        }
    }
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
    let history = legacy_history(&old, &expense.transaction.id, false).await;
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
    let expense =
        seed_legacy_transaction(&old, legacy_expense(), "txn_legacy", Some("legacy-create")).await;
    let history = legacy_history(&old, &expense.transaction.id, false).await;
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
    let fixture = seed_legacy_foreign(&old, "v2").await;

    // Prove this really is the old CHECK constraint, not a current schema
    // disguised by removing its latest migration row.
    assert!(
        sqlx::query("INSERT INTO pending_expenses (id, currency, amount_minor, date, category, remind_on, created_at, updated_at) SELECT 'unsupported-twd', 'TWD', 100, date, category, remind_on, created_at, updated_at FROM pending_expenses WHERE id = ?")
            .bind(&fixture.jpy_id)
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
    let old_pending_history = legacy_history(&old, &fixture.usd_id, true).await;
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
        sqlx::migrate!().iter().count() as i64
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
    let mut after: Value = serde_json::from_slice(&fs::read(&after_path).unwrap()).unwrap();
    strip_null_occurrences(&mut after["tables"]);
    assert_eq!(after["tables"], before["tables"]);
    let current = upgraded.get(&fixture.cny_id).await.unwrap();
    assert_eq!(current.transaction.amount.to_string(), "135.79");
    assert_eq!(current.refund_total, "150.00");
    assert_eq!(current.net_expense.as_deref(), Some("-14.21"));
    assert_eq!(current.foreign_expense.unwrap().id, fixture.usd_id);
    assert_eq!(
        serde_json::to_value(upgraded.pending_history(&fixture.usd_id).await.unwrap()).unwrap(),
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
            .snooze_pending(&fixture.jpy_id, "2026-10-06", Some("v2-jpy-snooze"))
            .await
            .unwrap()
            .replayed
    );
    let repeated_confirmation = upgraded
        .confirm_pending(
            &fixture.usd_id,
            fixture.confirmation,
            Some("v2-usd-confirm"),
        )
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
    let repeated_refund = upgraded
        .add(fixture.refund_input, Some("v2-refund"))
        .await
        .unwrap();
    assert!(repeated_refund.replayed);
    assert_eq!(
        repeated_refund.transaction.id,
        fixture.refund.transaction.id
    );
    assert!(
        upgraded
            .cancel_pending(&fixture.eur_id, Some("v2-eur-cancel"))
            .await
            .unwrap()
            .replayed
    );
    // The reinstalled closed-state trigger must still protect old snapshots.
    assert!(
        sqlx::query("UPDATE pending_expenses SET note = 'forbidden' WHERE id = ?")
            .bind(&fixture.usd_id)
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

#[tokio::test]
async fn v3_date_only_upgrade_preserves_history_and_replays_without_inventing_occurrence_times() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("date-only-v3.sqlite");
    let old = store_at_version(&path, 3).await;
    let expense =
        seed_legacy_transaction(&old, legacy_expense(), "txn_legacy_v3", Some("v3-create")).await;
    let fixture = seed_legacy_foreign(&old, "v3").await;
    for query in [
        "SELECT COUNT(*) FROM pragma_table_info('transactions') WHERE name = 'occurred_at'",
        "SELECT COUNT(*) FROM pragma_table_info('pending_expenses') WHERE name = 'occurred_at'",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&old.pool)
                .await
                .unwrap(),
            0
        );
    }
    let checksums: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&old.pool)
            .await
            .unwrap();
    assert_eq!(checksums.len(), 3);
    let before_path = directory.path().join("v3-before.json");
    archive::export(&old, &before_path).await.unwrap();
    let before: Value = serde_json::from_slice(&fs::read(&before_path).unwrap()).unwrap();
    assert!(!before["tables"].to_string().contains("occurred_at"));
    let cny_history = legacy_history(&old, &fixture.cny_id, false).await;
    let pending_history = legacy_history(&old, &fixture.usd_id, true).await;
    old.pool.close().await;

    let upgraded = Store::open(&path, false).await.unwrap();
    let retained_checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version <= 3 ORDER BY version",
    )
    .fetch_all(&upgraded.pool)
    .await
    .unwrap();
    assert_eq!(retained_checksums, checksums);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap(),
        4
    );
    let after_path = directory.path().join("v3-after.json");
    archive::export(&upgraded, &after_path).await.unwrap();
    let mut after: Value = serde_json::from_slice(&fs::read(&after_path).unwrap()).unwrap();
    strip_null_occurrences(&mut after["tables"]);
    assert_eq!(after["tables"], before["tables"]);
    assert_eq!(
        serde_json::to_value(upgraded.history(&fixture.cny_id).await.unwrap()).unwrap(),
        cny_history
    );
    assert_eq!(
        serde_json::to_value(upgraded.pending_history(&fixture.usd_id).await.unwrap()).unwrap(),
        pending_history
    );

    let repeated_cny = upgraded
        .add(legacy_expense(), Some("v3-create"))
        .await
        .unwrap();
    assert!(repeated_cny.replayed);
    assert_eq!(repeated_cny.transaction.id, expense.transaction.id);
    assert!(repeated_cny.transaction.occurred_at.is_none());
    let repeated_pending = upgraded
        .add_pending(pending_expense("JPY", "10000"), Some("v3-jpy-create"))
        .await
        .unwrap();
    assert!(repeated_pending.replayed);
    assert!(repeated_pending.pending.occurred_at.is_none());
    let repeated_confirm = upgraded
        .confirm_pending(
            &fixture.usd_id,
            fixture.confirmation,
            Some("v3-usd-confirm"),
        )
        .await
        .unwrap();
    assert!(repeated_confirm.replayed);
    assert!(repeated_confirm.pending.occurred_at.is_none());
    assert!(repeated_confirm.transaction.unwrap().occurred_at.is_none());
    let repeated_edit = upgraded
        .update(
            &fixture.cny_id,
            claw_expense::models::UpdateTransaction {
                amount: Some("135.79".parse().unwrap()),
                ..Default::default()
            },
            Some("v3-cny-edit"),
        )
        .await
        .unwrap();
    assert!(repeated_edit.replayed);
    assert!(repeated_edit.transaction.occurred_at.is_none());
    let cancelled = upgraded.get_pending(&fixture.eur_id).await.unwrap();
    assert_eq!(cancelled.pending.status, "cancelled");
    assert!(cancelled.pending.occurred_at.is_none());
    assert_eq!(
        upgraded
            .get_pending(&fixture.jpy_id)
            .await
            .unwrap()
            .pending
            .remind_on,
        "2026-10-06"
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
