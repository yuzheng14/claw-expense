//! Foreign-card purchases awaiting an exact, bank-confirmed CNY amount.
//! Pending amounts never participate in CNY totals or use floating-point math.
use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};
use futures_util::TryStreamExt;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, sqlite::SqliteRow};

use crate::{
    error::{AppError, Result},
    models::*,
    money::Money,
    occurrence::validate_occurrence,
    store::{
        Store, audit, checked_add, clean_optional, fetch_record, now, require_category,
        save_request, validate_amount, validate_date, validate_filters, validate_request_id,
    },
};

pub const SUPPORTED_CURRENCIES: &[(&str, u32)] = &[
    ("USD", 2),
    ("JPY", 0),
    ("EUR", 2),
    ("GBP", 2),
    ("HKD", 2),
    ("SGD", 2),
    ("AUD", 2),
    ("CAD", 2),
    ("CHF", 2),
    ("NZD", 2),
    ("KRW", 0),
    ("TWD", 2),
];

/// Supported ISO 4217 minor-unit precision. Deliberately rejects CNY and unknown codes.
pub fn currency_digits(currency: &str) -> Result<u32> {
    SUPPORTED_CURRENCIES
        .iter()
        .find(|(code, _)| *code == currency)
        .map(|(_, digits)| *digits)
        .ok_or_else(|| {
            AppError::invalid(
                "外币必须为 USD、JPY、EUR、GBP、HKD、SGD、AUD、CAD、CHF、NZD、KRW 或 TWD（大写）",
            )
        })
}

pub fn parse_foreign_minor(currency: &str, amount: &str) -> Result<i64> {
    let digits = currency_digits(currency)?;
    let minor = if digits == 2 {
        amount.parse::<Money>()?.minor()
    } else {
        if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(AppError::invalid("JPY/KRW 金额必须为无小数的正整数字符串"));
        }
        amount.bytes().try_fold(0_i64, |value, byte| {
            value
                .checked_mul(10)
                .and_then(|value| value.checked_add(i64::from(byte - b'0')))
                .ok_or_else(|| AppError::invalid("原币金额超出可表示范围"))
        })?
    };
    if minor <= 0 {
        return Err(AppError::invalid("原币金额必须大于零"));
    }
    Ok(minor)
}

pub fn format_foreign_minor(currency: &str, amount: i128) -> Result<String> {
    Ok(if currency_digits(currency)? == 0 {
        amount.to_string()
    } else {
        crate::money::format_minor(amount)
    })
}

