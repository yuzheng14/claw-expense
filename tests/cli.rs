use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const EXPENSE_CATEGORY: &str = "集成测试支出";
const INCOME_CATEGORY: &str = "集成测试收入";

struct Ledger {
    directory: TempDir,
    db: PathBuf,
}

impl Ledger {
    fn uninitialized() -> Self {
        let directory = tempfile::tempdir().expect("create isolated test directory");
        let db = directory.path().join("ledger.sqlite3");
        Self { directory, db }
    }

    fn initialized() -> Self {
        let ledger = Self::uninitialized();
        ledger.ok(&["init"]);
        ledger.ok(&["category", "add", EXPENSE_CATEGORY, "--kind", "expense"]);
        ledger.ok(&["category", "add", INCOME_CATEGORY, "--kind", "income"]);
        ledger
    }

    fn run(&self, args: &[&str], input: Option<&str>) -> Output {
        run_at(&self.db, args, input)
    }

    fn ok(&self, args: &[&str]) -> Value {
        success(self.run(args, None))
    }

    fn error(&self, args: &[&str]) -> Value {
        failure(self.run(args, None))
    }

    fn add(&self, kind: &str, amount: &str, date: &str) -> Value {
        let category = if kind == "income" {
            INCOME_CATEGORY
        } else {
            EXPENSE_CATEGORY
        };
        self.ok(&[
            "add",
            kind,
            "--amount",
            amount,
            "--date",
            date,
            "--category",
            category,
        ])
    }
}

fn run_at(db: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch claw-expense binary");
    if let Some(mut stdin) = child.stdin.take()
        && let Some(input) = input
    {
        stdin.write_all(input.as_bytes()).expect("write JSON input");
    }
    child.wait_with_output().expect("wait for CLI result")
}

fn decode(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected one JSON response: {error}; status={:?}; stdout={}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response = decode(&output);
    assert_eq!(response["ok"], true, "{response}");
    assert!(response.get("data").is_some(), "{response}");
    response["data"].clone()
}

