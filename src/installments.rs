//! Monthly CNY installment schedules. Only explicitly confirmed periods enter the ledger.
use chrono::{Months, NaiveDate};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, sqlite::SqliteRow};

use crate::{
    error::{AppError, Result},
    models::*,
    money::{Money, format_minor},
    occurrence::validate_occurrence,
    store::{
        Store, audit, checked_add, clean_optional, fetch_record, now, require_category,
        save_request, validate_amount, validate_date, validate_filters, validate_request_id,
    },
};

/// Divide integer fen; all remainder goes to the final period. Month dates are
/// independently anchored to the first date, so Jan 31 -> Feb 28 -> Mar 31.
fn schedule(input: &NewInstallment) -> Result<Vec<(String, i64, i64)>> {
    validate_amount(input.amount)?;
    if input.fee.minor() < 0
        || !(2..=360).contains(&input.periods)
        || input.amount.minor() < i64::from(input.periods)
    {
        return Err(AppError::invalid(
            "期数须为 2..360，本金须足够每期至少一分，手续费不能为负",
        ));
    }
    validate_date(&input.date)?;
    validate_date(&input.first_due_date)?;
    if input.first_due_date < input.date {
        return Err(AppError::invalid("首期日期不能早于消费日期"));
    }
    let first = NaiveDate::parse_from_str(&input.first_due_date, "%Y-%m-%d")
        .map_err(|_| AppError::invalid("首期日期无效"))?;
    let n = i64::from(input.periods);
    let mut result = Vec::with_capacity(input.periods as usize);
    for i in 0..input.periods {
        let date = first
            .checked_add_months(Months::new(i))
            .ok_or_else(|| AppError::invalid("分期日期超出范围"))?
            .to_string();
        validate_date(&date)?;
        let last = i == input.periods - 1;
        let principal = input.amount.minor() / n + if last { input.amount.minor() % n } else { 0 };
        let fee = input.fee.minor() / n + if last { input.fee.minor() % n } else { 0 };
        principal
            .checked_add(fee)
            .ok_or_else(|| AppError::invalid("单期本金与手续费合计超出金额范围"))?;
        result.push((date, principal, fee));
    }
    Ok(result)
}

