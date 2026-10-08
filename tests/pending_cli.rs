use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;

const CATEGORY: &str = "外币测试购物";

struct Ledger {
    directory: TempDir,
    db: PathBuf,
}

impl Ledger {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("isolated ledger directory");
        let db = directory.path().join("ledger.sqlite3");
        let ledger = Self { directory, db };
        ledger.ok(&["init"]);
        ledger.ok(&["category", "add", CATEGORY, "--kind", "expense"]);
        ledger
    }

    fn ok(&self, args: &[&str]) -> Value {
        success(run_at(&self.db, args, None))
    }

    fn error(&self, args: &[&str]) -> Value {
        failure(run_at(&self.db, args, None))
    }

    fn add(&self, currency: &str, amount: &str, date: &str) -> Value {
        self.ok(&[
            "pending",
            "add",
            "--currency",
            currency,
            "--amount",
            amount,
            "--date",
            date,
            "--category",
            CATEGORY,
        ])
    }
}

fn spawn_at(db: &Path, args: &[&str]) -> Child {
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
        .expect("start CLI")
}

fn run_at(db: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut child = spawn_at(db, args);
    if let Some(mut stdin) = child.stdin.take()
        && let Some(input) = input
    {
        stdin.write_all(input.as_bytes()).expect("write JSON input");
    }
    child.wait_with_output().expect("wait for CLI")
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
    let value = decode(&output);
    assert_eq!(value["ok"], true, "{value}");
    value["data"].clone()
}

fn failure(output: Output) -> Value {
    assert!(!output.status.success(), "CLI unexpectedly succeeded");
    let value = decode(&output);
    assert_eq!(value["ok"], false, "{value}");
    assert!(
        value["error"]["code"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    value["error"].clone()
}

fn pending_id(value: &Value) -> &str {
    value["pending"]["id"].as_str().expect("pending ID")
}

fn transaction_id(value: &Value) -> &str {
    value["transaction"]["id"].as_str().expect("transaction ID")
}

fn currency_total<'a>(summary: &'a Value, currency: &str) -> &'a Value {
    summary["pending_by_currency"]
        .as_array()
        .expect("pending currency totals")
        .iter()
        .find(|item| item["currency"] == currency)
        .unwrap_or_else(|| panic!("missing {currency}: {summary}"))
}

#[test]
fn twd_records_use_exact_two_decimal_amounts_and_confirm_without_duplicate_cny() {
    let ledger = Ledger::new();
    let mut record = json!({
        "currency": "TWD", "amount": "1234.56", "date": "2026-09-24",
        "category": CATEGORY, "merchant": "台湾海淘", "channel": "信用卡"
    });
    let args = ["pending", "record", "--request-id", "twd-purchase"];
    let added = success(run_at(&ledger.db, &args, Some(&record.to_string())));
    assert_eq!(added["pending"]["currency"], "TWD");
    assert_eq!(added["pending"]["amount"], "1234.56");
    assert!(added["transaction"].is_null());
    record["amount"] = json!("001234.56");
    let replayed = success(run_at(&ledger.db, &args, Some(&record.to_string())));
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["pending"], added["pending"]);

    let cent = ledger.add("TWD", "0.01", "2026-09-24");
    assert_eq!(cent["pending"]["amount"], "0.01");
    ledger.add("USD", "20", "2026-09-24");
    ledger.error(&[
        "pending",
        "add",
        "--currency",
        "TWD",
        "--amount",
        "1.001",
        "--date",
        "2026-09-24",
    ]);
    record["amount"] = json!(1234.56);
    failure(run_at(
        &ledger.db,
        &["pending", "record"],
        Some(&record.to_string()),
    ));
    let before = ledger.ok(&["summary"]);
    assert_eq!(before["pending_count"], 3);
    assert_eq!(before["expense"], "0.00");
    assert_eq!(currency_total(&before, "TWD")["amount"], "1234.57");
    assert_eq!(currency_total(&before, "TWD")["count"], 2);
    let filtered = ledger.ok(&["pending", "list", "--currency", "TWD"]);
    assert_eq!(filtered["total"], 2);
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["currency"] == "TWD")
    );

    let confirm_args = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "278.91",
        "--posted-date",
        "2026-09-28",
        "--request-id",
        "twd-confirm",
    ];
    let confirmed = ledger.ok(&confirm_args);
    let confirm_replay = ledger.ok(&confirm_args);
    assert_eq!(confirm_replay["replayed"], true);
    assert_eq!(confirm_replay["transaction"], confirmed["transaction"]);
    assert_eq!(confirmed["transaction"]["currency"], "CNY");
    assert_eq!(confirmed["transaction"]["amount"], "278.91");
    assert_eq!(confirmed["pending"]["currency"], "TWD");
    assert_eq!(confirmed["pending"]["amount"], "1234.56");
    let detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    assert_eq!(detail["foreign_expense"]["currency"], "TWD");
    assert_eq!(detail["foreign_expense"]["amount"], "1234.56");
    assert_eq!(ledger.ok(&["list"])["total"], 1);
    let after = ledger.ok(&["summary"]);
    assert_eq!(after["count"], 1);
    assert_eq!(after["expense"], "278.91");
    assert_eq!(currency_total(&after, "TWD")["amount"], "0.01");
    assert_eq!(currency_total(&after, "TWD")["count"], 1);
}