fn failure(output: Output) -> Value {
    assert!(!output.status.success(), "CLI unexpectedly succeeded");
    let response = decode(&output);
    assert_eq!(response["ok"], false, "{response}");
    assert!(
        response["error"]["code"]
            .as_str()
            .is_some_and(|code| !code.is_empty())
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    response["error"].clone()
}

fn id(result: &Value) -> &str {
    result["transaction"]["id"]
        .as_str()
        .expect("transaction ID")
}

fn assert_totals(
    summary: &Value,
    income: &str,
    expense: &str,
    refund: &str,
    net: &str,
    balance: &str,
) {
    assert_eq!(summary["income"], income, "{summary}");
    assert_eq!(summary["expense"], expense, "{summary}");
    assert_eq!(summary["refund"], refund, "{summary}");
    assert_eq!(summary["net_expense"], net, "{summary}");
    assert_eq!(summary["balance"], balance, "{summary}");
}

#[test]
fn help_and_version_succeed_without_initializing_a_database() {
    let ledger = Ledger::uninitialized();
    for args in [&["--help"][..], &["--version"][..], &["add", "--help"][..]] {
        let output = ledger.run(args, None);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(!output.stdout.is_empty());
    }
    assert!(!ledger.db.exists());
}

#[test]
fn initialization_is_explicit_and_global_options_work_after_subcommands() {
    let ledger = Ledger::uninitialized();
    ledger.error(&["list"]);
    assert!(
        !ledger.db.exists(),
        "read commands must not silently initialize a ledger"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .arg("init")
        .arg("--db")
        .arg(&ledger.db)
        .arg("--json")
        .output()
        .expect("initialize with trailing global arguments");
    let initialized = success(output);
    assert_eq!(initialized["db"], ledger.db.to_str().unwrap());
    assert!(ledger.db.is_file());
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "0.00",
        "0.00",
        "0.00",
        "0.00",
    );
}

#[test]
fn decimal_arithmetic_and_reopening_are_exact() {
    let ledger = Ledger::initialized();
    let first = ledger.add("expense", "0.10", "2026-09-01");
    let second = ledger.add("expense", "0.20", "2026-09-02");
    ledger.add("income", "0.40", "2026-09-03");
    assert_eq!(first["transaction"]["amount"], "0.10");
    assert_eq!(second["transaction"]["amount"], "0.20");
    assert_eq!(first["replayed"], false);

    // Every invocation opens the database in a new process.
    assert_eq!(
        ledger.ok(&["show", id(&first)])["transaction"],
        first["transaction"]
    );
    assert_totals(
        &ledger.ok(&["summary", "--month", "2026-09"]),
        "0.40",
        "0.30",
        "0.00",
        "0.30",
        "0.10",
    );
    assert_eq!(ledger.ok(&["list"])["total"], 3);
}

#[test]
fn invalid_amounts_and_unrecognized_record_fields_do_not_write_transactions() {
    let ledger = Ledger::initialized();
    for amount in [
        "1.001",
        "0.100",
        "92233720368547758.08",
        "1e2",
        "NaN",
        "0.00",
    ] {
        ledger.error(&["add", "expense", "--amount", amount, "--date", "2026-09-01"]);
    }

    let numeric_amount = json!({
        "kind": "expense", "amount": 0.1, "date": "2026-09-01", "category": EXPENSE_CATEGORY
    });
    failure(ledger.run(&["record"], Some(&numeric_amount.to_string())));
    let unknown_field = json!({
        "kind": "expense", "amount": "0.10", "date": "2026-09-01",
        "category": EXPENSE_CATEGORY, "unexpected": true
    });
    failure(ledger.run(&["record"], Some(&unknown_field.to_string())));
    failure(ledger.run(&["record"], Some("{broken json")));
    assert_eq!(ledger.ok(&["list"])["total"], 0);
}

#[test]
fn stdin_record_preserves_strings_and_rejects_invalid_dates() {
    let ledger = Ledger::initialized();
    let record = json!({
        "kind": "expense", "amount": "98.01", "date": "2026-08-31",
        "category": EXPENSE_CATEGORY, "note": "午餐\n含饮料", "channel": "微信支付"
    });
    let first = success(ledger.run(
        &["record", "--request-id", "stdin-request"],
        Some(&record.to_string()),
    ));
    assert_eq!(first["transaction"]["amount"], "98.01");
    assert_eq!(first["transaction"]["note"], "午餐\n含饮料");
    assert_eq!(first["transaction"]["channel"], "微信支付");
    let replay = success(ledger.run(
        &["record", "--request-id", "stdin-request"],
        Some(&record.to_string()),
    ));
    assert_eq!(replay["replayed"], true);
    assert_eq!(id(&first), id(&replay));
    ledger.error(&["add", "expense", "--amount", "1.00", "--date", "2026-02-30"]);
    ledger.error(&["list", "--month", "2026-13"]);
    assert_eq!(ledger.ok(&["list"])["total"], 1);
}

#[test]
fn excess_refunds_are_legal_and_follow_their_own_month() {
    let ledger = Ledger::initialized();
    let expense = ledger.add("expense", "98.00", "2026-08-31");
    let refund = ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "100.00",
        "--date",
        "2026-09-01",
        "--note",
        "商家多退两元",
        "--channel",
        "支付宝",
    ]);
    assert_eq!(refund["transaction"]["kind"], "refund");
    assert_eq!(refund["transaction"]["original_id"], id(&expense));
    assert_eq!(refund["transaction"]["category"], EXPENSE_CATEGORY);
    let detail = ledger.ok(&["show", id(&expense)]);
    assert_eq!(detail["refund_total"], "100.00");
    assert_eq!(detail["net_expense"], "-2.00");
    assert_eq!(detail["excess_refund"], "2.00");
    assert!(
        detail["refund_status"]
            .as_str()
            .is_some_and(|status| !status.is_empty())
    );
    assert_totals(
        &ledger.ok(&["summary", "--month", "2026-08"]),
        "0.00",
        "98.00",
        "0.00",
        "98.00",
        "-98.00",
    );
    assert_totals(
        &ledger.ok(&["summary", "--month", "2026-09"]),
        "0.00",
        "0.00",
        "100.00",
        "-100.00",
        "100.00",
    );
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "98.00",
        "100.00",
        "-2.00",
        "2.00",
    );
}