impl Store {
    pub async fn add_installment(
        &self,
        mut input: NewInstallment,
        request_id: Option<&str>,
    ) -> Result<InstallmentWriteResult> {
        let periods = schedule(&input)?;
        input.occurred_at = validate_occurrence(&input.date, input.occurred_at.as_deref())?;
        validate_request_id(request_id)?;
        let payload = serde_json::to_string(
            &serde_json::json!({"operation":"installment.add", "input":input}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let category = input.category.as_deref().unwrap_or("其他支出");
        require_category(&mut tx, category, Kind::Expense).await?;
        let id = format!("inst_{}", uuid::Uuid::new_v4().simple());
        let timestamp = now();
        sqlx::query("INSERT INTO installment_plans (id, amount_minor, fee_minor, periods, date, occurred_at, first_due_date, category, merchant, note, channel, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id).bind(input.amount.minor()).bind(input.fee.minor()).bind(i64::from(input.periods))
            .bind(&input.date).bind(&input.occurred_at).bind(&input.first_due_date).bind(category)
            .bind(clean_optional(input.merchant)).bind(clean_optional(input.note)).bind(clean_optional(input.channel))
            .bind(&timestamp).bind(&timestamp).execute(&mut *tx).await?;
        for (i, (date, principal, fee)) in periods.into_iter().enumerate() {
            sqlx::query("INSERT INTO installment_periods (plan_id, period, due_date, principal_minor, fee_minor) VALUES (?, ?, ?, ?, ?)")
                .bind(&id).bind((i + 1) as i64).bind(date).bind(principal).bind(fee).execute(&mut *tx).await?;
        }
        let detail = fetch_detail(&mut tx, &id).await?;
        plan_audit(&mut tx, "create", None, &detail).await?;
        let result = InstallmentWriteResult {
            detail,
            transaction: None,
            replayed: false,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn confirm_installment(
        &self,
        id: &str,
        period: u32,
        mut input: ConfirmInstallment,
        request_id: Option<&str>,
    ) -> Result<InstallmentWriteResult> {
        validate_request_id(request_id)?;
        input.occurred_at = validate_occurrence(&input.date, input.occurred_at.as_deref())?;
        let payload = serde_json::to_string(
            &serde_json::json!({"operation":"installment.confirm", "id":id, "period":period, "input":input}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let before = fetch_detail(&mut tx, id).await?;
        let item = before
            .schedule
            .iter()
            .find(|item| item.period == period)
            .ok_or_else(|| AppError::new("NOT_FOUND", "分期期数不存在"))?;
        if item.status == "confirmed" {
            if item.confirmed_date.as_deref() != Some(&input.date)
                || item.confirmed_occurred_at != input.occurred_at
            {
                return Err(AppError::new(
                    "ALREADY_CONFIRMED",
                    "本期已确认，请用 edit 修正关联的人民币账单",
                ));
            }
            let transaction =
                Some(fetch_record(&mut tx, item.transaction_id.as_deref().unwrap()).await?);
            let result = InstallmentWriteResult {
                detail: before,
                transaction,
                replayed: true,
            };
            save_request(&mut tx, request_id, &payload, &result).await?;
            tx.commit().await?;
            return Ok(result);
        }
        if before.plan.status != "active" || item.status != "pending" {
            return Err(AppError::new(
                "INSTALLMENT_CLOSED",
                "此分期计划或本期已取消",
            ));
        }
        if input.date < before.plan.date {
            return Err(AppError::invalid("入账日期不能早于消费日期"));
        }
        require_category(&mut tx, &before.plan.category, Kind::Expense).await?;
        let transaction_id = format!("txn_{}", uuid::Uuid::new_v4().simple());
        let timestamp = now();
        sqlx::query("INSERT INTO transactions (id, kind, amount_minor, category, date, occurred_at, note, channel, created_at, updated_at) VALUES (?, 'expense', ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&transaction_id).bind(item.amount.minor()).bind(&before.plan.category)
            .bind(&input.date).bind(&input.occurred_at).bind(&before.plan.note).bind(&before.plan.channel)
            .bind(&timestamp).bind(&timestamp).execute(&mut *tx).await?;
        let transaction = fetch_record(&mut tx, &transaction_id).await?;
        audit(&mut tx, "create", None, &transaction).await?;
        sqlx::query("UPDATE installment_periods SET status = 'confirmed', transaction_id = ?, confirmed_date = ?, confirmed_occurred_at = ?, confirmed_at = ? WHERE plan_id = ? AND period = ?")
            .bind(&transaction_id).bind(&input.date).bind(&input.occurred_at).bind(&timestamp)
            .bind(id).bind(i64::from(period)).execute(&mut *tx).await?;
        sqlx::query("UPDATE installment_plans SET status = CASE WHEN EXISTS (SELECT 1 FROM installment_periods WHERE plan_id = ? AND status = 'pending') THEN 'active' ELSE 'completed' END, updated_at = ? WHERE id = ?")
            .bind(id).bind(&timestamp).bind(id).execute(&mut *tx).await?;
        let detail = fetch_detail(&mut tx, id).await?;
        plan_audit(&mut tx, "confirm", Some(&before), &detail).await?;
        let result = InstallmentWriteResult {
            detail,
            transaction: Some(transaction),
            replayed: false,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Cancel only unconfirmed periods. Existing expenses and refunds remain auditable.
    pub async fn cancel_installment(
        &self,
        id: &str,
        request_id: Option<&str>,
    ) -> Result<InstallmentWriteResult> {
        validate_request_id(request_id)?;
        let payload =
            serde_json::to_string(&serde_json::json!({"operation":"installment.cancel", "id":id}))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(result) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let before = fetch_detail(&mut tx, id).await?;
        if before.plan.status == "completed" {
            return Err(AppError::new(
                "INSTALLMENT_CLOSED",
                "全部期数已确认，无剩余期数可取消",
            ));
        }
        let replayed = before.plan.status == "cancelled";
        let detail = if replayed {
            before
        } else {
            sqlx::query("UPDATE installment_periods SET status = 'cancelled' WHERE plan_id = ? AND status = 'pending'")
                .bind(id).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE installment_plans SET status = 'cancelled', updated_at = ? WHERE id = ?",
            )
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
            let detail = fetch_detail(&mut tx, id).await?;
            plan_audit(&mut tx, "cancel", Some(&before), &detail).await?;
            detail
        };
        let result = InstallmentWriteResult {
            detail,
            transaction: None,
            replayed,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_installment(&self, id: &str) -> Result<InstallmentDetail> {
        let mut tx = self.pool.begin().await?;
        let detail = fetch_detail(&mut tx, id).await?;
        tx.commit().await?;
        Ok(detail)
    }

    pub async fn list_installments(&self, query: &InstallmentFilters) -> Result<InstallmentList> {
        let filters = &query.filters;
        let limit = validate_query(filters)?;
        let status = query.status.as_deref().unwrap_or("active");
        if !["active", "completed", "cancelled", "all"].contains(&status) {
            return Err(AppError::invalid(
                "status 须为 active、completed、cancelled 或 all",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let mut count =
            QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM installment_plans p WHERE 1 = 1");
        push_filters(&mut count, filters, false);
        push_status(&mut count, status);
        let total = count.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut select =
            QueryBuilder::<Sqlite>::new("SELECT p.* FROM installment_plans p WHERE 1 = 1");
        push_filters(&mut select, filters, false);
        push_status(&mut select, status);
        select
            .push(" ORDER BY p.date, p.id LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(filters.offset);
        let items = select
            .build()
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(decode_plan)
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(InstallmentList {
            items,
            total,
            limit,
            offset: filters.offset,
        })
    }

    pub async fn due_installments(
        &self,
        filters: &Filters,
        as_of: &str,
    ) -> Result<InstallmentDueList> {
        let limit = validate_query(filters)?;
        validate_date(as_of)?;
        let source = " FROM installment_periods s JOIN installment_plans p ON s.plan_id = p.id WHERE p.status = 'active' AND s.status = 'pending' AND s.due_date <= ";
        let mut tx = self.pool.begin().await?;
        let mut count = QueryBuilder::<Sqlite>::new(format!("SELECT COUNT(*){source}"));
        count.push_bind(as_of);
        push_filters(&mut count, filters, true);
        let total = count.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut select = QueryBuilder::<Sqlite>::new(format!(
            "SELECT s.*, p.periods, p.category, p.merchant, p.note, p.channel{source}"
        ));
        select.push_bind(as_of);
        push_filters(&mut select, filters, true);
        select
            .push(" ORDER BY s.due_date, s.plan_id, s.period LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(filters.offset);
        let items = select
            .build()
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|row| {
                Ok(InstallmentDue {
                    installment: decode_period(row)?,
                    periods: row.try_get::<i64, _>("periods")? as u32,
                    category: row.try_get("category")?,
                    merchant: row.try_get("merchant")?,
                    note: row.try_get("note")?,
                    channel: row.try_get("channel")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(InstallmentDueList {
            items,
            total,
            limit,
            offset: filters.offset,
            as_of: as_of.to_owned(),
        })
    }

    pub async fn installment_history(&self, id: &str) -> Result<Vec<InstallmentAudit>> {
        let mut tx = self.pool.begin().await?;
        fetch_plan(&mut tx, id).await?;
        let rows = sqlx::query("SELECT * FROM installment_audit_log WHERE plan_id = ? ORDER BY id")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        let records = rows
            .iter()
            .map(|row| {
                Ok(InstallmentAudit {
                    id: row.try_get("id")?,
                    plan_id: row.try_get("plan_id")?,
                    action: row.try_get("action")?,
                    before: row
                        .try_get::<Option<String>, _>("before_json")?
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                    after: serde_json::from_str(row.try_get("after_json")?)?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(records)
    }
}

fn decode_plan(row: &SqliteRow) -> Result<InstallmentPlan> {
    let amount: i64 = row.try_get("amount_minor")?;
    let fee: i64 = row.try_get("fee_minor")?;
    Ok(InstallmentPlan {
        id: row.try_get("id")?,
        amount: Money::from_minor(amount),
        fee: Money::from_minor(fee),
        total: format_minor(checked_add(i128::from(amount), i128::from(fee))?),
        currency: "CNY".into(),
        periods: row.try_get::<i64, _>("periods")? as u32,
        date: row.try_get("date")?,
        occurred_at: row.try_get("occurred_at")?,
        first_due_date: row.try_get("first_due_date")?,
        category: row.try_get("category")?,
        merchant: row.try_get("merchant")?,
        note: row.try_get("note")?,
        channel: row.try_get("channel")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn decode_period(row: &SqliteRow) -> Result<InstallmentPeriod> {
    let principal: i64 = row.try_get("principal_minor")?;
    let fee: i64 = row.try_get("fee_minor")?;
    let amount = principal
        .checked_add(fee)
        .ok_or_else(|| AppError::new("AMOUNT_OVERFLOW", "单期金额溢出"))?;
    Ok(InstallmentPeriod {
        plan_id: row.try_get("plan_id")?,
        period: row.try_get::<i64, _>("period")? as u32,
        due_date: row.try_get("due_date")?,
        principal: Money::from_minor(principal),
        fee: Money::from_minor(fee),
        amount: Money::from_minor(amount),
        status: row.try_get("status")?,
        transaction_id: row.try_get("transaction_id")?,
        confirmed_date: row.try_get("confirmed_date")?,
        confirmed_occurred_at: row.try_get("confirmed_occurred_at")?,
        confirmed_at: row.try_get("confirmed_at")?,
    })
}

async fn fetch_plan(conn: &mut SqliteConnection, id: &str) -> Result<InstallmentPlan> {
    let row = sqlx::query("SELECT * FROM installment_plans WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| AppError::new("NOT_FOUND", format!("分期计划不存在：{id}")))?;
    decode_plan(&row)
}

async fn fetch_detail(conn: &mut SqliteConnection, id: &str) -> Result<InstallmentDetail> {
    let plan = fetch_plan(conn, id).await?;
    let rows = sqlx::query("SELECT * FROM installment_periods WHERE plan_id = ? ORDER BY period")
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
    let schedule = rows.iter().map(decode_period).collect::<Result<Vec<_>>>()?;
    let mut remaining = schedule.iter().filter(|item| item.status == "pending");
    let remaining_count = remaining.clone().count();
    let remaining_amount = format_minor(remaining.try_fold(0_i128, |total, item| {
        checked_add(total, i128::from(item.amount.minor()))
    })?);
    let confirmed_count = schedule
        .iter()
        .filter(|item| item.status == "confirmed")
        .count();
    let mut transactions = Vec::with_capacity(confirmed_count);
    for item in &schedule {
        if let Some(id) = &item.transaction_id {
            transactions.push(fetch_record(conn, id).await?);
        }
    }
    Ok(InstallmentDetail {
        plan,
        schedule,
        confirmed_count,
        remaining_count,
        remaining_amount,
        transactions,
    })
}

pub(crate) async fn find_by_transaction(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<InstallmentPeriod>> {
    let row = sqlx::query("SELECT * FROM installment_periods WHERE transaction_id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(decode_period).transpose()
}

async fn plan_audit(
    conn: &mut SqliteConnection,
    action: &str,
    before: Option<&InstallmentDetail>,
    after: &InstallmentDetail,
) -> Result<()> {
    sqlx::query("INSERT INTO installment_audit_log (plan_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&after.plan.id).bind(action).bind(before.map(serde_json::to_string).transpose()?)
        .bind(serde_json::to_string(after)?).bind(now()).execute(conn).await?;
    Ok(())
}

async fn replay(
    conn: &mut SqliteConnection,
    request_id: Option<&str>,
    payload: &str,
) -> Result<Option<InstallmentWriteResult>> {
    let Some(id) = request_id else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT payload, response_json FROM idempotency WHERE request_id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<&str, _>("payload")? != payload {
        return Err(AppError::new(
            "IDEMPOTENCY_CONFLICT",
            "此 request-id 已用于不同请求",
        ));
    }
    let mut result: InstallmentWriteResult = serde_json::from_str(row.try_get("response_json")?)?;
    result.replayed = true;
    Ok(Some(result))
}

fn validate_query(filters: &Filters) -> Result<i64> {
    validate_filters(filters)?;
    if filters.month.is_some() && (filters.from.is_some() || filters.to.is_some()) {
        return Err(AppError::invalid("month 与 from/to 不能同时使用"));
    }
    let limit = if filters.limit == 0 {
        50
    } else {
        filters.limit
    };
    if !(1..=1000).contains(&limit) || filters.offset < 0 {
        return Err(AppError::invalid(
            "limit 必须为 1..1000，offset 必须大于等于 0",
        ));
    }
    Ok(limit)
}

fn push_status(query: &mut QueryBuilder<Sqlite>, status: &str) {
    if status != "all" {
        query.push(" AND p.status = ").push_bind(status.to_owned());
    }
}

fn push_filters(query: &mut QueryBuilder<Sqlite>, filters: &Filters, due: bool) {
    let date = if due { "s.due_date" } else { "p.date" };
    if let Some(month) = &filters.month {
        query
            .push(format!(" AND substr({date}, 1, 7) = "))
            .push_bind(month.clone());
    }
    if let Some(from) = &filters.from {
        query
            .push(format!(" AND {date} >= "))
            .push_bind(from.clone());
    }
    if let Some(to) = &filters.to {
        query.push(format!(" AND {date} <= ")).push_bind(to.clone());
    }
    if let Some(category) = &filters.category {
        query.push(" AND p.category = ").push_bind(category.clone());
    }
    if let Some(keyword) = &filters.keyword {
        query
            .push(" AND (instr(p.id, ")
            .push_bind(keyword.clone())
            .push(") > 0 OR instr(COALESCE(p.merchant, ''), ")
            .push_bind(keyword.clone())
            .push(") > 0 OR instr(COALESCE(p.note, ''), ")
            .push_bind(keyword.clone())
            .push(") > 0 OR instr(COALESCE(p.channel, ''), ")
            .push_bind(keyword.clone())
            .push(") > 0)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(amount: i64, fee: i64, periods: u32, first: &str) -> NewInstallment {
        NewInstallment {
            amount: Money::from_minor(amount),
            fee: Money::from_minor(fee),
            periods,
            date: "2026-01-01".into(),
            first_due_date: first.into(),
            occurred_at: None,
            category: None,
            merchant: None,
            note: None,
            channel: None,
        }
    }

    #[test]
    fn exact_remainders_and_month_end_anchor() {
        assert_eq!(
            schedule(&input(10000, 101, 3, "2026-01-31")).unwrap(),
            [
                ("2026-01-31".into(), 3333, 33),
                ("2026-02-28".into(), 3333, 33),
                ("2026-03-31".into(), 3334, 35),
            ]
        );
        let leap = schedule(&input(12, 0, 3, "2028-01-31")).unwrap();
        assert_eq!(leap[1].0, "2028-02-29");
        assert_eq!(leap[2].0, "2028-03-31");
        assert_eq!(
            schedule(&input(360, 0, 360, "2026-01-01"))
                .unwrap()
                .last()
                .unwrap()
                .0,
            "2055-12-01"
        );
    }

    #[test]
    fn bounds_and_large_totals_never_round_or_overflow() {
        for entry in [
            input(0, 0, 3, "2026-02-01"),
            input(2, 0, 3, "2026-02-01"),
            input(100, -1, 3, "2026-02-01"),
            input(100, 0, 1, "2026-02-01"),
            input(1000, 0, 361, "2026-02-01"),
            input(100, 0, 3, "2025-12-31"),
            input(100, 0, 3, "2026-02-30"),
            input(100, 0, 3, "9999-12-31"),
            input(i64::MAX, i64::MAX, 2, "2026-02-01"),
        ] {
            assert!(schedule(&entry).is_err(), "{entry:?}");
        }
        for (amount, fee, periods) in [(3, 1, 3), (i64::MAX, 1, 2), (i64::MAX, i64::MAX, 360)] {
            let items = schedule(&input(amount, fee, periods, "2026-02-01")).unwrap();
            assert_eq!(
                items.iter().map(|i| i128::from(i.1)).sum::<i128>(),
                i128::from(amount)
            );
            assert_eq!(
                items.iter().map(|i| i128::from(i.2)).sum::<i128>(),
                i128::from(fee)
            );
            assert!(
                items
                    .iter()
                    .all(|i| i.1 > 0 && i.1.checked_add(i.2).is_some())
            );
        }
    }
}
