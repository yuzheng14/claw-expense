use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;

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
        ledger
    }

    fn ok(&self, args: &[&str]) -> Value {
        success(run_at(&self.db, args, None))
    }

    fn error(&self, args: &[&str]) -> Value {
        failure(run_at(&self.db, args, None))
    }
}

fn run_at(db: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI");
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

fn transaction_id(value: &Value) -> &str {
    value["transaction"]["id"].as_str().expect("transaction ID")
}

fn pending_id(value: &Value) -> &str {
    value["pending"]["id"].as_str().expect("pending ID")
}

fn assert_occurrence(record: &Value, date: &str, occurred_at: Option<&str>) {
    assert_eq!(record["date"], date, "{record}");
    assert_eq!(
        record.get("occurred_at"),
        Some(&json!(occurred_at)),
        "{record}"
    );
}

#[test]
fn date_only_and_default_today_records_do_not_invent_a_time() {
    let ledger = Ledger::new();
    let expense = ledger.ok(&["add", "expense", "--amount", "98", "--date", "2026-09-24"]);
    assert_occurrence(&expense["transaction"], "2026-09-24", None);
    let refund = ledger.ok(&[
        "refund",
        transaction_id(&expense),
        "--amount",
        "100",
        "--date",
        "2026-09-25",
    ]);
    assert_occurrence(&refund["transaction"], "2026-09-25", None);
    let pending = ledger.ok(&[
        "pending",
        "add",
        "--currency",
        "JPY",
        "--amount",
        "10000",
        "--date",
        "2026-09-24",
    ]);
    assert_occurrence(&pending["pending"], "2026-09-24", None);
    let confirmed = ledger.ok(&[
        "pending",
        "confirm",
        pending_id(&pending),
        "--amount",
        "510.38",
    ]);
    assert_occurrence(&confirmed["transaction"], "2026-09-24", None);

    let before = Local::now().date_naive().to_string();
    let today = ledger.ok(&["add", "income", "--amount", "1"]);
    let waiting = ledger.ok(&["pending", "add", "--currency", "USD", "--amount", "20"]);
    let after = Local::now().date_naive().to_string();
    for record in [&today["transaction"], &waiting["pending"]] {
        assert_eq!(record.get("occurred_at"), Some(&Value::Null));
        assert!(record["date"] == before || record["date"] == after);
    }
}

#[test]
fn minute_precision_and_nanoseconds_are_preserved_without_using_creation_time() {
    let ledger = Ledger::new();
    let start = Utc::now();
    let minute = "2020-02-29T09:30+08:00";
    let expense = ledger.ok(&["add", "expense", "--amount", "98", "--occurred-at", minute]);
    assert_occurrence(&expense["transaction"], "2020-02-29", Some(minute));
    let detailed = ledger.ok(&["show", transaction_id(&expense)]);
    assert_occurrence(&detailed["transaction"], "2020-02-29", Some(minute));
    let fractional = "2020-02-29T23:59:59.123456789-05:00";
    let income = ledger.ok(&[
        "add",
        "income",
        "--amount",
        "100",
        "--date",
        "2020-02-29",
        "--occurred-at",
        fractional,
    ]);
    assert_occurrence(&income["transaction"], "2020-02-29", Some(fractional));
    let end = Utc::now();
    for record in [&expense["transaction"], &income["transaction"]] {
        let created = DateTime::parse_from_rfc3339(record["created_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            created >= start && created <= end,
            "created_at must be system creation time: {record}"
        );
        assert_ne!(record["created_at"], record["occurred_at"]);
    }
}

#[test]
fn local_offset_date_controls_month_totals_pending_reminders_and_refunds() {
    let ledger = Ledger::new();
    // This instant is still September in UTC, but the purchase happened on October 1 locally.
    let purchase_time = "2026-10-01T00:15+14:00";
    let pending = ledger.ok(&[
        "pending",
        "add",
        "--currency",
        "TWD",
        "--amount",
        "1234.56",
        "--occurred-at",
        purchase_time,
    ]);
    assert_occurrence(&pending["pending"], "2026-10-01", Some(purchase_time));
    assert_eq!(pending["pending"]["remind_on"], "2026-10-04");
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-09"])["pending_count"],
        0
    );
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-10"])["pending_count"],
        1
    );
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-10-03"])["total"],
        0
    );
    assert_eq!(
        ledger.ok(&["pending", "due", "--as-of", "2026-10-04"])["total"],
        1
    );

    let start = Utc::now();
    let confirmed = ledger.ok(&[
        "pending",
        "confirm",
        pending_id(&pending),
        "--amount",
        "278.91",
        "--posted-date",
        "2026-10-05",
    ]);
    let end = Utc::now();
    assert_occurrence(&confirmed["pending"], "2026-10-01", Some(purchase_time));
    assert_occurrence(&confirmed["transaction"], "2026-10-01", Some(purchase_time));
    let confirmed_at =
        DateTime::parse_from_rfc3339(confirmed["pending"]["confirmed_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
    assert!(confirmed_at >= start && confirmed_at <= end);
    assert_eq!(confirmed["pending"]["posted_date"], "2026-10-05");
    let detail = ledger.ok(&["show", transaction_id(&confirmed)]);
    assert_occurrence(
        &detail["foreign_expense"],
        "2026-10-01",
        Some(purchase_time),
    );

    // Refund date follows its own original offset, not the expense month or UTC month.
    let refund_time = "2026-10-31T23:45:12-07:00";
    let refund = ledger.ok(&[
        "refund",
        transaction_id(&confirmed),
        "--amount",
        "280",
        "--occurred-at",
        refund_time,
    ]);
    assert_occurrence(&refund["transaction"], "2026-10-31", Some(refund_time));
    let october = ledger.ok(&["summary", "--month", "2026-10"]);
    assert_eq!(october["expense"], "278.91");
    assert_eq!(october["refund"], "280.00");
    assert_eq!(october["net_expense"], "-1.09");
    assert_eq!(october["pending_count"], 0);
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-11"])["refund"],
        "0.00"
    );
}

