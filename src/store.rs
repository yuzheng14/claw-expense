use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::{NaiveDate, SecondsFormat, Utc};
use futures_util::TryStreamExt;
use sqlx::{
    QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};

use crate::{
    error::{AppError, Result},
    models::*,
    money::{Money, format_minor},
};

const APPLICATION_ID: i64 = 1129071960;
const SELECT_RECORD: &str = "SELECT t.id, t.kind, t.amount_minor, t.currency, COALESCE(t.category, original.category) AS category, t.date, t.note, t.channel, t.original_id, t.voided, t.created_at, t.updated_at FROM transactions t LEFT JOIN transactions original ON t.original_id = original.id";

pub struct Store {
    pub pool: SqlitePool,
}

impl Store {
    /// Opening an existing ledger never silently creates or adopts another database.
    pub async fn open(path: &Path, create: bool) -> Result<Self> {
        let existed = path.try_exists()?;
        if !existed && !create {
            return Err(AppError::new(
                "NOT_INITIALIZED",
                "账本不存在，请先执行 init",
            ));
        }
        if !existed {
            crate::paths::ensure_unused_database_path(path)?;
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            // create_new also prevents races from truncating a newly created ledger.
            options.open(path)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(15));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        if existed {
            let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
                .fetch_one(&pool)
                .await?;
            if application_id != APPLICATION_ID {
                pool.close().await;
                return Err(AppError::new(
                    "NOT_INITIALIZED",
                    "指定文件不是 claw-expense 账本",
                ));
            }
        }
        sqlx::migrate!().run(&pool).await?;
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await?;
        Ok(Self { pool })
    }

