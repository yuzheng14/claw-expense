use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
};

use chrono::Local;
use clap::{Args, Parser, Subcommand, ValueEnum};
use claw_expense::{
    archive,
    error::{AppError, Result},
    models::{
        ConfirmPendingExpense, Filters, Kind, NewPendingExpense, NewTransaction, PendingFilters,
        UpdateTransaction,
    },
    money::Money,
    store::Store,
};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "claw-expense",
    version,
    about = "精准到分的本地记账 CLI，支持关联退款和 OpenClaw"
)]
struct Cli {
    #[arg(long, global = true, env = "CLAW_EXPENSE_DB", help = "数据库文件路径")]
    db: Option<PathBuf>,
    #[arg(long, global = true, help = "输出结构化 JSON；金额始终为字符串")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum EntryKind {
    Expense,
    Income,
}

impl From<EntryKind> for Kind {
    fn from(value: EntryKind) -> Self {
        match value {
            EntryKind::Expense => Self::Expense,
            EntryKind::Income => Self::Income,
        }
    }
}

#[derive(Args)]
struct EntryArgs {
    #[arg(long, help = "正数金额，最多两位小数，例如 98.01")]
    amount: Money,
    #[arg(long, default_value_t = today(), help = "实际发生日期 YYYY-MM-DD，默认本机今天")]
    date: String,
    #[arg(long)]
    note: Option<String>,
    #[arg(long, help = "支付或收款渠道，仅作为备注，不计算账户余额")]
    channel: Option<String>,
    #[arg(long, help = "稳定的请求标识；重试时相同请求不会重复入账")]
    request_id: Option<String>,
}

#[derive(Args, Default)]
struct FilterArgs {
    #[arg(long, conflicts_with_all = ["from", "to"], help = "月份 YYYY-MM")]
    month: Option<String>,
    #[arg(long, help = "开始日期 YYYY-MM-DD，含当天")]
    from: Option<String>,
    #[arg(long, help = "结束日期 YYYY-MM-DD，含当天")]
    to: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long, help = "按 ID、备注、渠道或外币消费商户查找")]
    search: Option<String>,
}

impl FilterArgs {
    fn filters(self) -> Filters {
        Filters {
            month: self.month,
            from: self.from,
            to: self.to,
            category: self.category,
            keyword: self.search,
            limit: 50,
            ..Filters::default()
        }
    }
}

#[derive(Subcommand)]
enum CategoryCommand {
    /// 查看全部分类
    List,
    /// 新建收入或支出分类
    Add {
        name: String,
        #[arg(long, value_enum)]
        kind: EntryKind,
    },
}

#[derive(Args)]
struct PendingListArgs {
    #[command(flatten)]
    filter: FilterArgs,
    #[arg(long, help = "原币币种，例如 USD、JPY")]
    currency: Option<String>,
    #[arg(long, default_value_t = today(), help = "查询基准日期 YYYY-MM-DD，默认本机今天")]
    as_of: String,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(i64).range(1..=1000))]
    limit: i64,
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    offset: i64,
}

impl PendingListArgs {
    fn filters(self, status: Option<String>, due_only: bool) -> PendingFilters {
        let mut filters = self.filter.filters();
        filters.limit = self.limit;
        filters.offset = self.offset;
        PendingFilters {
            filters,
            status,
            currency: self.currency,
            as_of: self.as_of,
            due_only,
        }
    }
}