impl Store {
    pub async fn add_pending(
        &self,
        mut input: NewPendingExpense,
        request_id: Option<&str>,
    ) -> Result<PendingWriteResult> {
        let minor = parse_foreign_minor(&input.currency, &input.amount)?;
        input.amount = format_foreign_minor(&input.currency, i128::from(minor))?;
        input.occurred_at = validate_occurrence(&input.date, input.occurred_at.as_deref())?;
        validate_request_id(request_id)?;
        let remind_on = NaiveDate::parse_from_str(&input.date, "%Y-%m-%d")
            .expect("validated date")
            .checked_add_days(Days::new(3))
            .ok_or_else(|| AppError::invalid("消费日期过晚，无法生成提醒日期"))?
            .format("%Y-%m-%d")
            .to_string();
        validate_date(&remind_on)?;
        let payload = serde_json::to_string(
            &serde_json::json!({"operation": "pending.add", "input": input}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let category = input.category.as_deref().unwrap_or("其他支出");
        require_category(&mut tx, category, Kind::Expense).await?;
        let id = format!("pending_{}", uuid::Uuid::new_v4().simple());
        let now = now();
        sqlx::query("INSERT INTO pending_expenses (id, currency, amount_minor, date, occurred_at, category, merchant, note, channel, remind_on, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id).bind(&input.currency).bind(minor).bind(&input.date).bind(&input.occurred_at).bind(category)
            .bind(clean_optional(input.merchant)).bind(clean_optional(input.note))
            .bind(clean_optional(input.channel)).bind(remind_on).bind(&now).bind(&now)
            .execute(&mut *tx).await?;
        let pending = fetch_pending(&mut tx, &id).await?;
        pending_audit(&mut tx, "create", None, &pending).await?;
        let result = PendingWriteResult {
            pending,
            transaction: None,
            replayed: false,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn confirm_pending(
        &self,
        id: &str,
        input: ConfirmPendingExpense,
        request_id: Option<&str>,
    ) -> Result<PendingWriteResult> {
        validate_amount(input.amount)?;
        validate_request_id(request_id)?;
        if let Some(date) = &input.posted_date {
            validate_date(date)?;
        }
        let payload = serde_json::to_string(
            &serde_json::json!({"operation": "pending.confirm", "id": id, "input": input}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let before = fetch_pending(&mut tx, id).await?;
        if input
            .posted_date
            .as_ref()
            .is_some_and(|date| date < &before.date)
        {
            return Err(AppError::invalid("银行入账日期不能早于消费日期"));
        }
        if before.status == "confirmed" {
            let confirmed_minor: i64 = sqlx::query_scalar(
                "SELECT confirmed_amount_minor FROM pending_expenses WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if confirmed_minor != input.amount.minor() || before.posted_date != input.posted_date {
                return Err(AppError::new(
                    "ALREADY_CONFIRMED",
                    "此待确认账单已确认；请通过 edit 修改关联的人民币支出",
                ));
            }
            // Return the current expense (including later edits/voids), not its old snapshot.
            let transaction = linked_transaction(&mut tx, &before).await?;
            let result = PendingWriteResult {
                pending: before,
                transaction,
                replayed: true,
            };
            save_request(&mut tx, request_id, &payload, &result).await?;
            tx.commit().await?;
            return Ok(result);
        }
        require_pending(&before)?;
        require_category(&mut tx, &before.category, Kind::Expense).await?;
        let transaction_id = format!("txn_{}", uuid::Uuid::new_v4().simple());
        let now = now();
        sqlx::query("INSERT INTO transactions (id, kind, amount_minor, category, date, occurred_at, note, channel, created_at, updated_at) VALUES (?, 'expense', ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&transaction_id).bind(input.amount.minor()).bind(&before.category).bind(&before.date)
            .bind(&before.occurred_at).bind(&before.note).bind(&before.channel).bind(&now).bind(&now)
            .execute(&mut *tx).await?;
        let transaction = fetch_record(&mut tx, &transaction_id).await?;
        audit(&mut tx, "create", None, &transaction).await?;
        sqlx::query("UPDATE pending_expenses SET status = 'confirmed', transaction_id = ?, confirmed_amount_minor = ?, confirmed_at = ?, posted_date = ?, updated_at = ? WHERE id = ?")
            .bind(&transaction_id).bind(input.amount.minor()).bind(&now).bind(input.posted_date)
            .bind(&now).bind(id).execute(&mut *tx).await?;
        let pending = fetch_pending(&mut tx, id).await?;
        pending_audit(&mut tx, "confirm", Some(&before), &pending).await?;
        let result = PendingWriteResult {
            pending,
            transaction: Some(transaction),
            replayed: false,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn cancel_pending(
        &self,
        id: &str,
        request_id: Option<&str>,
    ) -> Result<PendingWriteResult> {
        self.change_pending(id, None, request_id).await
    }

    pub async fn snooze_pending(
        &self,
        id: &str,
        until: &str,
        request_id: Option<&str>,
    ) -> Result<PendingWriteResult> {
        validate_date(until)?;
        self.change_pending(id, Some(until), request_id).await
    }

    async fn change_pending(
        &self,
        id: &str,
        until: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<PendingWriteResult> {
        validate_request_id(request_id)?;
        let action = if until.is_some() { "snooze" } else { "cancel" };
        let payload = serde_json::to_string(
            &serde_json::json!({"operation": format!("pending.{action}"), "id": id, "until": until}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let before = fetch_pending(&mut tx, id).await?;
        if before.status == "cancelled" && until.is_none() {
            let result = PendingWriteResult {
                pending: before,
                transaction: None,
                replayed: true,
            };
            save_request(&mut tx, request_id, &payload, &result).await?;
            tx.commit().await?;
            return Ok(result);
        }
        require_pending(&before)?;
        if let Some(until) = until {
            if until < before.date.as_str() {
                return Err(AppError::invalid("提醒日期不能早于消费日期"));
            }
            if until < before.remind_on.as_str() {
                return Err(AppError::invalid("延后提醒不能早于当前提醒日期"));
            }
            sqlx::query("UPDATE pending_expenses SET remind_on = ?, updated_at = ? WHERE id = ?")
                .bind(until)
                .bind(now())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query(
                "UPDATE pending_expenses SET status = 'cancelled', updated_at = ? WHERE id = ?",
            )
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        let pending = fetch_pending(&mut tx, id).await?;
        pending_audit(&mut tx, action, Some(&before), &pending).await?;
        let result = PendingWriteResult {
            pending,
            transaction: None,
            replayed: false,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_pending(&self, id: &str) -> Result<PendingDetail> {
        let mut tx = self.pool.begin().await?;
        let pending = fetch_pending(&mut tx, id).await?;
        let transaction = linked_transaction(&mut tx, &pending).await?;
        tx.commit().await?;
        Ok(PendingDetail {
            pending,
            transaction,
        })
    }

    pub async fn list_pending(&self, filters: &PendingFilters) -> Result<PendingListResult> {
        validate_filters(&filters.filters)?;
        validate_date(&filters.as_of)?;
        if let Some(currency) = &filters.currency {
            currency_digits(currency)?;
        }
        let status = filters.status.as_deref().unwrap_or("pending");
        if !["pending", "confirmed", "cancelled", "all"].contains(&status) {
            return Err(AppError::invalid(
                "status 必须为 pending、confirmed、cancelled 或 all",
            ));
        }
        let limit = if filters.filters.limit == 0 {
            50
        } else {
            filters.filters.limit
        };
        if !(1..=1000).contains(&limit) || filters.filters.offset < 0 {
            return Err(AppError::invalid(
                "limit 必须为 1..1000，offset 必须大于等于 0",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let mut count_query =
            QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM pending_expenses p");
        push_pending_filters(&mut count_query, &filters.filters, status);
        push_list_filters(&mut count_query, filters);
        let total: i64 = count_query.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT p.* FROM pending_expenses p");
        push_pending_filters(&mut query, &filters.filters, status);
        push_list_filters(&mut query, filters);
        query
            .push(" ORDER BY p.date, p.id LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(filters.filters.offset);
        let items = query
            .build()
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(decode_pending)
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(PendingListResult {
            items,
            total,
            limit,
            offset: filters.filters.offset,
            as_of: filters.as_of.clone(),
        })
    }

    pub async fn pending_history(&self, id: &str) -> Result<Vec<PendingAuditRecord>> {
        let mut tx = self.pool.begin().await?;
        fetch_pending(&mut tx, id).await?;
        let rows = sqlx::query("SELECT * FROM pending_audit_log WHERE pending_id = ? ORDER BY id")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        let records = rows
            .iter()
            .map(|row| {
                Ok(PendingAuditRecord {
                    id: row.try_get("id")?,
                    pending_id: row.try_get("pending_id")?,
                    action: row.try_get("action")?,
                    before: row
                        .try_get::<Option<String>, _>("before_json")?
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                    after: serde_json::from_str(&row.try_get::<String, _>("after_json")?)?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(records)
    }
}

fn require_pending(record: &PendingExpenseRecord) -> Result<()> {
    match record.status.as_str() {
        "pending" => Ok(()),
        "confirmed" => Err(AppError::new(
            "ALREADY_CONFIRMED",
            "此账单已确认，请操作关联的人民币支出",
        )),
        _ => Err(AppError::new("PENDING_CANCELLED", "此待确认账单已取消")),
    }
}

fn decode_pending(row: &SqliteRow) -> Result<PendingExpenseRecord> {
    let currency: String = row.try_get("currency")?;
    let amount = format_foreign_minor(
        &currency,
        i128::from(row.try_get::<i64, _>("amount_minor")?),
    )?;
    Ok(PendingExpenseRecord {
        id: row.try_get("id")?,
        currency,
        amount,
        date: row.try_get("date")?,
        occurred_at: row.try_get("occurred_at")?,
        category: row.try_get("category")?,
        merchant: row.try_get("merchant")?,
        note: row.try_get("note")?,
        channel: row.try_get("channel")?,
        status: row.try_get("status")?,
        remind_on: row.try_get("remind_on")?,
        transaction_id: row.try_get("transaction_id")?,
        confirmed_at: row.try_get("confirmed_at")?,
        posted_date: row.try_get("posted_date")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn fetch_pending(conn: &mut SqliteConnection, id: &str) -> Result<PendingExpenseRecord> {
    let row = sqlx::query("SELECT * FROM pending_expenses WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| AppError::new("NOT_FOUND", format!("待确认账单不存在：{id}")))?;
    decode_pending(&row)
}

pub(crate) async fn find_by_transaction(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<PendingExpenseRecord>> {
    sqlx::query("SELECT * FROM pending_expenses WHERE transaction_id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .as_ref()
        .map(decode_pending)
        .transpose()
}

async fn linked_transaction(
    conn: &mut SqliteConnection,
    record: &PendingExpenseRecord,
) -> Result<Option<TransactionRecord>> {
    match &record.transaction_id {
        Some(id) => Ok(Some(fetch_record(conn, id).await?)),
        None => Ok(None),
    }
}

async fn pending_audit(
    conn: &mut SqliteConnection,
    action: &str,
    before: Option<&PendingExpenseRecord>,
    after: &PendingExpenseRecord,
) -> Result<()> {
    sqlx::query("INSERT INTO pending_audit_log (pending_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&after.id).bind(action).bind(before.map(serde_json::to_string).transpose()?)
        .bind(serde_json::to_string(after)?).bind(now()).execute(conn).await?;
    Ok(())
}

async fn replay(
    conn: &mut SqliteConnection,
    request_id: Option<&str>,
    payload: &str,
) -> Result<Option<PendingWriteResult>> {
    let Some(id) = request_id else {
        return Ok(None);
    };
    let Some(row) =
        sqlx::query("SELECT payload, response_json FROM idempotency WHERE request_id = ?")
            .bind(id)
            .fetch_optional(conn)
            .await?
    else {
        return Ok(None);
    };
    if row.try_get::<&str, _>("payload")? != payload {
        return Err(AppError::new(
            "IDEMPOTENCY_CONFLICT",
            "此 request-id 已用于不同请求",
        ));
    }
    let mut result: PendingWriteResult = serde_json::from_str(row.try_get("response_json")?)?;
    result.replayed = true;
    Ok(Some(result))
}

fn push_pending_filters(query: &mut QueryBuilder<Sqlite>, filters: &Filters, status: &str) {
    query.push(" WHERE 1 = 1");
    if status != "all" {
        query.push(" AND p.status = ").push_bind(status.to_owned());
    }
    if filters.kind.is_some_and(|kind| kind != Kind::Expense) {
        query.push(" AND 0 = 1");
    }
    if let Some(month) = &filters.month {
        query.push(" AND substr(p.date, 1, 7) = ").push_bind(month);
    }
    if let Some(from) = &filters.from {
        query.push(" AND p.date >= ").push_bind(from);
    }
    if let Some(to) = &filters.to {
        query.push(" AND p.date <= ").push_bind(to);
    }
    if let Some(category) = &filters.category {
        query.push(" AND p.category = ").push_bind(category);
    }
    if let Some(keyword) = &filters.keyword {
        query
            .push(" AND (instr(COALESCE(p.note, ''), ")
            .push_bind(keyword)
            .push(") > 0 OR instr(p.id, ")
            .push_bind(keyword)
            .push(") > 0 OR instr(COALESCE(p.channel, ''), ")
            .push_bind(keyword)
            .push(") > 0 OR instr(COALESCE(p.merchant, ''), ")
            .push_bind(keyword)
            .push(") > 0)");
    }
}

fn push_list_filters(query: &mut QueryBuilder<Sqlite>, filters: &PendingFilters) {
    if let Some(currency) = &filters.currency {
        query.push(" AND p.currency = ").push_bind(currency);
    }
    if filters.due_only {
        query
            .push(" AND p.status = 'pending' AND p.remind_on <= ")
            .push_bind(&filters.as_of);
    }
}

pub(crate) async fn pending_totals(
    conn: &mut SqliteConnection,
    filters: &Filters,
) -> Result<(i64, Vec<PendingCurrencySummary>)> {
    let mut query =
        QueryBuilder::<Sqlite>::new("SELECT p.currency, p.amount_minor FROM pending_expenses p");
    push_pending_filters(&mut query, filters, "pending");
    let mut rows = query.build().fetch(conn);
    let mut count = 0_i64;
    let mut totals: BTreeMap<String, (i128, i64)> = BTreeMap::new();
    while let Some(row) = rows.try_next().await? {
        let currency: String = row.try_get("currency")?;
        let (amount, entries) = totals.entry(currency).or_default();
        *amount = checked_add(*amount, i128::from(row.try_get::<i64, _>("amount_minor")?))?;
        *entries = entries
            .checked_add(1)
            .ok_or_else(|| AppError::new("AMOUNT_OVERFLOW", "账单数量溢出"))?;
        count = count
            .checked_add(1)
            .ok_or_else(|| AppError::new("AMOUNT_OVERFLOW", "账单数量溢出"))?;
    }
    let values = totals
        .into_iter()
        .map(|(currency, (amount, count))| {
            Ok(PendingCurrencySummary {
                amount: format_foreign_minor(&currency, amount)?,
                currency,
                count,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((count, values))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("ledger.sqlite3"), true)
            .await
            .unwrap();
        (dir, store)
    }

    fn input(currency: &str, amount: &str) -> NewPendingExpense {
        NewPendingExpense {
            currency: currency.into(),
            amount: amount.into(),
            date: "2026-09-29".into(),
            occurred_at: None,
            category: Some("购物".into()),
            merchant: Some("乐天".into()),
            note: Some("海淘".into()),
            channel: Some("信用卡".into()),
        }
    }

    fn confirm(amount: &str) -> ConfirmPendingExpense {
        ConfirmPendingExpense {
            amount: amount.parse().unwrap(),
            posted_date: Some("2026-10-03".into()),
        }
    }

    fn pending_filters(as_of: &str) -> PendingFilters {
        PendingFilters {
            filters: Filters::default(),
            status: None,
            currency: None,
            as_of: as_of.into(),
            due_only: false,
        }
    }

    #[tokio::test]
    async fn pending_occurrence_survives_confirmation_without_moving_date_or_reminder() {
        let (_dir, store) = ledger().await;
        let mut data = input("USD", "20");
        data.date = "2026-09-30".into();
        data.occurred_at = Some("2026-09-30T23:30:12.123-07:00".into());
        let pending = store
            .add_pending(data.clone(), Some("timed-pending"))
            .await
            .unwrap()
            .pending;
        assert_eq!(pending.remind_on, "2026-10-03");
        assert_eq!(pending.occurred_at, data.occurred_at);
        assert_eq!(
            store
                .list_pending(&pending_filters("2026-10-03"))
                .await
                .unwrap()
                .items[0]
                .occurred_at,
            data.occurred_at
        );
        let confirmed = store
            .confirm_pending(&pending.id, confirm("140"), Some("confirm-time"))
            .await
            .unwrap();
        let txn = confirmed.transaction.unwrap();
        assert_eq!(txn.date, "2026-09-30");
        assert_eq!(txn.occurred_at, data.occurred_at);
        assert_eq!(
            store
                .get(&txn.id)
                .await
                .unwrap()
                .foreign_expense
                .unwrap()
                .occurred_at,
            data.occurred_at
        );
        assert_eq!(
            store.pending_history(&pending.id).await.unwrap()[1].after["occurred_at"],
            "2026-09-30T23:30:12.123-07:00"
        );
        assert_eq!(
            store
                .summary(&Filters {
                    month: Some("2026-09".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .expense,
            "140.00"
        );
        assert_eq!(
            store
                .summary(&Filters {
                    month: Some("2026-10".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .expense,
            "0.00"
        );
        data.date = "2026-10-01".into();
        assert!(store.add_pending(data, None).await.is_err());
    }

    #[tokio::test]
    async fn pending_old_payloads_and_snapshots_remain_compatible() {
        let (_dir, store) = ledger().await;
        let old = input("USD", "20");
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({
                "currency": "USD", "amount": "20", "date": "2026-09-29", "category": "购物",
                "merchant": "乐天", "note": "海淘", "channel": "信用卡"
            })
        );
        let pending = store
            .add_pending(old.clone(), Some("legacy-pending"))
            .await
            .unwrap()
            .pending;
        sqlx::query("UPDATE idempotency SET response_json = json_remove(response_json, '$.pending.occurred_at')")
            .execute(&store.pool).await.unwrap();
        let replay = store
            .add_pending(old, Some("legacy-pending"))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert!(replay.pending.occurred_at.is_none());
        assert!(
            store
                .confirm_pending(&pending.id, confirm("140"), None)
                .await
                .unwrap()
                .transaction
                .unwrap()
                .occurred_at
                .is_none()
        );
        let mut utc = input("USD", "20");
        utc.occurred_at = Some("2026-09-29T10:30+00:00".into());
        let first = store
            .add_pending(utc.clone(), Some("canonical-time"))
            .await
            .unwrap();
        utc.occurred_at = Some("2026-09-29T10:30Z".into());
        let replay = store
            .add_pending(utc, Some("canonical-time"))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.pending.id, first.pending.id);
        assert_eq!(
            replay.pending.occurred_at.as_deref(),
            Some("2026-09-29T10:30Z")
        );
        assert!(sqlx::query("UPDATE pending_expenses SET occurred_at = '2026-10-01T12:00Z' WHERE status = 'pending'").execute(&store.pool).await.is_err());
    }

    #[test]
    fn foreign_money_is_exact_and_currency_specific() {
        for (currency, input, expected) in [
            ("USD", "0.10", 10),
            ("USD", "90071992547409.93", 9_007_199_254_740_993),
            ("USD", "92233720368547758.07", i64::MAX),
            ("TWD", "1234.56", 123456),
            ("TWD", "0.01", 1),
            ("TWD", "92233720368547758.07", i64::MAX),
            ("JPY", "9223372036854775807", i64::MAX),
            ("KRW", "1000", 1000),
            ("JPY", "00100", 100),
        ] {
            assert_eq!(parse_foreign_minor(currency, input).unwrap(), expected);
        }
        for (currency, input) in [
            ("JPY", "1.0"),
            ("KRW", "0.01"),
            ("JPY", "1e3"),
            ("JPY", "-1"),
            ("JPY", "+1"),
            ("JPY", " 1"),
            ("JPY", "1,000"),
            ("JPY", "1_000"),
            ("JPY", "１"),
            ("JPY", "0"),
            ("USD", "0.00"),
            ("USD", "1.001"),
            ("USD", "1e2"),
            ("JPY", "9223372036854775808"),
            ("USD", "92233720368547758.08"),
            ("TWD", "1.001"),
            ("TWD", "0"),
            ("TWD", "1e2"),
            ("TWD", "92233720368547758.08"),
            ("CNY", "1"),
            ("BTC", "1"),
            ("usd", "1"),
        ] {
            assert!(
                parse_foreign_minor(currency, input).is_err(),
                "accepted {currency} {input}"
            );
        }
        assert_eq!(
            format_foreign_minor("JPY", i128::MAX).unwrap(),
            i128::MAX.to_string()
        );
        for (currency, precision) in SUPPORTED_CURRENCIES {
            assert_eq!(currency_digits(currency).unwrap(), *precision);
            assert_eq!(
                parse_foreign_minor(currency, "1").unwrap(),
                if *precision == 0 { 1 } else { 100 }
            );
        }
        let mut json = serde_json::to_value(input("USD", "10.00")).unwrap();
        json["amount"] = serde_json::json!(10.00);
        assert!(serde_json::from_value::<NewPendingExpense>(json).is_err());
    }

    #[tokio::test]
    async fn confirmation_preserves_original_purchase_and_supports_excess_refunds() {
        let (_dir, store) = ledger().await;
        let pending = store
            .add_pending(input("JPY", "10000"), None)
            .await
            .unwrap();
        assert_eq!(pending.pending.remind_on, "2026-10-02");
        assert!(pending.transaction.is_none());
        let null_amount: Option<i64> =
            sqlx::query_scalar("SELECT confirmed_amount_minor FROM pending_expenses WHERE id = ?")
                .bind(&pending.pending.id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(null_amount.is_none());
        let filters = Filters {
            month: Some("2026-09".into()),
            keyword: Some("乐天".into()),
            ..Default::default()
        };
        let summary = store.summary(&filters).await.unwrap();
        assert_eq!(summary.expense, "0.00");
        assert_eq!(summary.pending_count, 1);
        assert_eq!(summary.pending_by_currency[0].amount, "10000");
        let result = store
            .confirm_pending(&pending.pending.id, confirm("510.38"), None)
            .await
            .unwrap();
        let txn = result.transaction.unwrap();
        assert_eq!(txn.date, "2026-09-29");
        assert_eq!(txn.category, "购物");
        assert_eq!(result.pending.posted_date.as_deref(), Some("2026-10-03"));
        assert!(result.pending.confirmed_at.unwrap().ends_with('Z'));
        assert_eq!(
            store
                .get(&txn.id)
                .await
                .unwrap()
                .foreign_expense
                .unwrap()
                .amount,
            "10000"
        );
        let summary = store.summary(&filters).await.unwrap();
        assert_eq!(summary.expense, "510.38");
        assert_eq!(summary.pending_count, 0);
        assert_eq!(
            store
                .summary(&Filters {
                    month: Some("2026-10".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .expense,
            "0.00"
        );
        store
            .add(
                NewTransaction {
                    kind: Kind::Refund,
                    amount: "520.00".parse().unwrap(),
                    date: "2026-10-04".into(),
                    occurred_at: None,
                    category: None,
                    note: None,
                    channel: None,
                    original_id: Some(txn.id.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&txn.id).await.unwrap().net_expense.as_deref(),
            Some("-9.62")
        );
        assert_eq!(
            store
                .pending_history(&pending.pending.id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.history(&txn.id).await.unwrap().len(), 1);
        assert_eq!(
            store
                .cancel_pending(&pending.pending.id, None)
                .await
                .unwrap_err()
                .code,
            "ALREADY_CONFIRMED"
        );
        assert_eq!(
            store
                .snooze_pending(&pending.pending.id, "2026-10-05", None)
                .await
                .unwrap_err()
                .code,
            "ALREADY_CONFIRMED"
        );
    }

    #[tokio::test]
    async fn request_ids_and_reconfirmation_are_safe_after_edits_and_voids() {
        let (_dir, store) = ledger().await;
        let first = store
            .add_pending(input("USD", "20"), Some("create"))
            .await
            .unwrap();
        let id = first.pending.id;
        assert!(
            store
                .add_pending(input("USD", "20"), Some("create"))
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(
            store
                .add_pending(input("USD", "21"), Some("create"))
                .await
                .unwrap_err()
                .code,
            "IDEMPOTENCY_CONFLICT"
        );
        assert_eq!(
            store
                .confirm_pending(&id, confirm("140.01"), Some("create"))
                .await
                .unwrap_err()
                .code,
            "IDEMPOTENCY_CONFLICT"
        );
        let initial = store
            .confirm_pending(&id, confirm("140.01"), Some("confirm"))
            .await
            .unwrap()
            .transaction
            .unwrap();
        assert_eq!(
            store
                .confirm_pending(&id, confirm("140.02"), None)
                .await
                .unwrap_err()
                .code,
            "ALREADY_CONFIRMED"
        );
        store
            .update(
                &initial.id,
                UpdateTransaction {
                    amount: Some("140.99".parse().unwrap()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let repeated = store
            .confirm_pending(&id, confirm("140.01"), None)
            .await
            .unwrap();
        assert!(repeated.replayed);
        assert_eq!(repeated.transaction.unwrap().amount.to_string(), "140.99");
        store.void(&initial.id, false, None).await.unwrap();
        assert!(
            store
                .confirm_pending(&id, confirm("140.01"), None)
                .await
                .unwrap()
                .transaction
                .unwrap()
                .voided
        );
        let replay = store
            .confirm_pending(&id, confirm("140.01"), Some("confirm"))
            .await
            .unwrap();
        assert_eq!(
            replay.transaction.as_ref().unwrap().amount.to_string(),
            "140.01"
        );
        assert!(!replay.transaction.unwrap().voided);
        assert_eq!(
            store
                .add_pending(input("USD", "20"), Some("create"))
                .await
                .unwrap()
                .pending
                .status,
            "pending"
        );
        assert_eq!(store.pending_history(&id).await.unwrap().len(), 2);
        assert_eq!(
            store
                .void(&initial.id, false, Some("create"))
                .await
                .unwrap_err()
                .code,
            "IDEMPOTENCY_CONFLICT"
        );
    }

    #[tokio::test]
    async fn concurrent_confirmation_creates_exactly_one_expense() {
        let (_dir, store) = ledger().await;
        let id = store
            .add_pending(input("USD", "0.10"), None)
            .await
            .unwrap()
            .pending
            .id;
        let (first, second) = tokio::join!(
            store.confirm_pending(&id, confirm("0.70"), Some("first")),
            store.confirm_pending(&id, confirm("0.70"), Some("second"))
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(
            first.transaction.unwrap().id,
            second.transaction.unwrap().id
        );
        assert_ne!(first.replayed, second.replayed);
        assert_eq!(store.summary(&Filters::default()).await.unwrap().count, 1);
        assert_eq!(store.pending_history(&id).await.unwrap().len(), 2);
        let id = store
            .add_pending(input("USD", "0.20"), None)
            .await
            .unwrap()
            .pending
            .id;
        let (first, second) = tokio::join!(
            store.confirm_pending(&id, confirm("1.40"), None),
            store.confirm_pending(&id, confirm("1.41"), None)
        );
        assert!(first.is_ok() ^ second.is_ok());
        assert_eq!(
            first.err().or(second.err()).unwrap().code,
            "ALREADY_CONFIRMED"
        );
        assert_eq!(store.summary(&Filters::default()).await.unwrap().count, 2);
    }

    #[tokio::test]
    async fn equivalent_currency_amount_spellings_replay_same_request() {
        let (_dir, store) = ledger().await;
        for (currency, first, second, key, canonical) in [
            ("USD", "20", "00020.00", "usd", "20.00"),
            ("JPY", "10000", "00010000", "jpy", "10000"),
        ] {
            let original = store
                .add_pending(input(currency, first), Some(key))
                .await
                .unwrap();
            let repeated = store
                .add_pending(input(currency, second), Some(key))
                .await
                .unwrap();
            assert!(repeated.replayed);
            assert_eq!(original.pending.id, repeated.pending.id);
            assert_eq!(repeated.pending.amount, canonical);
        }
        assert_eq!(
            store
                .list_pending(&pending_filters("2026-10-01"))
                .await
                .unwrap()
                .total,
            2
        );
    }

    #[tokio::test]
    async fn summary_is_consistent_while_confirmations_are_committed() {
        let (_dir, store) = ledger().await;
        let mut ids = Vec::new();
        for _ in 0..20 {
            ids.push(
                store
                    .add_pending(input("USD", "1"), None)
                    .await
                    .unwrap()
                    .pending
                    .id,
            );
        }
        let write = async {
            for id in ids {
                store
                    .confirm_pending(&id, confirm("7"), None)
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
            }
        };
        let read = async {
            for _ in 0..40 {
                let summary = store.summary(&Filters::default()).await.unwrap();
                assert_eq!(summary.count + summary.pending_count, 20);
                assert_eq!(summary.expense, format!("{}.00", summary.count * 7));
            }
        };
        tokio::join!(write, read);
        assert_eq!(
            store
                .summary(&Filters::default())
                .await
                .unwrap()
                .pending_count,
            0
        );
    }

    #[tokio::test]
    async fn failed_confirmation_rolls_back_expense_link_audits_and_request() {
        let (_dir, store) = ledger().await;
        let id = store
            .add_pending(input("JPY", "100"), None)
            .await
            .unwrap()
            .pending
            .id;
        sqlx::query("CREATE TRIGGER fail_confirm BEFORE INSERT ON pending_audit_log WHEN NEW.action = 'confirm' BEGIN SELECT RAISE(ABORT, 'test failure'); END")
            .execute(&store.pool).await.unwrap();
        assert!(
            store
                .confirm_pending(&id, confirm("5.10"), Some("retry"))
                .await
                .is_err()
        );
        assert_eq!(
            store.get_pending(&id).await.unwrap().pending.status,
            "pending"
        );
        for sql in [
            "SELECT COUNT(*) FROM transactions",
            "SELECT COUNT(*) FROM audit_log",
            "SELECT COUNT(*) FROM idempotency",
        ] {
            let count: i64 = sqlx::query_scalar(sql)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{sql} leaked from failed confirmation");
        }
        assert_eq!(store.pending_history(&id).await.unwrap().len(), 1);
        sqlx::query("DROP TRIGGER fail_confirm")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .confirm_pending(&id, confirm("5.10"), Some("retry"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reminders_filters_cancellation_and_pagination() {
        let (_dir, store) = ledger().await;
        let mut original = input("USD", "20");
        original.date = "2026-09-01".into();
        let id = store.add_pending(original, None).await.unwrap().pending.id;
        let mut filters = pending_filters("2026-09-03");
        filters.due_only = true;
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 0);
        filters.as_of = "2026-09-04".into();
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 1);
        store
            .snooze_pending(&id, "2026-09-06", Some("snooze"))
            .await
            .unwrap();
        assert!(
            store
                .snooze_pending(&id, "2026-09-06", Some("snooze"))
                .await
                .unwrap()
                .replayed
        );
        assert!(store.snooze_pending(&id, "2026-09-05", None).await.is_err());
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 0);
        filters.as_of = "2026-09-06".into();
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 1);
        let other = store
            .add_pending(input("JPY", "1000"), None)
            .await
            .unwrap()
            .pending
            .id;
        filters.due_only = false;
        filters.filters.limit = 1;
        let page = store.list_pending(&filters).await.unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0].id, id);
        filters.filters.offset = 1;
        assert_eq!(
            store.list_pending(&filters).await.unwrap().items[0].id,
            other
        );
        filters.filters.offset = 0;
        filters.currency = Some("JPY".into());
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 1);
        filters.filters.keyword = Some("missing".into());
        assert_eq!(store.list_pending(&filters).await.unwrap().total, 0);
        store.cancel_pending(&id, Some("cancel")).await.unwrap();
        assert!(
            store
                .cancel_pending(&id, Some("cancel"))
                .await
                .unwrap()
                .replayed
        );
        assert!(store.cancel_pending(&id, None).await.unwrap().replayed);
        assert_eq!(
            store
                .confirm_pending(&id, confirm("140"), None)
                .await
                .unwrap_err()
                .code,
            "PENDING_CANCELLED"
        );
        assert_eq!(
            store
                .snooze_pending(&id, "2026-09-10", None)
                .await
                .unwrap_err()
                .code,
            "PENDING_CANCELLED"
        );
        assert_eq!(store.pending_history(&id).await.unwrap().len(), 3);
        let mut all = pending_filters("2026-10-10");
        all.status = Some("all".into());
        assert_eq!(store.list_pending(&all).await.unwrap().total, 2);
        all.due_only = true;
        assert_eq!(store.list_pending(&all).await.unwrap().total, 1);
    }

    #[tokio::test]
    async fn pending_totals_are_i128_exact_separated_and_use_identical_filters() {
        let (_dir, store) = ledger().await;
        for (currency, amount) in [
            ("JPY", "9223372036854775807"),
            ("JPY", "9223372036854775807"),
            ("USD", "92233720368547758.07"),
            ("USD", "92233720368547758.07"),
        ] {
            store
                .add_pending(input(currency, amount), None)
                .await
                .unwrap();
        }
        let summary = store.summary(&Filters::default()).await.unwrap();
        assert_eq!(summary.pending_count, 4);
        assert_eq!(summary.pending_by_currency[0].currency, "JPY");
        assert_eq!(
            summary.pending_by_currency[0].amount,
            "18446744073709551614"
        );
        assert_eq!(
            summary.pending_by_currency[1].amount,
            "184467440737095516.14"
        );
        assert_eq!(summary.pending_by_currency[1].count, 2);
        assert_eq!(summary.expense, "0.00");
        for filters in [
            Filters {
                month: Some("2026-10".into()),
                ..Default::default()
            },
            Filters {
                from: Some("2026-09-30".into()),
                ..Default::default()
            },
            Filters {
                to: Some("2026-09-28".into()),
                ..Default::default()
            },
            Filters {
                kind: Some(Kind::Income),
                ..Default::default()
            },
            Filters {
                kind: Some(Kind::Refund),
                ..Default::default()
            },
            Filters {
                category: Some("交通".into()),
                ..Default::default()
            },
            Filters {
                keyword: Some("missing".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(store.summary(&filters).await.unwrap().pending_count, 0);
        }
        assert_eq!(
            store
                .summary(&Filters {
                    limit: 1,
                    offset: 100,
                    keyword: Some("海淘".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .pending_count,
            4
        );
    }

    #[tokio::test]
    async fn invalid_inputs_and_sql_constraints_cannot_corrupt_lifecycle() {
        let (_dir, store) = ledger().await;
        for date in ["2026-02-30", "0000-01-01", "2026-9-1", "9999-12-30"] {
            let mut data = input("USD", "20");
            data.date = date.into();
            assert!(store.add_pending(data, None).await.is_err());
        }
        let mut data = input("USD", "20");
        data.category = Some("工资".into());
        assert!(store.add_pending(data, None).await.is_err());
        let id = store
            .add_pending(input("USD", "20"), None)
            .await
            .unwrap()
            .pending
            .id;
        assert!(
            store
                .confirm_pending(
                    &id,
                    ConfirmPendingExpense {
                        amount: Money::from_minor(0),
                        posted_date: None
                    },
                    None
                )
                .await
                .is_err()
        );
        assert!(
            store
                .confirm_pending(
                    &id,
                    ConfirmPendingExpense {
                        amount: "1".parse().unwrap(),
                        posted_date: Some("2026-09-28".into())
                    },
                    None
                )
                .await
                .is_err()
        );
        for sql in [
            "UPDATE pending_expenses SET amount_minor = 1.5",
            "UPDATE pending_expenses SET amount_minor = 0",
            "UPDATE pending_expenses SET status = 'wrong'",
            "UPDATE pending_expenses SET status = 'confirmed'",
            "UPDATE pending_expenses SET confirmed_amount_minor = 100",
            "UPDATE pending_expenses SET currency = 'CNY'",
            "UPDATE pending_expenses SET category = '工资'",
            "UPDATE pending_expenses SET remind_on = '2026-09-01'",
        ] {
            assert!(
                sqlx::query(sql).execute(&store.pool).await.is_err(),
                "accepted {sql}"
            );
        }
        let income = store
            .add(
                NewTransaction {
                    kind: Kind::Income,
                    amount: "10".parse().unwrap(),
                    date: "2026-09-29".into(),
                    occurred_at: None,
                    category: None,
                    note: None,
                    channel: None,
                    original_id: None,
                },
                None,
            )
            .await
            .unwrap()
            .transaction
            .id;
        assert!(sqlx::query("UPDATE pending_expenses SET status = 'confirmed', transaction_id = ?, confirmed_amount_minor = 1000, confirmed_at = 'now'").bind(income).execute(&store.pool).await.is_err());
        let txn = store
            .confirm_pending(&id, confirm("140"), None)
            .await
            .unwrap()
            .transaction
            .unwrap();
        assert!(sqlx::query("UPDATE pending_expenses SET status = 'pending', transaction_id = NULL, confirmed_amount_minor = NULL, confirmed_at = NULL, posted_date = NULL").execute(&store.pool).await.is_err());
        assert!(
            sqlx::query("UPDATE transactions SET kind = 'income' WHERE id = ?")
                .bind(txn.id)
                .execute(&store.pool)
                .await
                .is_err()
        );
    }
}