#[test]
fn malformed_or_ambiguous_timestamps_and_mismatched_dates_never_write() {
    let ledger = Ledger::new();
    for time in [
        "2026-09-24T09:30",
        "2026-09-24T09:30:00",
        "2026-09-24T09:30-00:00",
        "2026-02-30T09:30+08:00",
        "2026-09-24T24:00+08:00",
        "2026-09-24T23:59:60+08:00",
        "2026-09-24T09:30:00.1234567890+08:00",
        "2026-09-24T09:30+24:00",
    ] {
        ledger.error(&["add", "expense", "--amount", "1", "--occurred-at", time]);
        ledger.error(&[
            "pending",
            "add",
            "--currency",
            "USD",
            "--amount",
            "1",
            "--occurred-at",
            time,
        ]);
    }
    for args in [
        &[
            "add",
            "income",
            "--amount",
            "1",
            "--date",
            "2026-09-30",
            "--occurred-at",
            "2026-10-01T00:15+14:00",
        ][..],
        &[
            "pending",
            "add",
            "--currency",
            "JPY",
            "--amount",
            "1",
            "--date",
            "2026-09-30",
            "--occurred-at",
            "2026-10-01T00:15+14:00",
        ][..],
    ] {
        ledger.error(args);
    }
    assert_eq!(ledger.ok(&["list"])["total"], 0);
    assert_eq!(ledger.ok(&["pending", "list"])["total"], 0);
}

#[test]
fn json_input_keeps_date_required_accepts_null_and_checks_occurrence_type_and_date() {
    let ledger = Ledger::new();
    let time = "2026-09-24T09:30+08:00";
    let expense = json!({"kind":"expense", "amount":"98", "date":"2026-09-24", "occurred_at":time});
    let added = success(run_at(&ledger.db, &["record"], Some(&expense.to_string())));
    assert_occurrence(&added["transaction"], "2026-09-24", Some(time));
    let pending =
        json!({"currency":"JPY", "amount":"10000", "date":"2026-09-24", "occurred_at":time});
    let waiting = success(run_at(
        &ledger.db,
        &["pending", "record"],
        Some(&pending.to_string()),
    ));
    assert_occurrence(&waiting["pending"], "2026-09-24", Some(time));
    for (command, template, field) in [
        (&["record"][..], expense, "transaction"),
        (&["pending", "record"][..], pending, "pending"),
    ] {
        let mut no_date = template.clone();
        no_date.as_object_mut().unwrap().remove("date");
        failure(run_at(&ledger.db, command, Some(&no_date.to_string())));
        let mut wrong_date = template.clone();
        wrong_date["date"] = json!("2026-09-23");
        failure(run_at(&ledger.db, command, Some(&wrong_date.to_string())));
        for invalid in [json!(1234), json!(true), json!("2026-09-24T09:30")] {
            let mut input = template.clone();
            input["occurred_at"] = invalid;
            failure(run_at(&ledger.db, command, Some(&input.to_string())));
        }
        let mut with_null = template;
        with_null["occurred_at"] = Value::Null;
        let accepted = success(run_at(&ledger.db, command, Some(&with_null.to_string())));
        assert_occurrence(&accepted[field], "2026-09-24", None);
    }
    assert_eq!(ledger.ok(&["list"])["total"], 2);
    assert_eq!(ledger.ok(&["pending", "list"])["total"], 2);
}