#[test]
fn multiple_refunds_keep_independent_payment_channels() {
    let ledger = Ledger::initialized();
    let expense = ledger.ok(&[
        "add",
        "expense",
        "--amount",
        "120",
        "--date",
        "2026-09-01",
        "--category",
        EXPENSE_CATEGORY,
        "--channel",
        "微信支付",
    ]);
    let first = ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "30.10",
        "--date",
        "2026-09-02",
        "--channel",
        "银行卡",
    ]);
    let second = ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "40.20",
        "--date",
        "2026-09-03",
        "--channel",
        "现金",
    ]);
    assert_eq!(first["transaction"]["channel"], "银行卡");
    assert_eq!(second["transaction"]["channel"], "现金");
    let detail = ledger.ok(&["show", id(&expense)]);
    assert_eq!(detail["transaction"]["channel"], "微信支付");
    assert_eq!(detail["refund_total"], "70.30");
    assert_eq!(detail["net_expense"], "49.70");
    assert_eq!(detail["excess_refund"], "0.00");
    assert_eq!(detail["refunds"].as_array().unwrap().len(), 2);
}

#[test]
fn repeated_requests_replay_and_changed_payloads_conflict() {
    let ledger = Ledger::initialized();
    let args = [
        "add",
        "expense",
        "--amount",
        "98.01",
        "--date",
        "2026-09-01",
        "--category",
        EXPENSE_CATEGORY,
        "--request-id",
        "purchase-request",
    ];
    let first = ledger.ok(&args);
    let replay = ledger.ok(&args);
    assert_eq!(first["replayed"], false);
    assert_eq!(replay["replayed"], true);
    assert_eq!(first["transaction"], replay["transaction"]);
    let conflict = ledger.error(&[
        "add",
        "expense",
        "--amount",
        "98.02",
        "--date",
        "2026-09-01",
        "--category",
        EXPENSE_CATEGORY,
        "--request-id",
        "purchase-request",
    ]);
    assert_eq!(conflict["code"], "IDEMPOTENCY_CONFLICT");

    let refund_args = [
        "refund",
        id(&first),
        "--amount",
        "8.01",
        "--date",
        "2026-09-02",
        "--request-id",
        "refund-request",
    ];
    let refund = ledger.ok(&refund_args);
    let refund_replay = ledger.ok(&refund_args);
    assert_eq!(refund_replay["replayed"], true);
    assert_eq!(id(&refund), id(&refund_replay));
    assert_eq!(ledger.ok(&["list"])["total"], 2);
    assert_eq!(ledger.ok(&["show", id(&first)])["refund_total"], "8.01");
    assert_eq!(
        ledger
            .ok(&["history", id(&first)])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn editing_original_to_less_than_refunds_is_legal_and_audited() {
    let ledger = Ledger::initialized();
    let expense = ledger.add("expense", "100.00", "2026-09-01");
    ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "80.00",
        "--date",
        "2026-09-02",
    ]);
    let args = [
        "edit",
        id(&expense),
        "--amount",
        "50.00",
        "--note",
        "更正原始金额",
        "--request-id",
        "edit-request",
    ];
    let updated = ledger.ok(&args);
    assert_eq!(updated["transaction"]["amount"], "50.00");
    assert_eq!(ledger.ok(&args)["replayed"], true);
    let detail = ledger.ok(&["show", id(&expense)]);
    assert_eq!(detail["net_expense"], "-30.00");
    assert_eq!(detail["excess_refund"], "30.00");
    let history = ledger.ok(&["history", id(&expense)]);
    let edits: Vec<_> = history
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| !event["before"].is_null())
        .collect();
    assert_eq!(
        edits.len(),
        1,
        "replay must not add another audit event: {history}"
    );
    assert_eq!(edits[0]["before"]["amount"], "100.00");
    assert_eq!(edits[0]["after"]["amount"], "50.00");
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "50.00",
        "80.00",
        "-30.00",
        "30.00",
    );
}

