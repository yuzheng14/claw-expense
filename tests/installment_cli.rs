use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};
use tempfile::TempDir;

struct Ledger {
    directory: TempDir,
    db: PathBuf,
}
impl Ledger {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("ledger.sqlite");
        let ledger = Self { directory, db };
        ledger.ok(&["init"]);
        ledger
    }
    fn ok(&self, args: &[&str]) -> Value {
        success(run(&self.db, args, None))
    }
    fn error(&self, args: &[&str]) -> Value {
        failure(run(&self.db, args, None))
    }
    fn add(&self) -> Value {
        self.ok(&[
            "installment",
            "add",
            "--amount",
            "1000.01",
            "--fee",
            "1.01",
            "--periods",
            "3",
            "--date",
            "2026-01-20",
            "--first-due-date",
            "2026-01-31",
            "--category",
            "购物",
            "--merchant",
            "分期商户",
            "--channel",
            "信用卡",
            "--note",
            "手机",
            "--request-id",
            "plan-1",
        ])
    }
}
fn spawn(db: &Path, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn run(db: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut child = spawn(db, args);
    if let Some(mut stdin) = child.stdin.take()
        && let Some(input) = input
    {
        stdin.write_all(input.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    value["data"].clone()
}
fn failure(output: Output) -> Value {
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    value["error"].clone()
}
fn id(value: &Value) -> &str {
    value["plan"]["id"].as_str().unwrap()
}
fn txn(value: &Value) -> &str {
    value["transaction"]["id"].as_str().unwrap()
}
fn confirm(ledger: &Ledger, plan: &str, period: &str, date: &str) -> Value {
    ledger.ok(&[
        "installment",
        "confirm",
        plan,
        "--period",
        period,
        "--date",
        date,
    ])
}

#[test]
fn plans_are_exact_and_do_not_count_as_expenses_until_confirmed() {
    let ledger = Ledger::new();
    let added = ledger.add();
    assert_eq!(added["plan"]["total"], "1001.02");
    assert_eq!(added["plan"]["status"], "active");
    assert_eq!(added["remaining_amount"], "1001.02");
    assert_eq!(added["schedule"][0]["principal"], "333.33");
    assert_eq!(added["schedule"][0]["fee"], "0.33");
    assert_eq!(added["schedule"][2]["principal"], "333.35");
    assert_eq!(added["schedule"][2]["fee"], "0.35");
    assert_eq!(added["schedule"][1]["due_date"], "2026-02-28");
    assert_eq!(added["schedule"][2]["due_date"], "2026-03-31");
    assert_eq!(ledger.ok(&["summary"])["expense"], "0.00");
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    let due = ledger.ok(&["installment", "due", "--as-of", "2026-02-28"]);
    assert_eq!(due["total"], 2);
    assert_eq!(due["items"][1]["plan_id"], id(&added));
    assert_eq!(due["items"][1]["merchant"], "分期商户");
    let page = ledger.ok(&[
        "installment",
        "due",
        "--as-of",
        "2026-02-28",
        "--limit",
        "1",
        "--offset",
        "1",
    ]);
    assert_eq!(page["total"], 2);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["period"], 2);
    assert_eq!(
        ledger.ok(&[
            "installment",
            "due",
            "--as-of",
            "2026-02-28",
            "--month",
            "2026-02"
        ])["total"],
        1
    );
    assert_eq!(
        ledger.ok(&["installment", "list", "--month", "2026-02"])["total"],
        0
    );
    assert_eq!(
        ledger.ok(&[
            "installment",
            "list",
            "--search",
            "分期商户",
            "--category",
            "购物"
        ])["total"],
        1
    );
    assert_eq!(
        ledger.ok(&["installment", "list", "--search", "' OR 1=1 --"])["total"],
        0
    );
    assert_eq!(
        ledger.ok(&["installment", "show", id(&added)])["schedule"],
        added["schedule"]
    );
}

#[test]
fn confirmation_uses_actual_date_and_preserves_refunds_search_and_corrections() {
    let ledger = Ledger::new();
    let added = ledger.add();
    let args = [
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--occurred-at",
        "2026-02-01T00:05+08:00",
        "--request-id",
        "confirm-1",
    ];
    let confirmed = ledger.ok(&args);
    assert_eq!(confirmed["transaction"]["amount"], "333.66");
    assert_eq!(confirmed["transaction"]["date"], "2026-02-01");
    assert_eq!(
        confirmed["transaction"]["occurred_at"],
        "2026-02-01T00:05+08:00"
    );
    assert_eq!(confirmed["remaining_amount"], "667.36");
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-01"])["expense"],
        "0.00"
    );
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-02"])["expense"],
        "333.66"
    );
    assert_eq!(
        ledger.ok(&["show", txn(&confirmed)])["installment"]["plan_id"],
        id(&added)
    );
    assert_eq!(ledger.ok(&args)["replayed"], true);
    let other_time = ledger.error(&[
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--date",
        "2026-02-02",
    ]);
    assert_eq!(other_time["code"], "ALREADY_CONFIRMED");
    let refund = ledger.ok(&[
        "refund",
        txn(&confirmed),
        "--amount",
        "400",
        "--date",
        "2026-02-03",
    ]);
    assert_eq!(
        ledger.ok(&["summary", "--search", "分期商户"])["net_expense"],
        "-66.34"
    );
    assert_eq!(ledger.ok(&["list", "--search", id(&added)])["total"], 2);
    ledger.ok(&[
        "edit",
        txn(&confirmed),
        "--amount",
        "330",
        "--category",
        "餐饮",
    ]);
    let current = ledger.ok(&["installment", "show", id(&added)]);
    assert_eq!(current["transactions"][0]["amount"], "330.00");
    assert_eq!(current["schedule"][0]["amount"], "333.66");
    assert_eq!(ledger.ok(&args)["transaction"], confirmed["transaction"]);
    ledger.ok(&["void", txn(&refund)]);
    ledger.ok(&["void", txn(&confirmed)]);
    assert_eq!(ledger.ok(&["summary"])["expense"], "0.00");
    // The existing confirmation cannot be repeated to replace a voided expense.
    let repeat = ledger.ok(&[
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--occurred-at",
        "2026-02-01T00:05+08:00",
    ]);
    assert_eq!(repeat["transaction"]["id"], txn(&confirmed));
    assert_eq!(repeat["transaction"]["voided"], true);
    assert_eq!(ledger.ok(&["list", "--include-voided"])["total"], 2);
}