#[test]
fn editing_time_derives_date_and_clearing_time_is_explicit_and_audited() {
    let ledger = Ledger::new();
    let added = ledger.ok(&["add", "expense", "--amount", "98", "--date", "2026-09-24"]);
    let id = transaction_id(&added);
    let time = "2026-10-01T00:15+14:00";
    let args = [
        "edit",
        id,
        "--occurred-at",
        time,
        "--request-id",
        "set-occurrence",
    ];
    let updated = ledger.ok(&args);
    assert_occurrence(&updated["transaction"], "2026-10-01", Some(time));
    assert_eq!(ledger.ok(&args)["replayed"], true);
    ledger.error(&["edit", id, "--date", "2026-10-02"]);
    ledger.error(&["edit", id, "--occurred-at", time, "--clear-occurred-at"]);
    ledger.error(&["edit", id, "--date", "2026-09-30", "--occurred-at", time]);
    assert_occurrence(
        &ledger.ok(&["show", id])["transaction"],
        "2026-10-01",
        Some(time),
    );
    let history = ledger.ok(&["history", id]);
    let events = history.as_array().unwrap();
    assert_eq!(
        events.len(),
        2,
        "errors and request replay must not create audit events"
    );
    assert_occurrence(&events[1]["before"], "2026-09-24", None);
    assert_occurrence(&events[1]["after"], "2026-10-01", Some(time));

    let clear_args = [
        "edit",
        id,
        "--clear-occurred-at",
        "--request-id",
        "clear-occurrence",
    ];
    let cleared = ledger.ok(&clear_args);
    assert_occurrence(&cleared["transaction"], "2026-10-01", None);
    assert_eq!(ledger.ok(&clear_args)["replayed"], true);
    let reset = ledger.ok(&["edit", id, "--occurred-at", "2026-10-03T18:01:02+08:00"]);
    assert_occurrence(
        &reset["transaction"],
        "2026-10-03",
        Some("2026-10-03T18:01:02+08:00"),
    );
    let clear_with_date = ledger.ok(&["edit", id, "--clear-occurred-at", "--date", "2026-10-02"]);
    assert_occurrence(&clear_with_date["transaction"], "2026-10-02", None);
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-09"])["expense"],
        "0.00"
    );
    assert_eq!(
        ledger.ok(&["summary", "--month", "2026-10"])["expense"],
        "98.00"
    );
}

#[test]
fn timestamps_participate_in_idempotency_and_equivalent_utc_spellings_replay() {
    let ledger = Ledger::new();
    for (prefix, response_key, request_id) in [
        (
            &["add", "expense", "--amount", "98"][..],
            "transaction",
            "utc-expense",
        ),
        (
            &["pending", "add", "--currency", "USD", "--amount", "20"][..],
            "pending",
            "utc-pending",
        ),
    ] {
        let mut args = prefix.to_vec();
        args.extend([
            "--occurred-at",
            "2026-09-24T09:30Z",
            "--request-id",
            request_id,
        ]);
        let added = ledger.ok(&args);
        let mut equivalent = prefix.to_vec();
        equivalent.extend([
            "--occurred-at",
            "2026-09-24T09:30+00:00",
            "--request-id",
            request_id,
        ]);
        let replayed = ledger.ok(&equivalent);
        assert_eq!(replayed["replayed"], true);
        assert_eq!(replayed[response_key], added[response_key]);
        let mut different = prefix.to_vec();
        different.extend([
            "--occurred-at",
            "2026-09-24T09:31Z",
            "--request-id",
            request_id,
        ]);
        assert_eq!(ledger.error(&different)["code"], "IDEMPOTENCY_CONFLICT");
    }
    assert_eq!(ledger.ok(&["list"])["total"], 1);
    assert_eq!(ledger.ok(&["pending", "list"])["total"], 1);
}