#[test]
fn twd_backup_restore_and_export_preserve_amounts_precision_and_request_history() {
    let ledger = Ledger::new();
    let added = ledger.add("TWD", "1234.56", "2026-09-24");
    let confirmation = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "278.91",
        "--request-id",
        "twd-backup-confirm",
    ];
    let confirmed = ledger.ok(&confirmation);
    let waiting = ledger.add("TWD", "0.01", "2026-09-25");
    ledger.ok(&[
        "pending",
        "snooze",
        pending_id(&waiting),
        "--until",
        "2026-10-01",
    ]);
    let list = ledger.ok(&[
        "pending",
        "list",
        "--status",
        "all",
        "--currency",
        "TWD",
        "--as-of",
        "2026-10-01",
    ]);
    let history = ledger.ok(&["pending", "history", pending_id(&added)]);
    let detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    let summary = ledger.ok(&["summary"]);
    let backup = ledger.directory.path().join("twd-backup.sqlite3");
    ledger.ok(&["backup", "--output", backup.to_str().unwrap()]);
    let restored_db = ledger.directory.path().join("twd-restored.sqlite3");
    success(run_at(
        &restored_db,
        &["restore", "--input", backup.to_str().unwrap()],
        None,
    ));
    let restored = |args: &[&str]| success(run_at(&restored_db, args, None));
    assert_eq!(
        restored(&[
            "pending",
            "list",
            "--status",
            "all",
            "--currency",
            "TWD",
            "--as-of",
            "2026-10-01"
        ]),
        list
    );
    assert_eq!(
        restored(&["pending", "history", pending_id(&added)]),
        history
    );
    assert_eq!(restored(&["show", transaction_id(&confirmed)]), detail);
    assert_eq!(restored(&["summary"]), summary);
    assert_eq!(restored(&confirmation)["replayed"], true);
    assert_eq!(restored(&["list"])["total"], 1);

    let export = ledger.directory.path().join("twd-export.json");
    restored(&["export", "--output", export.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&fs::read(export).unwrap()).unwrap();
    assert_eq!(
        exported["amount_units"]["pending_expenses.amount_minor"],
        json!({"currency_column": "currency", "unit": "currency_hundredth", "exponent": 2})
    );
    let rows = exported["tables"]["pending_expenses"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let original = rows
        .iter()
        .find(|row| row["id"] == pending_id(&added))
        .unwrap();
    assert_eq!(original["currency"], "TWD");
    assert_eq!(original["amount_minor"], "123456");
    assert_eq!(original["confirmed_amount_minor"], "27891");
    let cent = rows
        .iter()
        .find(|row| row["id"] == pending_id(&waiting))
        .unwrap();
    assert_eq!(cent["amount_minor"], "1");
    assert!(cent["confirmed_amount_minor"].is_null());
    assert_eq!(
        exported["tables"]["pending_audit_log"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert!(
        exported["tables"]["idempotency"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["request_id"] == "twd-backup-confirm")
    );
}

#[test]
fn pending_purchase_has_no_cny_amount_until_confirmed_and_retains_both_dates() {
    let ledger = Ledger::new();
    let added = ledger.ok(&[
        "pending",
        "add",
        "--currency",
        "JPY",
        "--amount",
        "10000",
        "--date",
        "2026-09-30",
        "--category",
        CATEGORY,
        "--merchant",
        "乐天",
        "--channel",
        "招行信用卡",
        "--note",
        "海淘",
    ]);
    assert_eq!(added["pending"]["currency"], "JPY");
    assert_eq!(added["pending"]["amount"], "10000");
    assert_eq!(added["pending"]["date"], "2026-09-30");
    assert_eq!(added["pending"]["status"], "pending");
    assert_eq!(added["pending"]["remind_on"], "2026-10-03");
    assert!(added["pending"]["transaction_id"].is_null());
    assert!(added["pending"]["confirmed_at"].is_null());
    assert!(added["pending"]["posted_date"].is_null());
    assert!(added["transaction"].is_null());
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    let before = ledger.ok(&["summary", "--month", "2026-09"]);
    assert_eq!(before["expense"], "0.00");
    assert_eq!(before["pending_count"], 1);
    assert_eq!(currency_total(&before, "JPY")["amount"], "10000");

    let confirmed = ledger.ok(&[
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "510.38",
        "--posted-date",
        "2026-10-03",
    ]);
    assert_eq!(confirmed["pending"]["status"], "confirmed");
    assert_eq!(confirmed["pending"]["posted_date"], "2026-10-03");
    assert!(confirmed["pending"]["confirmed_at"].is_string());
    assert_eq!(confirmed["pending"]["amount"], "10000");
    assert_eq!(confirmed["pending"]["merchant"], "乐天");
    assert_eq!(confirmed["transaction"]["kind"], "expense");
    assert_eq!(confirmed["transaction"]["amount"], "510.38");
    assert_eq!(confirmed["transaction"]["date"], "2026-09-30");
    assert_eq!(confirmed["transaction"]["category"], CATEGORY);
    assert_eq!(confirmed["transaction"]["channel"], "招行信用卡");
    assert_eq!(
        confirmed["pending"]["transaction_id"],
        transaction_id(&confirmed)
    );
    assert_eq!(
        ledger.ok(&["pending", "show", pending_id(&added)])["pending"],
        confirmed["pending"]
    );
    let detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    assert_eq!(detail["foreign_expense"]["currency"], "JPY");
    assert_eq!(detail["foreign_expense"]["amount"], "10000");
    let september = ledger.ok(&["summary", "--month", "2026-09"]);
    assert_eq!(september["expense"], "510.38");
    assert_eq!(september["pending_count"], 0);
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-10"])["expense"],
        "0.00"
    );
}

#[test]
fn pending_summary_is_per_currency_exact_and_excludes_closed_or_out_of_range_items() {
    let ledger = Ledger::new();
    ledger.add("USD", "0.10", "2026-09-01");
    ledger.add("USD", "0.20", "2026-09-02");
    ledger.add("JPY", "10000", "2026-09-03");
    ledger.add("USD", "999", "2026-08-31");
    let cancelled = ledger.add("JPY", "200", "2026-09-04");
    ledger.ok(&["pending", "cancel", pending_id(&cancelled)]);
    let confirmed = ledger.add("USD", "20", "2026-09-05");
    ledger.ok(&[
        "pending",
        "confirm",
        pending_id(&confirmed),
        "--amount",
        "145.00",
    ]);

    let summary = ledger.ok(&["summary", "--month", "2026-09"]);
    assert_eq!(summary["pending_count"], 3);
    assert_eq!(summary["expense"], "145.00");
    assert_eq!(summary["pending_by_currency"].as_array().unwrap().len(), 2);
    assert_eq!(currency_total(&summary, "USD")["amount"], "0.30");
    assert_eq!(currency_total(&summary, "USD")["count"], 2);
    assert_eq!(currency_total(&summary, "JPY")["amount"], "10000");
    assert_eq!(currency_total(&summary, "JPY")["count"], 1);
    let filtered = ledger.ok(&["summary", "--from", "2026-09-02", "--to", "2026-09-03"]);
    assert_eq!(filtered["pending_count"], 2);
    assert_eq!(filtered["expense"], "0.00");
    assert_eq!(currency_total(&filtered, "USD")["amount"], "0.20");
}

#[test]
fn foreign_amount_precision_and_i128_totals_are_exact() {
    let ledger = Ledger::new();
    for (currency, amount) in [
        ("JPY", "1.1"),
        ("USD", "1.001"),
        ("USD", "1e3"),
        ("USD", "NaN"),
        ("USD", "0"),
        ("USD", "92233720368547758.08"),
        ("JPY", "92233720368547759"),
        ("KRW", "92233720368547759"),
        ("ZZZ", "1"),
    ] {
        ledger.error(&[
            "pending",
            "add",
            "--currency",
            currency,
            "--amount",
            amount,
            "--date",
            "2026-09-01",
        ]);
    }
    assert_eq!(ledger.ok(&["pending", "list"])["total"], 0);
    let maximum_usd = "92233720368547758.07";
    let maximum_integer_currency = "92233720368547758";
    for _ in 0..2 {
        assert_eq!(
            ledger.add("USD", maximum_usd, "2026-09-01")["pending"]["amount"],
            maximum_usd
        );
        for currency in ["JPY", "KRW"] {
            assert_eq!(
                ledger.add(currency, maximum_integer_currency, "2026-09-01")["pending"]["amount"],
                maximum_integer_currency
            );
        }
    }
    let summary = ledger.ok(&["summary"]);
    assert_eq!(
        currency_total(&summary, "USD")["amount"],
        "184467440737095516.14"
    );
    for currency in ["JPY", "KRW"] {
        assert_eq!(
            currency_total(&summary, currency)["amount"],
            "184467440737095516"
        );
    }
    assert_eq!(summary["expense"], "0.00");
}

async fn read_only_connection(db: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(db).read_only(true))
        .await
        .unwrap()
}

async fn stored_write_counts(db: &Path) -> (i64, i64, i64, i64, i64) {
    let mut connection = read_only_connection(db).await;
    let counts = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM pending_expenses), (SELECT COUNT(*) FROM pending_audit_log), (SELECT COUNT(*) FROM idempotency), (SELECT COUNT(*) FROM transactions), (SELECT COUNT(*) FROM audit_log)",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    counts
}