#[test]
fn void_requires_explicit_refund_cascade_and_audits_every_affected_record() {
    let ledger = Ledger::initialized();
    let expense = ledger.add("expense", "100.00", "2026-09-01");
    let first_refund = ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "20.00",
        "--date",
        "2026-09-02",
    ]);
    let second_refund = ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "30.00",
        "--date",
        "2026-09-03",
    ]);
    ledger.error(&["void", id(&expense)]);
    assert_eq!(ledger.ok(&["list"])["total"], 3);
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "100.00",
        "50.00",
        "50.00",
        "-50.00",
    );

    let args = [
        "void",
        id(&expense),
        "--cascade-refunds",
        "--request-id",
        "void-request",
    ];
    ledger.ok(&args);
    assert_eq!(ledger.ok(&args)["replayed"], true);
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    assert_eq!(ledger.ok(&["list", "--include-voided"])["total"], 3);
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "0.00",
        "0.00",
        "0.00",
        "0.00",
    );
    for transaction in [&expense, &first_refund, &second_refund] {
        assert_eq!(
            ledger.ok(&["show", id(transaction)])["transaction"]["voided"],
            true
        );
        let history = ledger.ok(&["history", id(transaction)]);
        let events = history.as_array().unwrap();
        assert_eq!(
            events.len(),
            2,
            "expected creation and void events: {history}"
        );
        assert!(events.iter().any(|event| event["before"]["voided"] == false && event["after"]["voided"] == true));
    }
    ledger.error(&[
        "refund",
        id(&expense),
        "--amount",
        "1.00",
        "--date",
        "2026-09-04",
    ]);
}

#[test]
fn filters_and_pagination_return_matching_records_and_full_matching_count() {
    let ledger = Ledger::initialized();
    let first = ledger.add("expense", "10", "2026-09-01");
    let second = ledger.add("expense", "20", "2026-09-02");
    ledger.ok(&["edit", id(&first), "--note", "早餐咖啡"]);
    ledger.ok(&["edit", id(&second), "--note", "午餐咖啡"]);
    ledger.add("income", "100", "2026-09-03");
    ledger.add("expense", "30", "2026-08-31");
    let matching = ledger.ok(&[
        "list",
        "--month",
        "2026-09",
        "--kind",
        "expense",
        "--category",
        EXPENSE_CATEGORY,
        "--search",
        "咖啡",
    ]);
    assert_eq!(matching["total"], 2);
    let first_page = ledger.ok(&[
        "list", "--month", "2026-09", "--kind", "expense", "--search", "咖啡", "--limit", "1",
        "--offset", "0",
    ]);
    let second_page = ledger.ok(&[
        "list", "--month", "2026-09", "--kind", "expense", "--search", "咖啡", "--limit", "1",
        "--offset", "1",
    ]);
    assert_eq!(first_page["total"], 2);
    assert_eq!(second_page["total"], 2);
    assert_eq!(first_page["items"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["items"].as_array().unwrap().len(), 1);
    assert_ne!(first_page["items"][0]["id"], second_page["items"][0]["id"]);
    assert_eq!(ledger.ok(&["list", "--search", "没有这个备注"])["total"], 0);
    let categories = ledger.ok(&["category", "list"]);
    assert!(
        categories
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category["name"] == EXPENSE_CATEGORY && category["kind"] == "expense")
    );
    assert!(
        categories
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category["name"] == INCOME_CATEGORY && category["kind"] == "income")
    );
}

#[test]
fn maximum_individual_amounts_and_totals_larger_than_i64_are_exact() {
    let ledger = Ledger::initialized();
    let maximum = "92233720368547758.07";
    let first = ledger.add("expense", maximum, "2026-09-01");
    let second = ledger.add("expense", maximum, "2026-09-02");
    assert_eq!(first["transaction"]["amount"], maximum);
    assert_eq!(second["transaction"]["amount"], maximum);
    assert_totals(
        &ledger.ok(&["summary"]),
        "0.00",
        "184467440737095516.14",
        "0.00",
        "184467440737095516.14",
        "-184467440737095516.14",
    );
}