#[test]
fn human_lists_and_details_display_recorded_offset_and_precision() {
    let ledger = Ledger::new();
    let time = "2026-09-24T09:30+08:00";
    let expense = ledger.ok(&["add", "expense", "--amount", "98", "--occurred-at", time]);
    let pending = ledger.ok(&[
        "pending",
        "add",
        "--currency",
        "JPY",
        "--amount",
        "10000",
        "--occurred-at",
        time,
    ]);
    for args in [
        &["list"][..],
        &["show", transaction_id(&expense)][..],
        &["pending", "list"][..],
        &["pending", "show", pending_id(&pending)][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
            .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "1")
            .arg("--db")
            .arg(&ledger.db)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        let rendered = String::from_utf8(output.stdout).unwrap();
        assert!(
            rendered.contains(time),
            "{args:?} omitted the recorded offset/precision: {rendered}"
        );
        assert!(
            !rendered.contains("2026-09-24T09:30:00+08:00"),
            "must not invent seconds: {rendered}"
        );
    }
}

#[test]
fn occurrence_precision_survives_backup_restore_export_and_idempotency_snapshots() {
    let ledger = Ledger::new();
    let time = "2026-09-24T09:30:12.123456789+08:00";
    let create = [
        "pending",
        "add",
        "--currency",
        "JPY",
        "--amount",
        "10000",
        "--occurred-at",
        time,
        "--request-id",
        "archive-purchase-time",
    ];
    let added = ledger.ok(&create);
    let confirm = [
        "pending",
        "confirm",
        pending_id(&added),
        "--amount",
        "510.38",
        "--request-id",
        "archive-confirm-time",
    ];
    let confirmed = ledger.ok(&confirm);
    let date_only = ledger.ok(&["add", "income", "--amount", "1000", "--date", "2026-09-24"]);
    let transaction = ledger.ok(&["show", transaction_id(&confirmed)]);
    let pending = ledger.ok(&["pending", "show", pending_id(&added)]);
    let history = ledger.ok(&["pending", "history", pending_id(&added)]);
    let backup = ledger.directory.path().join("occurrence-backup.sqlite3");
    ledger.ok(&["backup", "--output", backup.to_str().unwrap()]);
    let restored_db = ledger.directory.path().join("occurrence-restored.sqlite3");
    success(run_at(
        &restored_db,
        &["restore", "--input", backup.to_str().unwrap()],
        None,
    ));
    let restored = |args: &[&str]| success(run_at(&restored_db, args, None));
    assert_eq!(restored(&["show", transaction_id(&confirmed)]), transaction);
    assert_eq!(restored(&["pending", "show", pending_id(&added)]), pending);
    assert_eq!(
        restored(&["pending", "history", pending_id(&added)]),
        history
    );
    assert_eq!(restored(&create)["replayed"], true);
    assert_eq!(restored(&confirm)["replayed"], true);
    assert_eq!(restored(&["list"])["total"], 2);

    let export = ledger.directory.path().join("occurrence-export.json");
    restored(&["export", "--output", export.to_str().unwrap()]);
    let exported: Value = serde_json::from_slice(&fs::read(export).unwrap()).unwrap();
    let transactions = exported["tables"]["transactions"].as_array().unwrap();
    let expense = transactions
        .iter()
        .find(|row| row["id"] == transaction_id(&confirmed))
        .unwrap();
    assert_occurrence(expense, "2026-09-24", Some(time));
    let income = transactions
        .iter()
        .find(|row| row["id"] == transaction_id(&date_only))
        .unwrap();
    assert_occurrence(income, "2026-09-24", None);
    let original = &exported["tables"]["pending_expenses"][0];
    assert_occurrence(original, "2026-09-24", Some(time));
    for event in exported["tables"]["pending_audit_log"].as_array().unwrap() {
        let after: Value = serde_json::from_str(event["after_json"].as_str().unwrap()).unwrap();
        assert_occurrence(&after, "2026-09-24", Some(time));
    }
    let requests = exported["tables"]["idempotency"].as_array().unwrap();
    let confirmation = requests
        .iter()
        .find(|row| row["request_id"] == "archive-confirm-time")
        .unwrap();
    let response: Value =
        serde_json::from_str(confirmation["response_json"].as_str().unwrap()).unwrap();
    assert_occurrence(&response["pending"], "2026-09-24", Some(time));
    assert_occurrence(&response["transaction"], "2026-09-24", Some(time));
}