#[tokio::test]
async fn integer_currencies_store_hundredths_but_display_whole_units_and_replay_leading_zeroes() {
    let ledger = Ledger::new();
    let mut ids = Vec::new();
    for currency in ["JPY", "KRW"] {
        let request_id = format!("integer-{currency}");
        let args = |amount| {
            [
                "pending",
                "add",
                "--currency",
                currency,
                "--amount",
                amount,
                "--date",
                "2026-09-24",
                "--category",
                CATEGORY,
                "--request-id",
                request_id.as_str(),
            ]
        };
        let added = ledger.ok(&args("1000"));
        assert_eq!(added["pending"]["amount"], "1000");
        let repeated = ledger.ok(&args("0001000"));
        assert_eq!(repeated["replayed"], true);
        assert_eq!(repeated["pending"], added["pending"]);
        let record = json!({
            "currency": currency, "amount": "001000", "date": "2026-09-24", "category": CATEGORY
        });
        let repeated_json = success(run_at(
            &ledger.db,
            &["pending", "record", "--request-id", &request_id],
            Some(&record.to_string()),
        ));
        assert_eq!(repeated_json["replayed"], true);
        assert_eq!(repeated_json["pending"], added["pending"]);
        ids.push((currency, pending_id(&added).to_owned()));
    }
    assert_eq!(stored_write_counts(&ledger.db).await, (2, 2, 2, 0, 0));
    let mut connection = read_only_connection(&ledger.db).await;
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT currency, amount_minor, typeof(amount_minor) FROM pending_expenses ORDER BY currency",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("JPY".to_owned(), 100000, "integer".to_owned()),
            ("KRW".to_owned(), 100000, "integer".to_owned()),
        ]
    );
    connection.close().await.unwrap();
    let summary = ledger.ok(&["summary"]);
    for currency in ["JPY", "KRW"] {
        assert_eq!(currency_total(&summary, currency)["amount"], "1000");
    }

    let backup = ledger.directory.path().join("integer-backup.sqlite3");
    ledger.ok(&["backup", "--output", backup.to_str().unwrap()]);
    let restored_db = ledger.directory.path().join("integer-restored.sqlite3");
    success(run_at(
        &restored_db,
        &["restore", "--input", backup.to_str().unwrap()],
        None,
    ));
    let restored = |args: &[&str]| success(run_at(&restored_db, args, None));
    assert_eq!(restored(&["summary"]), summary);
    assert_eq!(stored_write_counts(&restored_db).await, (2, 2, 2, 0, 0));
    let export = ledger.directory.path().join("integer-export.json");
    restored(&["export", "--output", export.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&fs::read(export).unwrap()).unwrap();
    assert_eq!(
        exported["amount_units"]["pending_expenses.amount_minor"],
        json!({"currency_column": "currency", "unit": "currency_hundredth", "exponent": 2})
    );
    let rows = exported["tables"]["pending_expenses"].as_array().unwrap();
    for (currency, id) in ids {
        let row = rows.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(row["currency"], currency);
        assert_eq!(row["amount_minor"], "100000");
        assert_eq!(
            restored(&["pending", "show", &id])["pending"]["amount"],
            "1000"
        );
        let request_id = format!("integer-{currency}");
        let record = json!({
            "currency": currency, "amount": "0001000", "date": "2026-09-24", "category": CATEGORY
        });
        let replayed = success(run_at(
            &restored_db,
            &["pending", "record", "--request-id", &request_id],
            Some(&record.to_string()),
        ));
        assert_eq!(replayed["replayed"], true);
        assert_eq!(replayed["pending"]["id"], id);
    }
    assert_eq!(stored_write_counts(&restored_db).await, (2, 2, 2, 0, 0));
}