#[test]
fn cancel_only_closes_remaining_periods_and_replays_original_snapshots() {
    let ledger = Ledger::new();
    let added = ledger.add();
    let confirmed = confirm(&ledger, id(&added), "1", "2026-01-31");
    let args = [
        "installment",
        "cancel",
        id(&added),
        "--request-id",
        "cancel-1",
    ];
    let cancelled = ledger.ok(&args);
    assert_eq!(cancelled["plan"]["status"], "cancelled");
    assert_eq!(cancelled["schedule"][0]["status"], "confirmed");
    assert_eq!(cancelled["schedule"][1]["status"], "cancelled");
    assert_eq!(cancelled["remaining_amount"], "0.00");
    assert_eq!(ledger.ok(&args)["replayed"], true);
    assert_eq!(
        ledger.ok(&["installment", "cancel", id(&added)])["replayed"],
        true
    );
    assert_eq!(
        ledger.error(&[
            "installment",
            "confirm",
            id(&added),
            "--period",
            "2",
            "--date",
            "2026-03-01"
        ])["code"],
        "INSTALLMENT_CLOSED"
    );
    assert_eq!(
        ledger.ok(&["installment", "due", "--as-of", "2027-01-01"])["total"],
        0
    );
    assert_eq!(ledger.ok(&["summary"])["expense"], "333.66");
    assert_eq!(
        ledger.ok(&["show", txn(&confirmed)])["transaction"]["voided"],
        false
    );
    let replay = ledger.add();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["plan"]["status"], "active");
    assert_eq!(replay["schedule"], added["schedule"]);
    let history = ledger.ok(&["installment", "history", id(&added)]);
    assert_eq!(history.as_array().unwrap().len(), 3);
    assert_eq!(history[2]["action"], "cancel");
    assert_eq!(ledger.ok(&["installment", "list"])["total"], 0);
    assert_eq!(
        ledger.ok(&["installment", "list", "--status", "cancelled"])["total"],
        1
    );
}