#[test]
fn backup_restore_preserves_transactions_audit_and_idempotency_without_overwriting() {
    let ledger = Ledger::initialized();
    let args = [
        "add",
        "expense",
        "--amount",
        "98.01",
        "--date",
        "2026-09-01",
        "--category",
        EXPENSE_CATEGORY,
        "--request-id",
        "backup-purchase",
    ];
    let expense = ledger.ok(&args);
    ledger.ok(&["edit", id(&expense), "--note", "备份后应保留"]);
    ledger.ok(&[
        "refund",
        id(&expense),
        "--amount",
        "10.01",
        "--date",
        "2026-09-02",
    ]);
    let voided = ledger.add("income", "9", "2026-09-03");
    ledger.ok(&["void", id(&voided)]);
    let expected_detail = ledger.ok(&["show", id(&expense)]);
    let expected_history = ledger.ok(&["history", id(&expense)]);
    let expected_summary = ledger.ok(&["summary"]);
    let expected_list = ledger.ok(&["list", "--include-voided"]);
    let expected_categories = ledger.ok(&["category", "list"]);

    let backup_path = ledger.directory.path().join("snapshot.sqlite3");
    ledger.ok(&["backup", "--output", backup_path.to_str().unwrap()]);
    assert!(backup_path.is_file());
    let backup_bytes = fs::read(&backup_path).unwrap();
    assert!(backup_bytes.starts_with(b"SQLite format 3\0"));

    let restored_db = ledger.directory.path().join("restored.sqlite3");
    success(run_at(
        &restored_db,
        &["restore", "--input", backup_path.to_str().unwrap()],
        None,
    ));
    let restored = |args: &[&str]| success(run_at(&restored_db, args, None));
    assert_eq!(restored(&["show", id(&expense)]), expected_detail);
    assert_eq!(restored(&["history", id(&expense)]), expected_history);
    assert_eq!(restored(&["summary"]), expected_summary);
    assert_eq!(restored(&["list", "--include-voided"]), expected_list);
    assert_eq!(restored(&["category", "list"]), expected_categories);
    assert_eq!(restored(&args)["replayed"], true);
    assert_eq!(restored(&["list", "--include-voided"])["total"], 3);

    failure(run_at(
        &restored_db,
        &["restore", "--input", backup_path.to_str().unwrap()],
        None,
    ));
    assert_eq!(restored(&["summary"]), expected_summary);

    let sentinel = ledger.directory.path().join("existing.txt");
    fs::write(&sentinel, b"existing user data").unwrap();
    failure(run_at(
        &sentinel,
        &["restore", "--input", backup_path.to_str().unwrap()],
        None,
    ));
    assert_eq!(fs::read(&sentinel).unwrap(), b"existing user data");
    ledger.error(&["backup", "--output", backup_path.to_str().unwrap()]);
    assert_eq!(fs::read(&backup_path).unwrap(), backup_bytes);

    let export_path = ledger.directory.path().join("export.json");
    ledger.ok(&["export", "--output", export_path.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&fs::read(&export_path).unwrap()).unwrap();
    assert!(!exported.is_null(), "export must contain a JSON document");
}

#[test]
fn database_creation_rejects_orphaned_sqlite_sidecars_without_changing_them() {
    let ledger = Ledger::initialized();
    ledger.add("expense", "12.34", "2026-09-01");
    let source_backup = ledger.directory.path().join("sidecar-source.sqlite3");
    ledger.ok(&["backup", "--output", source_backup.to_str().unwrap()]);

    for operation in ["init", "backup", "restore"] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let destination = ledger
                .directory
                .path()
                .join(format!("{operation}{suffix}.sqlite3"));
            let mut sidecar_name = destination.as_os_str().to_os_string();
            sidecar_name.push(suffix);
            let sidecar = PathBuf::from(sidecar_name);
            let original_bytes =
                format!("preserve this {operation} {suffix} sidecar\0\n").into_bytes();
            fs::write(&sidecar, &original_bytes).unwrap();
            assert!(!destination.exists());

            let output = match operation {
                "init" => run_at(&destination, &["init"], None),
                "backup" => {
                    ledger.run(&["backup", "--output", destination.to_str().unwrap()], None)
                }
                "restore" => run_at(
                    &destination,
                    &["restore", "--input", source_backup.to_str().unwrap()],
                    None,
                ),
                _ => unreachable!(),
            };
            failure(output);
            assert!(
                !destination.exists(),
                "{operation} created a database alongside an existing {suffix} file"
            );
            assert_eq!(
                fs::read(&sidecar).unwrap(),
                original_bytes,
                "{operation} changed the existing {suffix} file"
            );
        }
    }
    assert_eq!(ledger.ok(&["list"])["total"], 1);
}