#[tokio::test]
async fn integer_currencies_reject_decimal_syntax_without_record_audit_or_request_side_effects() {
    let ledger = Ledger::new();
    for currency in ["JPY", "KRW"] {
        for (index, amount) in ["1.0", "1.00", "1000.00", "0.01", "1.", "92233720368547759"]
            .into_iter()
            .enumerate()
        {
            let request_id = format!("rejected-{currency}-{index}");
            ledger.error(&[
                "pending",
                "add",
                "--currency",
                currency,
                "--amount",
                amount,
                "--date",
                "2026-09-24",
                "--category",
                CATEGORY,
                "--request-id",
                &request_id,
            ]);
            let record = json!({
                "currency": currency, "amount": amount, "date": "2026-09-24", "category": CATEGORY
            });
            failure(run_at(
                &ledger.db,
                &["pending", "record", "--request-id", &request_id],
                Some(&record.to_string()),
            ));
            assert_eq!(stored_write_counts(&ledger.db).await, (0, 0, 0, 0, 0));
        }
    }
    let accepted = ledger.ok(&[
        "pending",
        "add",
        "--currency",
        "JPY",
        "--amount",
        "1000",
        "--date",
        "2026-09-24",
        "--request-id",
        "rejected-JPY-0",
    ]);
    assert_eq!(accepted["replayed"], false);
    assert_eq!(accepted["pending"]["amount"], "1000");
    assert_eq!(stored_write_counts(&ledger.db).await, (1, 1, 1, 0, 0));
}

