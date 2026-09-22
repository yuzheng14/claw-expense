use crate::money::Money;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Expense,
    Income,
    Refund,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expense => "expense",
            Self::Income => "income",
            Self::Refund => "refund",
        }
    }
}

impl std::str::FromStr for Kind {
    type Err = crate::error::AppError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "expense" => Ok(Self::Expense),
            "income" => Ok(Self::Income),
            "refund" => Ok(Self::Refund),
            _ => Err(crate::error::AppError::invalid(
                "类型必须是 expense、income 或 refund",
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTransaction {
    pub kind: Kind,
    pub amount: Money,
    pub date: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub original_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTransaction {
    pub amount: Option<Money>,
    pub date: Option<String>,
    pub category: Option<String>,
    pub note: Option<String>,
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub id: String,
    pub kind: Kind,
    pub amount: Money,
    pub currency: String,
    pub category: String,
    pub date: String,
    pub note: Option<String>,
    pub channel: Option<String>,
    pub original_id: Option<String>,
    pub voided: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResult {
    pub transaction: TransactionRecord,
    pub replayed: bool,
    pub affected_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionDetail {
    pub transaction: TransactionRecord,
    pub refunds: Vec<TransactionRecord>,
    pub refund_total: String,
    pub net_expense: Option<String>,
    pub refund_status: Option<String>,
    pub excess_refund: String,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub month: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: Option<Kind>,
    pub category: Option<String>,
    pub keyword: Option<String>,
    pub include_voided: bool,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub items: Vec<TransactionRecord>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Serialize)]
pub struct CategorySummary {
    pub category: String,
    pub income: String,
    pub expense: String,
    pub refund: String,
    pub net_expense: String,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub currency: String,
    pub count: i64,
    pub income: String,
    pub expense: String,
    pub refund: String,
    pub net_expense: String,
    pub balance: String,
    pub by_category: Vec<CategorySummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    pub kind: Kind,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    pub id: i64,
    pub transaction_id: String,
    pub action: String,
    pub before: Option<serde_json::Value>,
    pub after: serde_json::Value,
    pub created_at: String,
}