    pub async fn add(
        &self,
        input: NewTransaction,
        request_id: Option<&str>,
    ) -> Result<WriteResult> {
        validate_amount(input.amount)?;
        validate_date(&input.date)?;
        validate_request_id(request_id)?;
        if input.kind == Kind::Refund {
            if input.original_id.as_deref().is_none_or(str::is_empty) {
                return Err(AppError::invalid("退款必须指定原支出 ID"));
            }
            if input.category.is_some() {
                return Err(AppError::invalid("退款分类继承原支出，不接受 category"));
            }
        } else if input.original_id.is_some() {
            return Err(AppError::invalid("只有退款可以指定 original_id"));
        }
        let payload =
            serde_json::to_string(&serde_json::json!({"operation": "add", "input": input}))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(replay) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(replay);
        }
        let category = match input.kind {
            Kind::Refund => {
                let original = fetch_record(&mut tx, input.original_id.as_deref().unwrap()).await?;
                if original.kind != Kind::Expense || original.voided {
                    return Err(AppError::invalid("退款只能关联未作废的支出"));
                }
                None
            }
            kind => {
                let name = input
                    .category
                    .as_deref()
                    .unwrap_or(if kind == Kind::Expense {
                        "其他支出"
                    } else {
                        "其他收入"
                    });
                require_category(&mut tx, name, kind).await?;
                Some(name.to_owned())
            }
        };
        let id = format!("txn_{}", uuid::Uuid::new_v4().simple());
        let now = now();
        sqlx::query("INSERT INTO transactions (id, kind, amount_minor, category, date, note, channel, original_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id).bind(input.kind.as_str()).bind(input.amount.minor())
            .bind(category).bind(&input.date).bind(clean_optional(input.note))
            .bind(clean_optional(input.channel)).bind(input.original_id)
            .bind(&now).bind(&now).execute(&mut *tx).await?;
        let record = fetch_record(&mut tx, &id).await?;
        audit(&mut tx, "create", None, &record).await?;
        let result = WriteResult {
            transaction: record,
            replayed: false,
            affected_ids: vec![id],
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn update(
        &self,
        id: &str,
        patch: UpdateTransaction,
        request_id: Option<&str>,
    ) -> Result<WriteResult> {
        validate_request_id(request_id)?;
        if patch.amount.is_none()
            && patch.date.is_none()
            && patch.category.is_none()
            && patch.note.is_none()
            && patch.channel.is_none()
        {
            return Err(AppError::invalid("修改至少需要提供一个字段"));
        }
        if let Some(amount) = patch.amount {
            validate_amount(amount)?;
        }
        if let Some(date) = &patch.date {
            validate_date(date)?;
        }
        let payload = serde_json::to_string(
            &serde_json::json!({"operation": "update", "id": id, "patch": patch}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(replay) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(replay);
        }
        let before = fetch_record(&mut tx, id).await?;
        if before.voided {
            return Err(AppError::new("VOIDED_TRANSACTION", "已作废的账单不能修改"));
        }
        if let Some(category) = &patch.category {
            if before.kind == Kind::Refund {
                return Err(AppError::invalid("退款分类继承原支出，不能单独修改"));
            }
            require_category(&mut tx, category, before.kind).await?;
        }
        let amount = patch.amount.unwrap_or(before.amount);
        let date = patch.date.as_deref().unwrap_or(&before.date);
        let category = if before.kind == Kind::Refund {
            None
        } else {
            Some(patch.category.as_deref().unwrap_or(&before.category))
        };
        let note = match patch.note {
            Some(value) => clean_optional(Some(value)),
            None => before.note.clone(),
        };
        let channel = match patch.channel {
            Some(value) => clean_optional(Some(value)),
            None => before.channel.clone(),
        };
        sqlx::query("UPDATE transactions SET amount_minor = ?, date = ?, category = ?, note = ?, channel = ?, updated_at = ? WHERE id = ?")
            .bind(amount.minor()).bind(date).bind(category).bind(note).bind(channel).bind(now()).bind(id)
            .execute(&mut *tx).await?;
        let after = fetch_record(&mut tx, id).await?;
        audit(&mut tx, "update", Some(&before), &after).await?;
        let result = WriteResult {
            transaction: after,
            replayed: false,
            affected_ids: vec![id.to_owned()],
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn void(
        &self,
        id: &str,
        cascade_refunds: bool,
        request_id: Option<&str>,
    ) -> Result<WriteResult> {
        validate_request_id(request_id)?;
        let payload = serde_json::to_string(
            &serde_json::json!({"operation": "void", "id": id, "cascade_refunds": cascade_refunds}),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(replay) = replay(&mut tx, request_id, &payload).await? {
            tx.commit().await?;
            return Ok(replay);
        }
        let before = fetch_record(&mut tx, id).await?;
        if before.voided {
            return Err(AppError::new("VOIDED_TRANSACTION", "账单已经作废"));
        }
        let refund_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM transactions WHERE original_id = ? AND voided = 0 ORDER BY id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !refund_ids.is_empty() && !cascade_refunds {
            return Err(AppError::new(
                "HAS_REFUNDS",
                "支出仍有关联退款，请先作废退款或使用 --cascade-refunds",
            ));
        }
        let mut affected_ids = vec![id.to_owned()];
        for refund_id in refund_ids {
            let refund = fetch_record(&mut tx, &refund_id).await?;
            void_record(&mut tx, &refund).await?;
            affected_ids.push(refund_id);
        }
        let after = void_record(&mut tx, &before).await?;
        let result = WriteResult {
            transaction: after,
            replayed: false,
            affected_ids,
        };
        save_request(&mut tx, request_id, &payload, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get(&self, id: &str) -> Result<TransactionDetail> {
        let mut tx = self.pool.begin().await?;
        let transaction = fetch_record(&mut tx, id).await?;
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_RECORD);
        builder
            .push(" WHERE t.original_id = ")
            .push_bind(id)
            .push(" ORDER BY t.date, t.created_at, t.id");
        let refunds = builder
            .build()
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(decode_record)
            .collect::<Result<Vec<_>>>()?;
        let total = refunds
            .iter()
            .filter(|r| !r.voided)
            .try_fold(0_i128, |sum, r| {
                checked_add(sum, i128::from(r.amount.minor()))
            })?;
        let (net, status, excess) = if transaction.kind == Kind::Expense {
            let amount = i128::from(transaction.amount.minor());
            let status = if total == 0 {
                "none"
            } else if total < amount {
                "partial"
            } else if total == amount {
                "full"
            } else {
                "excess"
            };
            (
                Some(format_minor(checked_sub(amount, total)?)),
                Some(status.to_owned()),
                checked_sub(total, amount)?.max(0),
            )
        } else {
            (None, None, 0)
        };
        tx.commit().await?;
        Ok(TransactionDetail {
            transaction,
            refunds,
            refund_total: format_minor(total),
            net_expense: net,
            refund_status: status,
            excess_refund: format_minor(excess),
        })
    }

    pub async fn list(&self, filters: &Filters) -> Result<ListResult> {
        validate_filters(filters)?;
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
        let mut tx = self.pool.begin().await?;
        let mut count_query = QueryBuilder::<Sqlite>::new(
            "SELECT COUNT(*) FROM transactions t LEFT JOIN transactions original ON t.original_id = original.id",
        );
        push_filters(&mut count_query, filters, filters.include_voided);
        let total: i64 = count_query.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut query = QueryBuilder::<Sqlite>::new(SELECT_RECORD);
        push_filters(&mut query, filters, filters.include_voided);
        query
            .push(" ORDER BY t.date DESC, t.created_at DESC, t.id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(filters.offset);
        let items = query
            .build()
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(decode_record)
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(ListResult {
            items,
            total,
            limit,
            offset: filters.offset,
        })
    }

    /// Summaries always exclude voided transactions and never apply pagination.
    pub async fn summary(&self, filters: &Filters) -> Result<Summary> {
        validate_filters(filters)?;
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT t.kind, t.amount_minor, COALESCE(t.category, original.category) AS category FROM transactions t LEFT JOIN transactions original ON t.original_id = original.id",
        );
        push_filters(&mut query, filters, false);
        let mut rows = query.build().fetch(&self.pool);
        let mut totals = Totals::default();
        let mut categories: BTreeMap<String, Totals> = BTreeMap::new();
        let mut count = 0_i64;
        while let Some(row) = rows.try_next().await? {
            let kind: Kind = row.try_get::<&str, _>("kind")?.parse()?;
            let amount = i128::from(row.try_get::<i64, _>("amount_minor")?);
            totals.add(kind, amount)?;
            categories
                .entry(row.try_get("category")?)
                .or_default()
                .add(kind, amount)?;
            count = count.checked_add(1).ok_or_else(overflow)?;
        }
        let by_category = categories
            .into_iter()
            .map(|(category, totals)| {
                Ok(CategorySummary {
                    category,
                    income: format_minor(totals.income),
                    expense: format_minor(totals.expense),
                    refund: format_minor(totals.refund),
                    net_expense: format_minor(totals.net()?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Summary {
            currency: "CNY".to_owned(),
            count,
            income: format_minor(totals.income),
            expense: format_minor(totals.expense),
            refund: format_minor(totals.refund),
            net_expense: format_minor(totals.net()?),
            balance: format_minor(checked_sub(totals.income, totals.net()?)?),
            by_category,
        })
    }

    pub async fn categories(&self) -> Result<Vec<Category>> {
        sqlx::query("SELECT name, kind FROM categories ORDER BY kind, name")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| {
                Ok(Category {
                    name: row.try_get("name")?,
                    kind: row.try_get::<&str, _>("kind")?.parse()?,
                })
            })
            .collect()
    }

    pub async fn add_category(&self, name: &str, kind: Kind) -> Result<Category> {
        if name.trim().is_empty() || name != name.trim() {
            return Err(AppError::invalid("分类名称不能为空或包含首尾空白"));
        }
        if kind == Kind::Refund {
            return Err(AppError::invalid("退款继承原支出分类，不支持创建退款分类"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT kind FROM categories WHERE name = ?")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            if existing != kind.as_str() {
                return Err(AppError::new(
                    "CATEGORY_CONFLICT",
                    "此分类名称已用于另一种收支类型",
                ));
            }
        } else {
            sqlx::query("INSERT INTO categories (name, kind) VALUES (?, ?)")
                .bind(name)
                .bind(kind.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Category {
            name: name.to_owned(),
            kind,
        })
    }

    pub async fn history(&self, id: &str) -> Result<Vec<AuditRecord>> {
        let mut tx = self.pool.begin().await?;
        fetch_record(&mut tx, id).await?;
        let rows = sqlx::query("SELECT id, transaction_id, action, before_json, after_json, created_at FROM audit_log WHERE transaction_id = ? ORDER BY id")
            .bind(id).fetch_all(&mut *tx).await?;
        let records = rows
            .iter()
            .map(|row| {
                Ok(AuditRecord {
                    id: row.try_get("id")?,
                    transaction_id: row.try_get("transaction_id")?,
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

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}
fn clean_optional(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}
fn overflow() -> AppError {
    AppError::new("AMOUNT_OVERFLOW", "金额汇总超出可表示范围")
}
fn checked_add(left: i128, right: i128) -> Result<i128> {
    left.checked_add(right).ok_or_else(overflow)
}
fn checked_sub(left: i128, right: i128) -> Result<i128> {
    left.checked_sub(right).ok_or_else(overflow)
}
fn validate_amount(amount: Money) -> Result<()> {
    if amount.minor() <= 0 {
        return Err(AppError::invalid("单笔金额必须大于零"));
    }
    Ok(())
}
fn validate_request_id(id: Option<&str>) -> Result<()> {
    if id.is_some_and(|id| id.trim().is_empty() || id.len() > 200) {
        return Err(AppError::invalid(
            "request-id 必须为 1..200 字节的非空字符串",
        ));
    }
    Ok(())
}
fn validate_date(value: &str) -> Result<()> {
    if value.len() != 10
        || !value.bytes().enumerate().all(|(i, byte)| {
            if i == 4 || i == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
        || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err()
        || value.starts_with("0000")
    {
        return Err(AppError::invalid(
            "日期必须为有效的 YYYY-MM-DD（年份 0001..9999）",
        ));
    }
    Ok(())
}
fn validate_filters(filters: &Filters) -> Result<()> {
    if let Some(month) = &filters.month {
        if month.len() != 7 {
            return Err(AppError::invalid("月份必须为 YYYY-MM"));
        }
        validate_date(&format!("{month}-01"))?;
    }
    if let Some(from) = &filters.from {
        validate_date(from)?;
    }
    if let Some(to) = &filters.to {
        validate_date(to)?;
    }
    if let (Some(from), Some(to)) = (&filters.from, &filters.to)
        && from > to
    {
        return Err(AppError::invalid("from 不能晚于 to"));
    }
    Ok(())
}

fn push_filters(query: &mut QueryBuilder<Sqlite>, filters: &Filters, include_voided: bool) {
    query.push(" WHERE 1 = 1");
    if !include_voided {
        query.push(" AND t.voided = 0");
    }
    if let Some(month) = &filters.month {
        query.push(" AND substr(t.date, 1, 7) = ").push_bind(month);
    }
    if let Some(from) = &filters.from {
        query.push(" AND t.date >= ").push_bind(from);
    }
    if let Some(to) = &filters.to {
        query.push(" AND t.date <= ").push_bind(to);
    }
    if let Some(kind) = filters.kind {
        query.push(" AND t.kind = ").push_bind(kind.as_str());
    }
    if let Some(category) = &filters.category {
        query
            .push(" AND COALESCE(t.category, original.category) = ")
            .push_bind(category);
    }
    if let Some(keyword) = &filters.keyword {
        query
            .push(" AND (instr(COALESCE(t.note, ''), ")
            .push_bind(keyword)
            .push(") > 0 OR instr(t.id, ")
            .push_bind(keyword)
            .push(") > 0 OR instr(COALESCE(t.channel, ''), ")
            .push_bind(keyword)
            .push(") > 0)");
    }
}

fn decode_record(row: &SqliteRow) -> Result<TransactionRecord> {
    Ok(TransactionRecord {
        id: row.try_get("id")?,
        kind: row.try_get::<&str, _>("kind")?.parse()?,
        amount: Money::from_minor(row.try_get("amount_minor")?),
        currency: row.try_get("currency")?,
        category: row.try_get("category")?,
        date: row.try_get("date")?,
        note: row.try_get("note")?,
        channel: row.try_get("channel")?,
        original_id: row.try_get("original_id")?,
        voided: row.try_get("voided")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
async fn fetch_record(conn: &mut SqliteConnection, id: &str) -> Result<TransactionRecord> {
    let mut query = QueryBuilder::<Sqlite>::new(SELECT_RECORD);
    query.push(" WHERE t.id = ").push_bind(id);
    let row = query
        .build()
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| AppError::new("NOT_FOUND", format!("账单不存在：{id}")))?;
    decode_record(&row)
}
async fn require_category(conn: &mut SqliteConnection, name: &str, kind: Kind) -> Result<()> {
    let existing: Option<String> = sqlx::query_scalar("SELECT kind FROM categories WHERE name = ?")
        .bind(name)
        .fetch_optional(conn)
        .await?;
    if existing.as_deref() != Some(kind.as_str()) {
        return Err(AppError::invalid(format!(
            "分类不存在或类型不匹配：{name}；请先创建对应分类"
        )));
    }
    Ok(())
}
async fn audit(
    conn: &mut SqliteConnection,
    action: &str,
    before: Option<&TransactionRecord>,
    after: &TransactionRecord,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_log (transaction_id, action, before_json, after_json, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&after.id).bind(action).bind(before.map(serde_json::to_string).transpose()?)
        .bind(serde_json::to_string(after)?).bind(now()).execute(conn).await?;
    Ok(())
}
async fn void_record(
    conn: &mut SqliteConnection,
    before: &TransactionRecord,
) -> Result<TransactionRecord> {
    sqlx::query("UPDATE transactions SET voided = 1, updated_at = ? WHERE id = ?")
        .bind(now())
        .bind(&before.id)
        .execute(&mut *conn)
        .await?;
    let after = fetch_record(conn, &before.id).await?;
    audit(conn, "void", Some(before), &after).await?;
    Ok(after)
}
async fn replay(
    conn: &mut SqliteConnection,
    request_id: Option<&str>,
    payload: &str,
) -> Result<Option<WriteResult>> {
    let Some(id) = request_id else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT payload, response_json FROM idempotency WHERE request_id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<&str, _>("payload")? != payload {
        return Err(AppError::new(
            "IDEMPOTENCY_CONFLICT",
            "此 request-id 已用于不同请求",
        ));
    }
    let mut result: WriteResult = serde_json::from_str(row.try_get("response_json")?)?;
    result.replayed = true;
    Ok(Some(result))
}
async fn save_request(
    conn: &mut SqliteConnection,
    request_id: Option<&str>,
    payload: &str,
    result: &WriteResult,
) -> Result<()> {
    if let Some(id) = request_id {
        sqlx::query("INSERT INTO idempotency (request_id, payload, response_json, created_at) VALUES (?, ?, ?, ?)")
            .bind(id).bind(payload).bind(serde_json::to_string(result)?).bind(now()).execute(conn).await?;
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    income: i128,
    expense: i128,
    refund: i128,
}
impl Totals {
    fn add(&mut self, kind: Kind, amount: i128) -> Result<()> {
        let value = match kind {
            Kind::Income => &mut self.income,
            Kind::Expense => &mut self.expense,
            Kind::Refund => &mut self.refund,
        };
        *value = checked_add(*value, amount)?;
        Ok(())
    }
    fn net(&self) -> Result<i128> {
        checked_sub(self.expense, self.refund)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("ledger.sqlite3"), true)
            .await
            .unwrap();
        (directory, store)
    }

    fn expense(amount: &str, date: &str) -> NewTransaction {
        NewTransaction {
            kind: Kind::Expense,
            amount: amount.parse().unwrap(),
            date: date.to_owned(),
            category: Some("购物".to_owned()),
            note: None,
            channel: Some("银行卡".to_owned()),
            original_id: None,
        }
    }

    fn refund(original: &str, amount: &str, date: &str) -> NewTransaction {
        NewTransaction {
            kind: Kind::Refund,
            amount: amount.parse().unwrap(),
            date: date.to_owned(),
            category: None,
            note: None,
            channel: Some("微信".to_owned()),
            original_id: Some(original.to_owned()),
        }
    }

    fn month(month: &str) -> Filters {
        Filters {
            month: Some(month.to_owned()),
            ..Filters::default()
        }
    }

    #[tokio::test]
    async fn partial_full_and_excess_refunds_preserve_exact_cash_flow_dates() {
        let (_directory, store) = ledger().await;
        let original = store
            .add(expense("98.01", "2026-08-20"), None)
            .await
            .unwrap()
            .transaction
            .id;
        assert_eq!(
            store.get(&original).await.unwrap().refund_status.as_deref(),
            Some("none")
        );
        store
            .add(refund(&original, "0.10", "2026-09-01"), None)
            .await
            .unwrap();
        let partial = store.get(&original).await.unwrap();
        assert_eq!(partial.refund_status.as_deref(), Some("partial"));
        assert_eq!(partial.net_expense.as_deref(), Some("97.91"));
        store
            .add(refund(&original, "97.91", "2026-09-02"), None)
            .await
            .unwrap();
        assert_eq!(
            store.get(&original).await.unwrap().refund_status.as_deref(),
            Some("full")
        );
        store
            .add(refund(&original, "1.99", "2026-09-03"), None)
            .await
            .unwrap();
        let detail = store.get(&original).await.unwrap();
        assert_eq!(detail.refund_total, "100.00");
        assert_eq!(detail.net_expense.as_deref(), Some("-1.99"));
        assert_eq!(detail.excess_refund, "1.99");
        assert_eq!(detail.refund_status.as_deref(), Some("excess"));
        assert_eq!(detail.transaction.channel.as_deref(), Some("银行卡"));
        assert!(
            detail
                .refunds
                .iter()
                .all(|r| r.channel.as_deref() == Some("微信"))
        );
        let august = store.summary(&month("2026-08")).await.unwrap();
        assert_eq!(
            (
                august.expense.as_str(),
                august.refund.as_str(),
                august.net_expense.as_str()
            ),
            ("98.01", "0.00", "98.01")
        );
        let september = store.summary(&month("2026-09")).await.unwrap();
        assert_eq!(
            (
                september.income.as_str(),
                september.refund.as_str(),
                september.net_expense.as_str()
            ),
            ("0.00", "100.00", "-100.00")
        );
        assert_eq!(september.by_category[0].category, "购物");
        assert_eq!(september.by_category[0].net_expense, "-100.00");
    }

    #[tokio::test]
    async fn totals_exceed_i64_without_losing_a_cent() {
        let (_directory, store) = ledger().await;
        let first = store
            .add(expense("92233720368547758.07", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction;
        store
            .add(expense("92233720368547758.07", "2026-09-01"), None)
            .await
            .unwrap();
        store
            .add(expense("0.01", "2026-09-01"), None)
            .await
            .unwrap();
        assert_eq!(
            store
                .get(&first.id)
                .await
                .unwrap()
                .transaction
                .amount
                .minor(),
            i64::MAX
        );
        assert_eq!(
            store.summary(&Filters::default()).await.unwrap().expense,
            "184467440737095516.15"
        );
        store
            .add(
                refund(&first.id, "92233720368547758.07", "2026-09-02"),
                None,
            )
            .await
            .unwrap();
        store
            .add(
                refund(&first.id, "92233720368547758.07", "2026-09-02"),
                None,
            )
            .await
            .unwrap();
        let detail = store.get(&first.id).await.unwrap();
        assert_eq!(detail.refund_total, "184467440737095516.14");
        assert_eq!(detail.net_expense.as_deref(), Some("-92233720368547758.07"));
        assert_eq!(
            store
                .summary(&Filters::default())
                .await
                .unwrap()
                .net_expense,
            "0.01"
        );
    }

    #[tokio::test]
    async fn request_ids_replay_original_response_and_reject_changed_payload() {
        let (_directory, store) = ledger().await;
        let input = expense("35.01", "2026-09-01");
        let first = store.add(input.clone(), Some("add-1")).await.unwrap();
        let second = store.add(input.clone(), Some("add-1")).await.unwrap();
        assert!(second.replayed);
        assert_eq!(first.transaction.id, second.transaction.id);
        assert_eq!(store.history(&first.transaction.id).await.unwrap().len(), 1);
        assert_eq!(store.list(&Filters::default()).await.unwrap().total, 1);
        assert_eq!(
            store
                .add(expense("35.02", "2026-09-01"), Some("add-1"))
                .await
                .unwrap_err()
                .code,
            "IDEMPOTENCY_CONFLICT"
        );
        let patch = UpdateTransaction {
            note: Some("午饭".to_owned()),
            ..UpdateTransaction::default()
        };
        store
            .update(&first.transaction.id, patch.clone(), Some("update-1"))
            .await
            .unwrap();
        assert!(
            store
                .update(&first.transaction.id, patch, Some("update-1"))
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(
            store
                .void(&first.transaction.id, false, Some("add-1"))
                .await
                .unwrap_err()
                .code,
            "IDEMPOTENCY_CONFLICT"
        );
        store
            .void(&first.transaction.id, false, Some("void-1"))
            .await
            .unwrap();
        assert!(
            store
                .void(&first.transaction.id, false, Some("void-1"))
                .await
                .unwrap()
                .replayed
        );
        let replay = store.add(input, Some("add-1")).await.unwrap();
        assert!(replay.replayed);
        assert!(!replay.transaction.voided);
        assert_eq!(store.history(&first.transaction.id).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn concurrent_retries_create_only_one_transaction() {
        let (_directory, store) = ledger().await;
        let input = expense("0.30", "2026-09-01");
        let (first, second) = tokio::join!(
            store.add(input.clone(), Some("same")),
            store.add(input, Some("same"))
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.transaction.id, second.transaction.id);
        assert_ne!(first.replayed, second.replayed);
        assert_eq!(store.summary(&Filters::default()).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn concurrent_partial_updates_preserve_both_changes() {
        let (_directory, store) = ledger().await;
        let id = store
            .add(expense("1.00", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction
            .id;
        let (first, second) = tokio::join!(
            store.update(
                &id,
                UpdateTransaction {
                    note: Some("午饭".to_owned()),
                    ..Default::default()
                },
                None
            ),
            store.update(
                &id,
                UpdateTransaction {
                    channel: Some("支付宝".to_owned()),
                    ..Default::default()
                },
                None
            ),
        );
        first.unwrap();
        second.unwrap();
        let record = store.get(&id).await.unwrap().transaction;
        assert_eq!(record.note.as_deref(), Some("午饭"));
        assert_eq!(record.channel.as_deref(), Some("支付宝"));
        assert_eq!(store.history(&id).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn expense_category_changes_follow_refunds_and_clear_optional_fields() {
        let (_directory, store) = ledger().await;
        store.add_category("代付", Kind::Expense).await.unwrap();
        let id = store
            .add(expense("20.00", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction
            .id;
        let refund = store
            .add(refund(&id, "25.00", "2026-09-02"), None)
            .await
            .unwrap()
            .transaction
            .id;
        store
            .update(
                &id,
                UpdateTransaction {
                    amount: Some("10.00".parse().unwrap()),
                    category: Some("代付".to_owned()),
                    channel: Some(String::new()),
                    note: Some(String::new()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&refund).await.unwrap().transaction.category,
            "代付"
        );
        let detail = store.get(&id).await.unwrap();
        assert_eq!(detail.net_expense.as_deref(), Some("-15.00"));
        assert!(detail.transaction.note.is_none());
        assert!(detail.transaction.channel.is_none());
        assert!(
            store
                .update(
                    &refund,
                    UpdateTransaction {
                        category: Some("购物".to_owned()),
                        ..Default::default()
                    },
                    None
                )
                .await
                .is_err()
        );
        let totals = store
            .summary(&Filters {
                category: Some("代付".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(totals.count, 2);
        assert_eq!(totals.net_expense, "-15.00");
        let audit = store.history(&id).await.unwrap();
        assert_eq!(audit[1].before.as_ref().unwrap()["amount"], "20.00");
        assert_eq!(audit[1].after["amount"], "10.00");
    }

    #[tokio::test]
    async fn cascading_void_is_atomic_audited_and_excluded_from_totals() {
        let (_directory, store) = ledger().await;
        let id = store
            .add(expense("20.00", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction
            .id;
        let r1 = store
            .add(refund(&id, "5.00", "2026-09-02"), None)
            .await
            .unwrap()
            .transaction
            .id;
        let r2 = store
            .add(refund(&id, "20.00", "2026-09-03"), None)
            .await
            .unwrap()
            .transaction
            .id;
        assert_eq!(
            store.void(&id, false, Some("void")).await.unwrap_err().code,
            "HAS_REFUNDS"
        );
        assert_eq!(store.history(&id).await.unwrap().len(), 1);
        let result = store.void(&id, true, Some("void")).await.unwrap();
        assert_eq!(result.affected_ids.len(), 3);
        for id in [&id, &r1, &r2] {
            assert!(store.get(id).await.unwrap().transaction.voided);
            assert_eq!(
                store.history(id).await.unwrap().last().unwrap().action,
                "void"
            );
        }
        assert_eq!(store.get(&id).await.unwrap().refund_total, "0.00");
        assert_eq!(store.list(&Filters::default()).await.unwrap().total, 0);
        let filters = Filters {
            include_voided: true,
            ..Default::default()
        };
        assert_eq!(store.list(&filters).await.unwrap().total, 3);
        assert_eq!(store.summary(&filters).await.unwrap().count, 0);
        assert!(
            store
                .update(
                    &id,
                    UpdateTransaction {
                        amount: Some("50".parse().unwrap()),
                        ..Default::default()
                    },
                    None
                )
                .await
                .is_err()
        );
        assert!(
            store
                .add(refund(&id, "1.00", "2026-09-04"), None)
                .await
                .is_err()
        );
        assert!(store.void(&id, true, Some("void")).await.unwrap().replayed);
    }

    #[tokio::test]
    async fn voiding_one_refund_recalculates_status_without_changing_expense() {
        let (_directory, store) = ledger().await;
        let id = store
            .add(expense("1.00", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction
            .id;
        let refund = store
            .add(refund(&id, "2.00", "2026-09-02"), None)
            .await
            .unwrap()
            .transaction
            .id;
        store.void(&refund, false, None).await.unwrap();
        let detail = store.get(&id).await.unwrap();
        assert_eq!(detail.refund_status.as_deref(), Some("none"));
        assert_eq!(detail.net_expense.as_deref(), Some("1.00"));
        assert_eq!(detail.refunds.len(), 1);
        assert!(!detail.transaction.voided);
        store.void(&id, false, None).await.unwrap();
    }

    #[tokio::test]
    async fn failed_audit_rolls_back_transaction_and_idempotency_key() {
        let (_directory, store) = ledger().await;
        sqlx::query("CREATE TRIGGER fail_audit BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT, 'test failure'); END").execute(&store.pool).await.unwrap();
        assert!(
            store
                .add(expense("1.00", "2026-09-01"), Some("retry"))
                .await
                .is_err()
        );
        assert_eq!(store.list(&Filters::default()).await.unwrap().total, 0);
        let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM idempotency")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(keys, 0);
        sqlx::query("DROP TRIGGER fail_audit")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            !store
                .add(expense("1.00", "2026-09-01"), Some("retry"))
                .await
                .unwrap()
                .replayed
        );
    }

    #[tokio::test]
    async fn malformed_inputs_leave_the_ledger_unchanged() {
        let (_directory, store) = ledger().await;
        for date in [
            "2026-9-01",
            "2026-09-1",
            "2026-02-29",
            "2026-13-01",
            "0000-01-01",
            "2026-09-01 ",
            "2026-09-01T00:00:00Z",
        ] {
            assert!(
                store.add(expense("1.00", date), None).await.is_err(),
                "{date}"
            );
        }
        assert!(store.add(expense("0", "2026-09-01"), None).await.is_err());
        let mut negative = expense("1", "2026-09-01");
        negative.amount = Money::from_minor(-1);
        assert!(store.add(negative, None).await.is_err());
        let mut unknown = expense("1", "2026-09-01");
        unknown.category = Some("不存在".to_owned());
        assert!(store.add(unknown, None).await.is_err());
        assert!(
            store
                .add(expense("1", "2026-09-01"), Some("  "))
                .await
                .is_err()
        );
        let mut wrong_kind = expense("1", "2026-09-01");
        wrong_kind.kind = Kind::Income;
        assert!(store.add(wrong_kind, None).await.is_err());
        let mut with_original = expense("1", "2026-09-01");
        with_original.original_id = Some("missing".to_owned());
        assert!(store.add(with_original, None).await.is_err());
        assert!(
            store
                .add(refund("missing", "1", "2026-09-01"), None)
                .await
                .is_err()
        );
        assert_eq!(store.list(&Filters::default()).await.unwrap().total, 0);
        // Leap day is valid in leap years and remains a date rather than a timezone conversion.
        store.add(expense("1", "2024-02-29"), None).await.unwrap();
    }

    #[tokio::test]
    async fn income_defaults_and_category_types_are_enforced() {
        let (_directory, store) = ledger().await;
        let mut input = expense("500.00", "2026-09-01");
        input.kind = Kind::Income;
        input.category = None;
        let income = store.add(input, None).await.unwrap().transaction;
        assert_eq!(income.category, "其他收入");
        assert!(
            store
                .add(refund(&income.id, "1", "2026-09-02"), None)
                .await
                .is_err()
        );
        assert!(store.add_category("退款", Kind::Refund).await.is_err());
        assert!(store.add_category("购物", Kind::Income).await.is_err());
        assert!(store.add_category(" ", Kind::Expense).await.is_err());
        let summary = store.summary(&Filters::default()).await.unwrap();
        assert_eq!(summary.income, "500.00");
        assert_eq!(summary.balance, "500.00");
        assert!(store.get(&income.id).await.unwrap().net_expense.is_none());
    }

    #[tokio::test]
    async fn date_filters_pagination_and_keywords_are_literal() {
        let (_directory, store) = ledger().await;
        for (date, note) in [
            ("2026-08-31", "100%_x"),
            ("2026-09-01", "100 apples"),
            ("2026-09-02", "100%_x"),
        ] {
            let mut input = expense("1.00", date);
            input.note = Some(note.to_owned());
            store.add(input, None).await.unwrap();
        }
        let matches = store
            .list(&Filters {
                keyword: Some("%_".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(matches.total, 2);
        let page = store
            .list(&Filters {
                month: Some("2026-09".to_owned()),
                limit: 1,
                offset: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].date, "2026-09-01");
        assert_eq!(
            store
                .list(&Filters {
                    from: Some("2026-09-01".to_owned()),
                    to: Some("2026-09-01".to_owned()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .total,
            1
        );
        for invalid_month in ["2026-9", "2026-00", "2026-13", "0000-01"] {
            assert!(store.summary(&month(invalid_month)).await.is_err());
        }
        assert!(
            store
                .list(&Filters {
                    from: Some("2026-09-02".to_owned()),
                    to: Some("2026-09-01".to_owned()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            store
                .list(&Filters {
                    limit: 1001,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            store
                .list(&Filters {
                    limit: -1,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            store
                .list(&Filters {
                    offset: -1,
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn schema_requires_positive_integer_money_and_a_valid_original() {
        let (_directory, store) = ledger().await;
        let id = store
            .add(expense("1.00", "2026-09-01"), None)
            .await
            .unwrap()
            .transaction
            .id;
        assert!(
            sqlx::query("UPDATE transactions SET amount_minor = 1.5 WHERE id = ?")
                .bind(&id)
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE transactions SET amount_minor = 0 WHERE id = ?")
                .bind(&id)
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE transactions SET amount_minor = 'oops' WHERE id = ?")
                .bind(&id)
                .execute(&store.pool)
                .await
                .is_err()
        );
        assert!(sqlx::query("UPDATE transactions SET kind = 'refund', original_id = id, category = NULL WHERE id = ?").bind(&id).execute(&store.pool).await.is_err());
        let storage_type: String =
            sqlx::query_scalar("SELECT typeof(amount_minor) FROM transactions WHERE id = ?")
                .bind(id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(storage_type, "integer");
        assert_eq!(
            store.summary(&Filters::default()).await.unwrap().expense,
            "1.00"
        );
    }

    #[tokio::test]
    async fn opening_requires_initialization_and_preserves_foreign_databases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ledger.sqlite3");
        let error = Store::open(&path, false).await.err().unwrap();
        assert_eq!(error.code, "NOT_INITIALIZED");
        assert!(!path.exists());
        let store = Store::open(&path, true).await.unwrap();
        let app_id: i64 = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(app_id, APPLICATION_ID);
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(journal, "wal");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        store.pool.close().await;
        Store::open(&path, false).await.unwrap().pool.close().await;
        let other = directory.path().join("foreign.sqlite3");
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&other)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TABLE precious (content TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert_eq!(
            Store::open(&other, true).await.err().unwrap().code,
            "NOT_INITIALIZED"
        );
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&other))
            .await
            .unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(tables, ["precious"]);
    }
}