#[test]
fn json_record_requires_string_amount_and_explicit_valid_date_and_replays() {
    let ledger = Ledger::new();
    let record = json!({
        "currency": "USD", "amount": "20.00", "date": "2026-09-24", "category": CATEGORY,
        "merchant": "Codex", "channel": "信用卡", "note": "订阅\n待入账"
    });
    let args = ["pending", "record", "--request-id", "json-purchase"];
    let added = success(run_at(&ledger.db, &args, Some(&record.to_string())));
    let replayed = success(run_at(&ledger.db, &args, Some(&record.to_string())));
    assert_eq!(added["replayed"], false);
    assert_eq!(replayed["replayed"], true);
    assert_eq!(added["pending"], replayed["pending"]);
    assert_eq!(added["pending"]["note"], "订阅\n待入账");
    for invalid in [
        json!({"currency":"USD", "amount":20.1, "date":"2026-09-24"}),
        json!({"currency":"USD", "amount":"20.1"}),
        json!({"currency":"USD", "amount":"20.1", "date":"2026-02-30"}),
        json!({"currency":"USD", "amount":"20.1", "date":"2026-09-24", "unknown":true}),
    ] {
        failure(run_at(
            &ledger.db,
            &["pending", "record"],
            Some(&invalid.to_string()),
        ));
    }
    failure(run_at(&ledger.db, &["pending", "record"], Some("{broken")));
    let file = ledger.directory.path().join("pending.json");
    fs::write(&file, record.to_string()).unwrap();
    let from_file = ledger.ok(&["pending", "record", "--input", file.to_str().unwrap()]);
    assert_eq!(from_file["pending"]["merchant"], "Codex");
    assert_eq!(ledger.ok(&["pending", "list"])["total"], 2);
}