#[test]
fn export_preserves_large_integers_audit_and_idempotency_and_refuses_overwrite() {
    let ledger = Ledger::initialized();
    let maximum = "92233720368547758.07";
    let expense = ledger.ok(&[
        "add",
        "expense",
        "--amount",
        maximum,
        "--date",
        "2026-09-01",
        "--category",
        EXPENSE_CATEGORY,
        "--request-id",
        "export-create",
    ]);
    ledger.ok(&[
        "edit",
        id(&expense),
        "--note",
        "完整保留审计记录",
        "--request-id",
        "export-edit",
    ]);
    ledger.ok(&["void", id(&expense), "--request-id", "export-void"]);

    let export_path = ledger.directory.path().join("lossless-export.json");
    ledger.ok(&["export", "--output", export_path.to_str().unwrap()]);
    let original_bytes = fs::read(&export_path).unwrap();
    let exported: Value = serde_json::from_slice(&original_bytes).unwrap();
    let transactions = exported["tables"]["transactions"].as_array().unwrap();
    assert_eq!(
        transactions.len(),
        1,
        "voided records must remain in exports"
    );
    assert_eq!(transactions[0]["id"], id(&expense));
    assert_eq!(transactions[0]["amount_minor"], i64::MAX.to_string());
    assert_eq!(transactions[0]["voided"], "1");
    assert_eq!(transactions[0]["note"], "完整保留审计记录");

    let audit = exported["tables"]["audit_log"].as_array().unwrap();
    assert_eq!(audit.len(), 3);
    for action in ["create", "update", "void"] {
        let event = audit
            .iter()
            .find(|event| event["action"] == action)
            .unwrap();
        assert_eq!(event["transaction_id"], id(&expense));
        let after: Value = serde_json::from_str(event["after_json"].as_str().unwrap()).unwrap();
        assert_eq!(after["id"], id(&expense));
        assert_eq!(after["amount"], maximum);
    }

    let requests = exported["tables"]["idempotency"].as_array().unwrap();
    assert_eq!(requests.len(), 3);
    for request_id in ["export-create", "export-edit", "export-void"] {
        let request = requests
            .iter()
            .find(|request| request["request_id"] == request_id)
            .unwrap();
        let payload: Value = serde_json::from_str(request["payload"].as_str().unwrap()).unwrap();
        assert!(!payload.is_null());
        let response: Value =
            serde_json::from_str(request["response_json"].as_str().unwrap()).unwrap();
        assert_eq!(response["transaction"]["id"], id(&expense));
        assert_eq!(response["transaction"]["amount"], maximum);
    }

    // Make a replacement export differ, so an accidental overwrite cannot pass.
    ledger.add("income", "1.00", "2026-09-02");
    ledger.error(&["export", "--output", export_path.to_str().unwrap()]);
    assert_eq!(fs::read(&export_path).unwrap(), original_bytes);
}

#[test]
fn pagination_rejects_zero_or_excessive_limits_and_negative_offsets() {
    let ledger = Ledger::initialized();
    for args in [
        &["list", "--limit", "0"][..],
        &["list", "--limit", "1001"][..],
        &["list", "--offset", "-1"][..],
    ] {
        ledger.error(args);
    }
    assert_eq!(
        ledger.ok(&["list", "--limit", "1", "--offset", "0"])["total"],
        0
    );
    assert_eq!(ledger.ok(&["list", "--limit", "1000"])["total"], 0);
}