#[derive(Subcommand)]
enum PendingCommand {
    /// 消费时先记录原币金额，人民币金额暂不计入收支
    Add {
        #[arg(long)]
        currency: String,
        #[arg(long, help = "正数十进制字符串；USD 最多两位小数，JPY 必须为整数")]
        amount: String,
        #[arg(long, default_value_t = today(), help = "实际消费日期 YYYY-MM-DD")]
        date: String,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        merchant: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 从 stdin 或文件读取待确认账单 JSON；原币金额为字符串，日期必填
    Record {
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 按消费日期由旧到新查询，默认只显示待确认记录
    List {
        #[command(flatten)]
        query: PendingListArgs,
        #[arg(long, value_parser = ["pending", "confirmed", "cancelled", "all"])]
        status: Option<String>,
    },
    /// 查询到期需提醒的待确认账单；只读，不代表已发送提醒
    Due {
        #[command(flatten)]
        query: PendingListArgs,
    },
    /// 查看原币信息，以及已确认后关联的人民币支出
    Show { id: String },
    /// 补齐银行实际人民币金额，只生成一笔关联支出，仍归原消费日期
    Confirm {
        id: String,
        #[arg(long, help = "实际人民币金额，最多两位小数，不是汇率")]
        amount: Money,
        #[arg(long, help = "银行实际入账日 YYYY-MM-DD；不知道时省略")]
        posted_date: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 取消未确认且未实际扣款的消费，保留历史
    Cancel {
        id: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 将未确认账单的下次提醒日设为指定日期
    Snooze {
        id: String,
        #[arg(long, help = "下次提醒日 YYYY-MM-DD，含当天")]
        until: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 查看待确认账单的创建、确认、取消和延后提醒历史
    History { id: String },
}

#[derive(Subcommand)]
enum Command {
    /// 初始化本地账本（人民币）
    Init,
    /// 记录一笔支出或收入
    Add {
        #[arg(value_enum)]
        kind: EntryKind,
        #[arg(long)]
        category: Option<String>,
        #[command(flatten)]
        entry: EntryArgs,
    },
    /// 关联原支出的退款或报销，允许超过原支付金额
    Refund {
        id: String,
        #[command(flatten)]
        entry: EntryArgs,
    },
    /// 从 stdin 或文件读取一笔 JSON 记录；金额必须是字符串
    Record {
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 查询账单，默认不含已作废记录
    List {
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(long, value_parser = ["expense", "income", "refund"])]
        kind: Option<String>,
        #[arg(long)]
        include_voided: bool,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        offset: i64,
    },
    /// 查看账单详情及关联退款
    Show { id: String },
    /// 汇总实际发生期间的收入、支出、退款及分类数据
    Summary {
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// 修改一笔账单；空备注或空渠道表示清空该字段
    Edit {
        id: String,
        #[arg(long)]
        amount: Option<Money>,
        #[arg(long)]
        date: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 作废账单并保留历史记录
    Void {
        id: String,
        #[arg(long, help = "同时作废原支出的全部关联退款")]
        cascade_refunds: bool,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// 查看新增、修改和作废历史
    History { id: String },
    /// 管理分类
    Category {
        #[command(subcommand)]
        command: CategoryCommand,
    },
    /// 外币消费先记录，人民币金额确定后确认入账
    Pending {
        #[command(subcommand)]
        command: PendingCommand,
    },
    /// 完整导出 JSON（包含作废记录、审计和请求标识），不覆盖文件
    Export {
        #[arg(long)]
        output: PathBuf,
    },
    /// 创建一致的 SQLite 备份，不覆盖文件
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    /// 将备份恢复到 --db 指定的新文件，不覆盖现有账本
    Restore {
        #[arg(long)]
        input: PathBuf,
    },
}

fn today() -> String {
    Local::now().date_naive().to_string()
}

fn database_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    explicit.map(Ok).unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "claw-expense")
            .map(|dirs| dirs.data_local_dir().join("ledger.sqlite"))
            .ok_or_else(|| AppError::invalid("无法确定数据目录，请用 --db 指定数据库文件"))
    })
}

fn read_json<T: serde::de::DeserializeOwned>(input: Option<PathBuf>) -> Result<T> {
    let reader: Box<dyn Read> = if let Some(path) = input {
        Box::new(std::fs::File::open(path)?)
    } else {
        if io::stdin().is_terminal() {
            return Err(AppError::invalid(
                "请通过管道传入 JSON，或使用 --input 文件路径",
            ));
        }
        Box::new(io::stdin())
    };
    const MAX_INPUT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    reader.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(AppError::invalid("JSON 输入不能超过 1 MiB"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn run(cli: Cli) -> Result<Value> {
    let db = database_path(cli.db)?;
    if let Command::Restore { input } = &cli.command {
        archive::restore(input, &db).await?;
        return Ok(json!({"db": db, "restored_from": input}));
    }
    let store = Store::open(&db, matches!(cli.command, Command::Init)).await?;
    let result = execute(&store, cli.command, &db).await;
    store.pool.close().await;
    result
}

async fn execute(store: &Store, command: Command, db: &std::path::Path) -> Result<Value> {
    match command {
        Command::Init => Ok(json!({"db": db, "currency": "CNY"})),
        Command::Add {
            kind,
            category,
            entry,
        } => {
            let input = NewTransaction {
                kind: kind.into(),
                amount: entry.amount,
                date: entry.date,
                category,
                note: entry.note,
                channel: entry.channel,
                original_id: None,
            };
            Ok(serde_json::to_value(
                store.add(input, entry.request_id.as_deref()).await?,
            )?)
        }
        Command::Refund { id, entry } => {
            let input = NewTransaction {
                kind: Kind::Refund,
                amount: entry.amount,
                date: entry.date,
                category: None,
                note: entry.note,
                channel: entry.channel,
                original_id: Some(id),
            };
            Ok(serde_json::to_value(
                store.add(input, entry.request_id.as_deref()).await?,
            )?)
        }
        Command::Record { input, request_id } => {
            let input: NewTransaction = read_json(input)?;
            Ok(serde_json::to_value(
                store.add(input, request_id.as_deref()).await?,
            )?)
        }
        Command::List {
            filter,
            kind,
            include_voided,
            limit,
            offset,
        } => {
            let mut filters = filter.filters();
            filters.kind = kind.map(|value| value.parse()).transpose()?;
            filters.include_voided = include_voided;
            filters.limit = limit;
            filters.offset = offset;
            Ok(serde_json::to_value(store.list(&filters).await?)?)
        }
        Command::Show { id } => Ok(serde_json::to_value(store.get(&id).await?)?),
        Command::Summary { filter } => Ok(serde_json::to_value(
            store.summary(&filter.filters()).await?,
        )?),
        Command::Edit {
            id,
            amount,
            date,
            category,
            note,
            channel,
            request_id,
        } => {
            let patch = UpdateTransaction {
                amount,
                date,
                category,
                note,
                channel,
            };
            Ok(serde_json::to_value(
                store.update(&id, patch, request_id.as_deref()).await?,
            )?)
        }
        Command::Void {
            id,
            cascade_refunds,
            request_id,
        } => Ok(serde_json::to_value(
            store
                .void(&id, cascade_refunds, request_id.as_deref())
                .await?,
        )?),
        Command::History { id } => Ok(serde_json::to_value(store.history(&id).await?)?),
        Command::Category { command } => match command {
            CategoryCommand::List => Ok(serde_json::to_value(store.categories().await?)?),
            CategoryCommand::Add { name, kind } => Ok(serde_json::to_value(
                store.add_category(&name, kind.into()).await?,
            )?),
        },
        Command::Pending { command } => execute_pending(store, command).await,
        Command::Backup { output } => {
            archive::backup(store, &output).await?;
            Ok(json!({"output": output, "format": "sqlite"}))
        }
        Command::Export { output } => {
            archive::export(store, &output).await?;
            Ok(json!({"output": output, "format": "claw-expense-export", "version": 2}))
        }
        Command::Restore { .. } => {
            unreachable!("restore is handled before opening the destination")
        }
    }
}

async fn execute_pending(store: &Store, command: PendingCommand) -> Result<Value> {
    match command {
        PendingCommand::Add {
            currency,
            amount,
            date,
            category,
            merchant,
            note,
            channel,
            request_id,
        } => Ok(serde_json::to_value(
            store
                .add_pending(
                    NewPendingExpense {
                        currency,
                        amount,
                        date,
                        category,
                        merchant,
                        note,
                        channel,
                    },
                    request_id.as_deref(),
                )
                .await?,
        )?),
        PendingCommand::Record { input, request_id } => Ok(serde_json::to_value(
            store
                .add_pending(read_json(input)?, request_id.as_deref())
                .await?,
        )?),
        PendingCommand::List { query, status } => Ok(serde_json::to_value(
            store.list_pending(&query.filters(status, false)).await?,
        )?),
        PendingCommand::Due { query } => Ok(serde_json::to_value(
            store.list_pending(&query.filters(None, true)).await?,
        )?),
        PendingCommand::Show { id } => Ok(serde_json::to_value(store.get_pending(&id).await?)?),
        PendingCommand::Confirm {
            id,
            amount,
            posted_date,
            request_id,
        } => Ok(serde_json::to_value(
            store
                .confirm_pending(
                    &id,
                    ConfirmPendingExpense {
                        amount,
                        posted_date,
                    },
                    request_id.as_deref(),
                )
                .await?,
        )?),
        PendingCommand::Cancel { id, request_id } => Ok(serde_json::to_value(
            store.cancel_pending(&id, request_id.as_deref()).await?,
        )?),
        PendingCommand::Snooze {
            id,
            until,
            request_id,
        } => Ok(serde_json::to_value(
            store
                .snooze_pending(&id, &until, request_id.as_deref())
                .await?,
        )?),
        PendingCommand::History { id } => {
            Ok(serde_json::to_value(store.pending_history(&id).await?)?)
        }
    }
}

fn cell(value: &Value, key: &str) -> String {
    let text = value.get(key).and_then(Value::as_str).unwrap_or("");
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn entry_line(entry: &Value) -> String {
    let label = match entry.get("kind").and_then(Value::as_str) {
        Some("income") => "收入",
        Some("refund") => "退款",
        _ => "支出",
    };
    format!(
        "{}  {}  {} 元  {}  {}  {}{}",
        cell(entry, "id"),
        cell(entry, "date"),
        cell(entry, "amount"),
        label,
        cell(entry, "category"),
        cell(entry, "note"),
        if entry["voided"] == true {
            " [已作废]"
        } else {
            ""
        }
    )
}

fn pending_line(entry: &Value) -> String {
    let status = match entry["status"].as_str() {
        Some("confirmed") => "已确认",
        Some("cancelled") => "已取消",
        _ => "待确认人民币金额",
    };
    let mut text = format!(
        "{}  {}  {} {}  [{}]  {}  {}  {}",
        cell(entry, "id"),
        cell(entry, "date"),
        cell(entry, "amount"),
        cell(entry, "currency"),
        status,
        cell(entry, "merchant"),
        cell(entry, "category"),
        cell(entry, "note"),
    );
    if entry["status"] == "pending" {
        text.push_str(&format!("  提醒日 {}", cell(entry, "remind_on")));
    }
    text
}

fn human_output(data: &Value) -> Result<String> {
    if let Some(pending) = data.get("pending") {
        let mut text = pending_line(pending);
        text.push('\n');
        if data["replayed"] == true {
            text.push_str("此前已处理，未重复执行；当前状态请用 pending show 查询。\n");
        }
        if data.get("transaction").is_some_and(Value::is_object) {
            text.push_str("关联人民币账单：\n");
            text.push_str(&entry_line(&data["transaction"]));
            text.push('\n');
        }
        if pending["posted_date"].is_string() {
            text.push_str(&format!("银行入账日 {}\n", cell(pending, "posted_date")));
        }
        if pending["confirmed_at"].is_string() {
            text.push_str(&format!("确认时间 {}\n", cell(pending, "confirmed_at")));
        }
        return Ok(text);
    }
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        let mut text = format!("共 {} 笔，当前显示 {} 笔\n", data["total"], items.len());
        for item in items {
            if item.get("status").is_some() {
                text.push_str(&pending_line(item));
            } else {
                text.push_str(&entry_line(item));
            }
            text.push('\n');
        }
        return Ok(text);
    }
    if data.get("by_category").is_some() {
        let mut text = format!(
            "收入 {} 元 | 支出 {} 元 | 退款 {} 元\n净支出 {} 元 | 收支结余 {} 元 | {} 笔\n",
            cell(data, "income"),
            cell(data, "expense"),
            cell(data, "refund"),
            cell(data, "net_expense"),
            cell(data, "balance"),
            data["count"]
        );
        for item in data["by_category"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "{}：收入 {} / 支出 {} / 退款 {} / 净支出 {} 元\n",
                cell(item, "category"),
                cell(item, "income"),
                cell(item, "expense"),
                cell(item, "refund"),
                cell(item, "net_expense")
            ));
        }
        if data["pending_count"].as_i64().unwrap_or(0) > 0 {
            text.push_str(&format!(
                "另有 {} 笔外币消费待确认，尚未计入人民币收支：\n",
                data["pending_count"]
            ));
            for item in data["pending_by_currency"].as_array().into_iter().flatten() {
                text.push_str(&format!(
                    "{} {}（{} 笔）\n",
                    cell(item, "amount"),
                    cell(item, "currency"),
                    item["count"]
                ));
            }
        }
        return Ok(text);
    }
    if let Some(entry) = data.get("transaction") {
        let mut text = entry_line(entry);
        text.push('\n');
        if data["replayed"] == true {
            text.push_str("重复请求：返回原操作结果，未重复执行。\n");
        }
        if data.get("foreign_expense").is_some_and(Value::is_object) {
            text.push_str("原币消费：\n");
            text.push_str(&pending_line(&data["foreign_expense"]));
            text.push('\n');
        }
        if let Some(total) = data.get("refund_total") {
            text.push_str(&format!("累计退回 {} 元", total.as_str().unwrap_or("0.00")));
            if let Some(net) = data["net_expense"].as_str() {
                text.push_str(&format!(" | 净支出 {net} 元"));
            }
            text.push('\n');
            for refund in data["refunds"].as_array().into_iter().flatten() {
                text.push_str(&entry_line(refund));
                text.push('\n');
            }
        }
        return Ok(text);
    }
    Ok(format!("{}\n", serde_json::to_string_pretty(data)?))
}

fn emit(result: Result<Value>, json_output: bool) -> ExitCode {
    let success = result.is_ok();
    let output = if json_output {
        let value = match result {
            Ok(data) => json!({"ok": true, "data": data}),
            Err(error) => json!({"ok": false, "error": error}),
        };
        serde_json::to_string(&value)
            .map(|text| text + "\n")
            .map_err(AppError::from)
    } else {
        match result {
            Ok(data) => human_output(&data),
            Err(error) => {
                let _ = writeln!(io::stderr().lock(), "{}: {}", error.code, error.message);
                return ExitCode::FAILURE;
            }
        }
    };
    match output {
        Ok(text) => {
            if let Err(error) = io::stdout().lock().write_all(text.as_bytes()) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    return if success {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    };
                }
                let _ = writeln!(io::stderr().lock(), "输出失败: {error}");
                return ExitCode::FAILURE;
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error}");
            return ExitCode::FAILURE;
        }
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let json_requested = std::env::args_os().any(|arg| arg == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return ExitCode::SUCCESS;
            }
            return emit(
                Err(AppError::new("CLI_ARGUMENT_ERROR", error.to_string())),
                json_requested,
            );
        }
    };
    let json_output = cli.json;
    emit(run(cli).await, json_output)
}