#[test]
fn confirm_is_atomic_idempotent_and_terminal_and_cny_validation_is_strict() {
    let ledger = Ledger::new();
    let added = ledger.add("USD", "20.00", "2026-09-24");
    for amount in ["0", "1.001", "92233720368547758.08"] {
        ledger.error(&["pending", "confirm", pending_id(&added), "--amount", amount]);
    }
    ledger.error(&[
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "145",
        "--posted-date",
        "2026-09-23",
    ]);
    assert_eq!(
        ledger.ok(&["pending", "show", pending_id(&added)])["pending"]["status"],
        "pending"
    );
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    let args = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "145.27",
        "--request-id",
        "confirm-codex",
    ];
    let confirmed = ledger.ok(&args);
    let replayed = ledger.ok(&args);
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["pending"], confirmed["pending"]);
    assert_eq!(replayed["transaction"], confirmed["transaction"]);
    ledger.error(&[
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "145.28",
        "--request-id",
        "confirm-codex",
    ]);
    let repeated = ledger.ok(&[
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "145.27",
    ]);
    assert_eq!(repeated["transaction"], confirmed["transaction"]);
    let error = ledger.error(&[
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "145.28",
    ]);
    assert_eq!(error["code"], "ALREADY_CONFIRMED");
    ledger.error(&["pending", "cancel", pending_id(&added)]);
    ledger.error(&[
        "pending",
        "snooze",
        pending_id(&added),
        "--until",
        "2026-10-01",
    ]);
    assert_eq!(ledger.ok(&["list"])["total"], 1);
    assert_eq!(ledger.ok(&["summary"])["expense"], "145.27");
    let history = ledger.ok(&["pending", "history", pending_id(&added)]);
    assert_eq!(
        history.as_array().unwrap().len(),
        2,
        "failed writes/replays must not add audit events"
    );
    assert_eq!(
        ledger
            .ok(&["history", transaction_id(&confirmed)])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn concurrent_confirm_requests_create_only_one_cny_expense() {
    let ledger = Ledger::new();
    let added = ledger.add("JPY", "10000", "2026-09-24");
    let args = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "510.38",
        "--request-id",
        "concurrent-confirm",
    ];
    let first = spawn_at(&ledger.db, &args);
    let second = spawn_at(&ledger.db, &args);
    let a = success(first.wait_with_output().unwrap());
    let b = success(second.wait_with_output().unwrap());
    assert_eq!(transaction_id(&a), transaction_id(&b));
    assert_ne!(a["replayed"], b["replayed"]);
    assert_eq!(ledger.ok(&["list"])["total"], 1);
    assert_eq!(ledger.ok(&["summary"])["expense"], "510.38");
    assert_eq!(
        ledger
            .ok(&["pending", "history", pending_id(&added)])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn concurrent_conflicting_confirmations_cannot_both_settle_the_same_purchase() {
    let ledger = Ledger::new();
    let added = ledger.add("USD", "20", "2026-09-24");
    let first = spawn_at(
        &ledger.db,
        &[
            "pending",
            "confirm",
            pending_id(&added),
            "--amount",
            "145.27",
        ],
    );
    let second = spawn_at(
        &ledger.db,
        &[
            "pending",
            "confirm",
            pending_id(&added),
            "--amount",
            "145.28",
        ],
    );
    let mut outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1,
        "exactly one of two conflicting confirmations may succeed: {outputs:?}"
    );
    outputs.sort_by_key(|output| output.status.success());
    let [failed, succeeded] = outputs;
    assert_eq!(failure(failed)["code"], "ALREADY_CONFIRMED");
    let confirmed = success(succeeded);
    assert_eq!(ledger.ok(&["list"])["total"], 1);
    assert_eq!(
        ledger.ok(&["summary"])["expense"],
        confirmed["transaction"]["amount"]
    );
    assert_eq!(
        ledger
            .ok(&["pending", "history", pending_id(&added)])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn due_dates_snoozing_and_cancellation_keep_the_reminder_queue_accurate() {
    let ledger = Ledger::new();
    let added = ledger.add("USD", "20", "2026-09-24");
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-09-26"])["total"],
        0
    );
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-09-27"])["total"],
        1
    );
    let args = [
        "pending",
        "snooze",
        pending_id(&added),
        "--until",
        "2026-10-01",
        "--request-id",
        "snooze-codex",
    ];
    let snoozed = ledger.ok(&args);
    assert_eq!(snoozed["pending"]["remind_on"], "2026-10-01");
    assert_eq!(ledger.ok(&args)["replayed"], true);
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-09-30"])["total"],
        0
    );
    let due = ledger.ok(&["pending", "due", "--as-of", "2026-10-01"]);
    assert_eq!(due["as_of"], "2026-10-01");
    assert_eq!(due["total"], 1);
    let cancel_args = [
        "pending",
        "cancel",
        pending_id(&added),
        "--request-id",
        "cancel-codex",
    ];
    assert_eq!(ledger.ok(&cancel_args)["pending"]["status"], "cancelled");
    assert_eq!(ledger.ok(&cancel_args)["replayed"], true);
    ledger.error(&["pending", "confirm", pending_id(&added), "--amount", "145"]);
    ledger.error(&[
        "pending",
        "snooze",
        pending_id(&added),
        "--until",
        "2026-10-02",
    ]);
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-12-01"])["total"],
        0
    );
    assert_eq!(
        ledger.ok(&["pending", "list", "--status", "cancelled"])["total"],
        1
    );
    assert_eq!(ledger.ok(&["summary"])["pending_count"], 0);
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    assert_eq!(
        ledger
            .ok(&["pending", "history", pending_id(&added)])
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn pending_filters_pagination_and_search_are_independent_of_cny_transactions() {
    let ledger = Ledger::new();
    for date in ["2026-09-01", "2026-09-02"] {
        ledger.ok(&[
            "pending",
            "add",
            "--currency",
            "USD",
            "--amount",
            "20",
            "--date",
            date,
            "--category",
            CATEGORY,
            "--merchant",
            "Codex",
        ]);
    }
    ledger.add("JPY", "2000", "2026-09-03");
    ledger.add("USD", "10", "2026-08-31");
    let closed = ledger.add("USD", "3", "2026-09-04");
    ledger.ok(&["pending", "cancel", pending_id(&closed)]);
    let filtered = ledger.ok(&[
        "pending",
        "list",
        "--status",
        "pending",
        "--month",
        "2026-09",
        "--currency",
        "USD",
        "--category",
        CATEGORY,
        "--search",
        "Codex",
    ]);
    assert_eq!(filtered["total"], 2);
    let first = ledger.ok(&[
        "pending", "list", "--month", "2026-09", "--search", "Codex", "--limit", "1", "--offset",
        "0",
    ]);
    let second = ledger.ok(&[
        "pending", "list", "--month", "2026-09", "--search", "Codex", "--limit", "1", "--offset",
        "1",
    ]);
    assert_eq!(first["total"], 2);
    assert_eq!(second["total"], 2);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    assert_eq!(
        ledger.ok(&["pending", "list", "--status", "all"])["total"],
        5
    );
    assert_eq!(
        ledger.ok(&[
            "pending",
            "list",
            "--from",
            "2026-09-02",
            "--to",
            "2026-09-03"
        ])["total"],
        2
    );
    for args in [
        &["pending", "list", "--limit", "0"][..],
        &["pending", "list", "--limit", "1001"][..],
        &["pending", "list", "--offset", "-1"][..],
        &["pending", "list", "--as-of", "2026-02-30"][..],
        &["pending", "list", "--month", "2026-13"][..],
    ] {
        ledger.error(args);
    }
}

#[test]
fn confirmed_foreign_expenses_keep_existing_excess_refund_behavior() {
    let ledger = Ledger::new();
    let added = ledger.add("USD", "13", "2026-09-24");
    let confirmed = ledger.ok(&["pending", "confirm", pending_id(&added), "--amount", "98"]);
    ledger.ok(&[
        "refund",
        transaction_id(&confirmed),
        "--amount",
        "100",
        "--date",
        "2026-09-25",
        "--channel",
        "支付宝",
    ]);
    let detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    assert_eq!(detail["net_expense"], "-2.00");
    assert_eq!(detail["excess_refund"], "2.00");
    assert_eq!(detail["foreign_expense"]["amount"], "13.00");
    assert_eq!(detail["foreign_expense"]["currency"], "USD");
    assert_eq!(ledger.ok(&["summary"])["net_expense"], "-2.00");
}

#[test]
fn human_pending_list_identifies_original_currency_without_fake_zero_cny() {
    let ledger = Ledger::new();
    ledger.add("JPY", "10000", "2026-09-24");
    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
        .arg("--db")
        .arg(&ledger.db)
        .args(["pending", "list", "--as-of", "2026-09-28"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(rendered.contains("JPY"), "{rendered}");
    assert!(rendered.contains("10000"), "{rendered}");
    assert!(
        !rendered.contains("0.00"),
        "unknown CNY must not appear as zero: {rendered}"
    );
}

#[test]
fn backup_restore_and_export_preserve_pending_closed_items_audit_and_idempotency() {
    let ledger = Ledger::new();
    let added = ledger.add("JPY", "10000", "2026-09-24");
    let confirmation = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "510.38",
        "--posted-date",
        "2026-09-28",
        "--request-id",
        "backup-confirm",
    ];
    let confirmed = ledger.ok(&confirmation);
    let waiting = ledger.add("USD", "20", "2026-09-25");
    ledger.ok(&[
        "pending",
        "snooze",
        pending_id(&waiting),
        "--until",
        "2026-10-01",
    ]);
    let cancelled = ledger.add("JPY", "300", "2026-09-25");
    ledger.ok(&["pending", "cancel", pending_id(&cancelled)]);
    let before_list = ledger.ok(&[
        "pending",
        "list",
        "--status",
        "all",
        "--as-of",
        "2026-10-01",
    ]);
    let before_history = ledger.ok(&["pending", "history", pending_id(&added)]);
    let before_detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    let before_summary = ledger.ok(&["summary"]);
    let backup = ledger.directory.path().join("pending-backup.sqlite3");
    ledger.ok(&["backup", "--output", backup.to_str().unwrap()]);
    let restored_db = ledger.directory.path().join("restored.sqlite3");
    success(run_at(
        &restored_db,
        &["restore", "--input", backup.to_str().unwrap()],
        None,
    ));
    let restored = |args: &[&str]| success(run_at(&restored_db, args, None));
    assert_eq!(
        restored(&[
            "pending",
            "list",
            "--status",
            "all",
            "--as-of",
            "2026-10-01"
        ]),
        before_list
    );
    assert_eq!(
        restored(&["pending", "history", pending_id(&added)]),
        before_history
    );
    assert_eq!(
        restored(&["show", transaction_id(&confirmed)]),
        before_detail
    );
    assert_eq!(restored(&["summary"]), before_summary);
    assert_eq!(restored(&confirmation)["replayed"], true);
    assert_eq!(restored(&["list"])["total"], 1);

    let export = ledger.directory.path().join("pending-export.json");
    ledger.ok(&["export", "--output", export.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&fs::read(export).unwrap()).unwrap();
    let tables = exported["tables"].as_object().expect("export tables");
    let records: Vec<_> = tables
        .values()
        .filter_map(Value::as_array)
        .flat_map(|rows| rows.iter())
        .collect();
    for (item, status) in [
        (&added, "confirmed"),
        (&waiting, "pending"),
        (&cancelled, "cancelled"),
    ] {
        assert!(
            records
                .iter()
                .any(|row| row["id"] == pending_id(item) && row["status"] == status),
            "export must contain {status} pending record"
        );
    }
    let audit_after: Vec<Value> = records
        .iter()
        .filter_map(|row| row["after_json"].as_str())
        .map(|text| serde_json::from_str(text).expect("audit JSON"))
        .collect();
    assert!(
        audit_after
            .iter()
            .any(|row| row["id"] == pending_id(&added) && row["status"] == "pending")
    );
    assert!(
        audit_after
            .iter()
            .any(|row| row["id"] == pending_id(&added) && row["status"] == "confirmed")
    );
    assert!(
        records
            .iter()
            .any(|row| row["request_id"] == "backup-confirm")
    );
}