#[test]
fn completion_and_concurrent_confirmation_create_one_expense_per_period() {
    let ledger = Ledger::new();
    let added = ledger.add();
    let args = [
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--date",
        "2026-01-31",
        "--request-id",
        "race",
    ];
    let first = spawn(&ledger.db, &args);
    let second = spawn(&ledger.db, &args);
    let a = success(first.wait_with_output().unwrap());
    let b = success(second.wait_with_output().unwrap());
    assert_eq!(txn(&a), txn(&b));
    assert_ne!(a["replayed"], b["replayed"]);
    let third_args = [
        "installment",
        "confirm",
        id(&added),
        "--period",
        "2",
        "--date",
        "2026-02-28",
    ];
    let first = spawn(&ledger.db, &third_args);
    let second = spawn(&ledger.db, &third_args);
    assert_eq!(
        txn(&success(first.wait_with_output().unwrap())),
        txn(&success(second.wait_with_output().unwrap()))
    );
    let done = confirm(&ledger, id(&added), "3", "2026-03-31");
    assert_eq!(done["plan"]["status"], "completed");
    assert_eq!(done["confirmed_count"], 3);
    assert_eq!(done["remaining_count"], 0);
    assert_eq!(ledger.ok(&["list"])["total"], 3);
    assert_eq!(ledger.ok(&["summary"])["expense"], "1001.02");
    assert_eq!(
        ledger.ok(&["installment", "list", "--status", "completed"])["total"],
        1
    );
    assert_eq!(
        ledger.ok(&["installment", "due", "--as-of", "2027-01-01"])["total"],
        0
    );
    assert_eq!(
        ledger.error(&["installment", "cancel", id(&added)])["code"],
        "INSTALLMENT_CLOSED"
    );
    assert_eq!(
        ledger
            .ok(&["installment", "history", id(&added)])
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn json_validation_and_request_conflicts_leave_no_partial_data() {
    let ledger = Ledger::new();
    let base =
        json!({"amount":"10.00", "periods":3, "date":"2026-01-20", "first_due_date":"2026-01-31"});
    for (key, value) in [
        ("amount", json!(10)),
        ("amount", json!("0")),
        ("amount", json!("0.02")),
        ("amount", json!("1.001")),
        ("fee", json!("-1")),
        ("fee", json!(1)),
        ("fee", json!("1e2")),
        ("periods", json!(1)),
        ("periods", json!(361)),
        ("periods", json!(2.5)),
        ("first_due_date", json!("2026-01-19")),
        ("first_due_date", json!("9999-12-31")),
        ("date", json!("2026-02-30")),
        ("category", json!("工资")),
        ("category", json!("不存在")),
        ("unknown", json!(true)),
        ("occurred_at", json!("2026-01-21T12:30+08:00")),
    ] {
        let mut record = base.clone();
        record[key] = value;
        failure(run(
            &ledger.db,
            &["installment", "record", "--request-id", "bad"],
            Some(&record.to_string()),
        ));
        assert_eq!(
            ledger.ok(&["installment", "list", "--status", "all"])["total"],
            0
        );
    }
    let added = success(run(
        &ledger.db,
        &["installment", "record", "--request-id", "bad"],
        Some(&base.to_string()),
    ));
    assert_eq!(added["plan"]["fee"], "0.00");
    let mut changed = base.clone();
    changed["amount"] = json!("11");
    assert_eq!(
        failure(run(
            &ledger.db,
            &["installment", "record", "--request-id", "bad"],
            Some(&changed.to_string())
        ))["code"],
        "IDEMPOTENCY_CONFLICT"
    );
    assert_eq!(
        ledger.error(&[
            "add",
            "expense",
            "--amount",
            "10",
            "--date",
            "2026-01-20",
            "--request-id",
            "bad"
        ])["code"],
        "IDEMPOTENCY_CONFLICT"
    );
    assert_eq!(
        ledger.error(&[
            "installment",
            "confirm",
            id(&added),
            "--period",
            "1",
            "--date",
            "2026-01-19",
            "--request-id",
            "confirm-bad"
        ])["code"],
        "INVALID_INPUT"
    );
    assert_eq!(
        ledger.error(&[
            "installment",
            "confirm",
            id(&added),
            "--period",
            "4",
            "--date",
            "2026-02-01"
        ])["code"],
        "NOT_FOUND"
    );
    assert_eq!(ledger.ok(&["summary"])["count"], 0);
    assert_eq!(
        ledger
            .ok(&["installment", "history", id(&added)])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let ok = ledger.ok(&[
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--date",
        "2026-01-31",
        "--request-id",
        "confirm-bad",
    ]);
    assert_eq!(ok["replayed"], false);
    assert_eq!(
        ledger.error(&["installment", "due", "--as-of", "2026-02-30"])["code"],
        "INVALID_INPUT"
    );
}

#[test]
fn exports_and_backups_keep_schedules_links_audit_and_idempotency() {
    let ledger = Ledger::new();
    let added = ledger.add();
    confirm(&ledger, id(&added), "1", "2026-01-31");
    let export = ledger.directory.path().join("export.json");
    ledger.ok(&["export", "--output", export.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&std::fs::read(export).unwrap()).unwrap();
    assert_eq!(exported["version"], 3);
    assert_eq!(
        exported["tables"]["installment_plans"][0]["amount_minor"],
        "100001"
    );
    assert_eq!(
        exported["tables"]["installment_periods"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        exported["tables"]["installment_audit_log"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        exported["amount_units"]["installment_periods.principal_minor"]["unit"],
        "fen"
    );
    let backup = ledger.directory.path().join("backup.sqlite");
    ledger.ok(&["backup", "--output", backup.to_str().unwrap()]);
    let restored = ledger.directory.path().join("restored.sqlite");
    success(run(
        &restored,
        &["restore", "--input", backup.to_str().unwrap()],
        None,
    ));
    let original = ledger.ok(&["installment", "show", id(&added)]);
    assert_eq!(
        success(run(&restored, &["installment", "show", id(&added)], None)),
        original
    );
    assert_eq!(
        success(run(
            &restored,
            &["installment", "history", id(&added)],
            None
        )),
        ledger.ok(&["installment", "history", id(&added)])
    );
    let replay = success(run(
        &restored,
        &[
            "installment",
            "confirm",
            id(&added),
            "--period",
            "1",
            "--date",
            "2026-01-31",
        ],
        None,
    ));
    assert_eq!(replay["replayed"], true);
    assert_eq!(success(run(&restored, &["summary"], None))["count"], 1);
}

#[test]
fn large_plan_total_and_human_output_remain_exact() {
    let ledger = Ledger::new();
    let added = ledger.ok(&[
        "installment",
        "add",
        "--amount",
        "92233720368547758.07",
        "--fee",
        "0.01",
        "--periods",
        "2",
        "--date",
        "2026-01-01",
        "--first-due-date",
        "2026-01-31",
    ]);
    assert_eq!(added["plan"]["total"], "92233720368547758.08");
    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
        .arg("--db")
        .arg(&ledger.db)
        .args(["installment", "show", id(&added)])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("92233720368547758.08"));
    assert!(text.contains("第 2 期"));
    assert!(text.contains("待确认"));
}

#[tokio::test]
async fn late_audit_failure_rolls_back_expense_link_status_and_request_together() {
    use claw_expense::store::Store;
    let ledger = Ledger::new();
    let added = ledger.add();
    let store = Store::open(&ledger.db, false).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_installment_confirmation BEFORE INSERT ON installment_audit_log WHEN NEW.action = 'confirm' BEGIN SELECT RAISE(ABORT, 'injected audit failure'); END;")
        .execute(&store.pool).await.unwrap();
    store.pool.close().await;
    let args = [
        "installment",
        "confirm",
        id(&added),
        "--period",
        "1",
        "--date",
        "2026-01-31",
        "--request-id",
        "atomic-confirm",
    ];
    assert_eq!(ledger.error(&args)["code"], "DATABASE_ERROR");
    assert_eq!(ledger.ok(&["summary"])["count"], 0);
    let current = ledger.ok(&["installment", "show", id(&added)]);
    assert_eq!(current["schedule"], added["schedule"]);
    assert_eq!(
        ledger
            .ok(&["installment", "history", id(&added)])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let store = Store::open(&ledger.db, false).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM idempotency WHERE request_id = 'atomic-confirm'"
        )
        .fetch_one(&store.pool)
        .await
        .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER reject_installment_confirmation")
        .execute(&store.pool)
        .await
        .unwrap();
    store.pool.close().await;
    assert_eq!(ledger.ok(&args)["replayed"], false);
    assert_eq!(ledger.ok(&["summary"])["count"], 1);
}
