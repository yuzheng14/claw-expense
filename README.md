# claw-expense

给 OpenClaw 和人使用的本地记账 CLI。Rust + Clap + SQLx + SQLite，处理人民币收支、关联退款、外币待确认消费和查询汇总。

> 外币待确认及可选实际发生时间目前在开发分支，尚未发布。`v0.1.0` Release 不包含 `pending` 命令或 `--occurred-at`；review 时请从本分支构建 CLI（下文“构建与运行”）。本 PR 不修改已发布的安装包或创建 Release。

## 安装 Skill（推荐）

目前发布的二进制支持 **macOS ARM64 / Apple Silicon**，运行不需要 Rust 或独立 SQLite。

从 [Release](https://github.com/yuzheng14/claw-expense/releases/latest) 下载 `claw-expense-skill-v0.1.0.tar.gz`，解压后将其中的 `claw-expense` 目录安装到 OpenClaw：

```bash
openclaw skills install ./claw-expense --as claw-expense
```

也可以把这个完整目录复制到 OpenClaw 工作区的 `skills/` 下。已经克隆本仓库时，可以直接安装源目录：

```bash
openclaw skills install ./skills/claw-expense --as claw-expense
```

**只安装 Skill 即可。** Skill 每次通过自带的 `scripts/run.sh` 调用 CLI；如果找不到可用 CLI，就自动从官方 Release 下载固定版本的 macOS ARM64 包，验证 SHA-256、架构和版本，再安装到 `~/.local/bin`。无需手动把这个目录加到 PATH。

```bash
# 在仓库目录也可以直接运行包装脚本，首次缺少 CLI 时会自动下载
bash skills/claw-expense/scripts/run.sh --version
bash skills/claw-expense/scripts/run.sh --db ./ledger.sqlite init
```

`CLAW_EXPENSE_INSTALL_DIR` 可指定可写安装目录。默认下载版本来自 Skill 的 `VERSION` 文件；`CLAW_EXPENSE_VERSION=vX.Y.Z` 可显式选择缺失时下载的版本。已有可用 CLI 会复用，不自动升级或覆盖；脚本不使用 sudo、不修改 shell 配置。当前不为 Linux 或 Intel Mac 提供自动下载包。

直接使用 CLI 的用户，也可以从同一 Release 下载 `claw-expense-v0.1.0-aarch64-apple-darwin.tar.gz`，用 `SHA256SUMS` 校验并解压后运行其中的 `claw-expense`。

## 构建与运行

需要 Rust 1.94+ 和 C 编译器（编译内置 SQLite）。

```bash
cargo build --release --locked
./target/release/claw-expense --help
```

也可以执行 `cargo install --path . --locked` 安装到自己的 Cargo bin 目录。项目附带 `Cargo.lock`；SQLite 编译进程序，不需要单独安装数据库服务。

macOS 如果默认 Xcode 提示许可尚未接受，而已经装有 Command Line Tools，可以仅对构建指定工具目录：

```bash
env DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release --locked
```

## 开始记账

以下示例假设已经安装 `claw-expense`；也可替换为 `./target/release/claw-expense`。

```bash
# 显式指定账本；也可以通过全局 --db 参数指定
export CLAW_EXPENSE_DB="$PWD/ledger.sqlite"
claw-expense init

claw-expense add expense --amount 35 --category 餐饮 --note 午饭
claw-expense add income --amount 5000 --category 工资 --date 2026-09-01
claw-expense list --month 2026-09
claw-expense summary --month 2026-09
```

优先级为 `--db` > `CLAW_EXPENSE_DB` > 操作系统用户数据目录中的 `claw-expense/ledger.sqlite`。`init` 会返回实际路径。除 `init` 和恢复外，其余命令不会创建不存在的账本。

交易 ID 由程序返回，例如 `txn_...`。未指定日期但提供了 `--occurred-at` 时，使用该时间所带时区偏移的当地日期；日期和时间都未提供时才使用运行 CLI 那台机器的本地日期。补记或供 Agent 重试的请求应固定实际发生的日期/时间。支付/收款渠道通过 `--channel` 保存，仅作为交易信息，不计算账户余额。

## 实际发生时间（可选）

人民币收支、退款和外币待确认消费均可记录具体时间。`occurred_at` 表示实际发生时间，和系统录入时间 `created_at`、修改时间 `updated_at`、外币人民币金额确认时间 `confirmed_at` 分开保存。

```bash
# 只知道日期：仍可这样记，occurred_at 为 null，不补成凌晨零点
claw-expense add expense --amount 35 --date 2026-09-24 --note 午饭

# 知道几点几分：保留分钟精度，不擅自补秒；日期可以从时间推导
claw-expense add expense --amount 35 --occurred-at '2026-09-24T12:35+08:00'

# 知道秒数：按提供的精度记录
claw-expense pending add --currency TWD --amount 1234.56 \
  --occurred-at '2026-09-24T14:35:20+08:00' --merchant 海外商户

# 为已有人民币账单补充或修正时间，或清除时间、保留日期
claw-expense edit txn_实际ID --occurred-at '2026-09-24T12:36+08:00'
claw-expense edit txn_实际ID --clear-occurred-at
```

- 时间必须带明确的数字时区偏移（如 `+08:00`、`-07:00`）或 UTC 的 `Z`，格式为 `YYYY-MM-DDTHH:MM[:SS[.小数]]偏移`。秒小数最多 9 位；不静默截断。只知道日期时省略时间，不用录入时刻代替消费时刻。
- 保留原偏移和分钟/秒/小数精度，仅将显式 `+00:00` 规范为 `Z`；不转换为 UTC 后改写消费日期。拒绝无时区、未知偏移 `-00:00`、无效日期、`24:00`、闰秒或过多小数位。
- 同时提供 `--date` 和 `--occurred-at` 时，两者日期必须一致。JSON `record` / `pending record` 仍要求 `date`，可选 `occurred_at` 必须是字符串或 `null`，并满足相同的日期一致性要求。
- `edit --occurred-at` 未提供 `--date` 时会从新时间推导日期。已有时间的账单单独改日期若产生冲突会被拒绝，需同时更新实际时间，或显式 `--clear-occurred-at` 清除时间；不能静默丢失已知时间。新增/编辑的时间同样受审计和请求去重保护。
- 确认外币人民币金额时，原消费时间原样复制到关联的人民币支出，不取确认时刻；后续人民币账单的时间纠错保留审计，原外币记录仍保留最初的消费信息。
- 月报和日期筛选仍按记录的消费当地日期。例如 `2026-09-30T23:30-07:00` 属于 9 月，即使对应 UTC 已到 10 月。提醒仍从消费日期起计算 3 个日历日，不改成 72 小时。
- 列表和详情有时间就展示含偏移的时间，否则只展示日期；列表仍以消费日期为主要排序条件。旧账升级后时间为 `null`，原审计和幂等快照不补写时间。备份/恢复/导出保留该字段。

## 退款与报销

```bash
claw-expense add expense --amount 98 --category 购物 --channel 银行卡 \
  --date 2026-08-31 --note 帮朋友代付 --request-id msg-123-expense-1 --json

# 将 txn_实际ID 换成上一步返回的 ID
claw-expense refund txn_实际ID --amount 100 --channel 微信 \
  --date 2026-09-01 --note 朋友凑整报销 --request-id msg-124-refund-1 --json

claw-expense show txn_实际ID
```

- 退款必须关联一笔未作废的支出；支持部分、多次、全额和超额退款。
- 退款金额本身为正数，累计退款可以超过原支出；超额部分不另记为收入。
- 原单净支出 = 原支出 − 全部未作废的关联退款。上例结果为 `-2.00` 元。
- 状态 `none / partial / full / excess` 分别表示未退款、部分退款、全额退款、超额退回。
- 退款的收款渠道可以与原支付渠道不同；退款分类继承原支出，不能单独指定。
- 月报按每笔支出和退款各自的实际发生日期统计。上例 8 月支出 `98.00`，9 月退款 `100.00`、净支出 `-100.00`。
- 原支出修改分类后，关联退款显示及分类汇总随之变化。修改金额后出现超额退款是合法的。

## 人民币金额精度

金额路径不使用 `f32`、`f64` 或 SQLite 浮点运算。

| 环节 | 规则 |
| --- | --- |
| 输入 | ASCII 十进制字符串，支持 `12`、`12.3`、`12.30` |
| 无效输入 | 零金额、负号、正号、首尾空白、科学计数法、三位及以上小数均拒绝 |
| 存储 | `STRICT` 表的 `INTEGER`，单位为分；单笔范围为 `0.01` 至 `92233720368547758.07` 元 |
| 计算 | Rust `i128` 受检运算汇总；溢出返回错误，禁止截断或回绕 |
| 输出 | 整数格式化为固定两位小数；JSON 金额始终使用字符串 |

`1.005` 和 `1.000` 都不会被自动舍入成 `1.00`。单笔支出、收入、退款均为正金额，由交易类型表达方向；净支出和结余可以为负。汇总不使用 SQLite 的 `SUM`、`TOTAL` 或 `AVG`。

## 外币消费：先记录，再确认人民币金额

适用于信用卡外币消费后，隔几天才能知道实际人民币金额的场景。外币记录处于 `pending` 时只进入待确认清单，不生成零元账单、不猜测汇率，也不混入人民币收支。确认后生成唯一关联的人民币支出，原币信息继续保留。

```bash
# 消费当天记录，不必知道人民币金额；默认消费日后 3 天进入提醒清单
claw-expense pending add --currency JPY --amount 10000 --date 2026-09-30 \
  --merchant 乐天 --category 购物 --channel 招行信用卡 \
  --request-id msg-201-pending-1 --json

# 找到待确认 ID（返回值 pending.id），默认按消费日期由旧到新列出
claw-expense pending list --currency JPY --json
claw-expense pending show <待确认ID> --json

# 确认银行实际人民币金额；不知道银行入账日期可省略 --posted-date
claw-expense pending confirm <待确认ID> --amount 510.38 \
  --posted-date 2026-10-03 --request-id msg-202-confirm-1 --json

# 消费仍属于 9 月，不因 10 月才确认就移到 10 月
claw-expense summary --month 2026-09

# 已确认记录可通过 --status 查看；show 的 transaction.id 是人民币账单 ID
claw-expense pending list --status confirmed --json
claw-expense pending history <待确认ID> --json
```

原币支持 USD、EUR、GBP、HKD、SGD、AUD、CAD、CHF、NZD、TWD（新台币，最多两位小数），JPY、KRW（整数）。原币金额使用对应币种的整数最小单位，单笔最大 `i64::MAX` 个最小单位，分类汇总采用受检 `i128`；所有 JSON 金额仍为字符串。不支持的币种及多余小数会明确拒绝，不做隐式换算或舍入。人民币确认金额沿用上文的精确到分规则。

例如新台币消费可以记录为 `pending add --currency TWD --amount 1234.56 --date 2026-09-24`；之后仍通过 `pending confirm` 补齐实际人民币金额。

Agent 可以从 stdin 用 `pending record` 传 JSON，避免拼接用户文本：

```bash
claw-expense pending record --json --request-id msg-203-pending-1 <<'JSON'
{"currency":"USD","amount":"20.00","date":"2026-09-30","merchant":"订阅服务","channel":"信用卡"}
JSON
```

消费日期 `date` 必填，分类省略时使用“其他支出”。返回值中的 `pending.amount` 是原币金额；未确认时 `transaction` 为 `null`，确认后包含实际人民币账单。银行入账日 `posted_date` 可选，系统确认时间 `confirmed_at` 自动记录，三者分别表达不同事实。

确认和重复请求的规则：

- 确认、关联、审计和请求去重在同一数据库事务内完成；并发确认不会生成两笔支出。
- 不带同一请求标识重复确认，若人民币金额与银行入账日期与首次相同，会返回同一关联支出；不同则返回 `ALREADY_CONFIRMED`，不会覆盖金额。后续纠错使用人民币账单的 `edit`。
- 同一 `--request-id` 和有效载荷重试仍重放原操作快照；需要最新状态时执行 `pending show`。
- 确认后的退款用人民币 `transaction.id` 关联，继续允许部分、多次及超额退款。取消待办不能替代已经发生的退款。
- 取消仅适用于未确认且未实际扣款的交易：`pending cancel <ID> --request-id ...`，保留历史。不支持直接编辑待确认原币信息；填错时在用户确认后取消并重新记录。

月报新增 `pending_count` 和 `pending_by_currency`（每项含 `currency`、`amount`、`count`），按相同消费日期和筛选条件统计。它们不计入原有 `expense` / `net_expense` / `count`，也不将不同币种相加。延后提醒不影响月报的待确认数量；确认会补齐原消费期间的人民币统计。普通 `list` 只列正式人民币账单，外币待确认使用 `pending list`。

### 到期清单与提醒

```bash
# 只读查询，不代表已经发送提醒；不要按当月筛选，以免漏掉跨月记录
claw-expense pending due --as-of 2026-10-03 --json

# 过几天再提醒：设为明确日期，当天重新进入到期清单
claw-expense pending snooze <待确认ID> --until 2026-10-05 \
  --request-id msg-204-snooze-1 --json

# 已取消、已确认记录不会出现在到期清单，完整历史仍可查
claw-expense pending list --status all --limit 100 --offset 0 --json
```

`pending list` / `due` 支持 `--currency`、`--month` 或 `--from/--to`、`--category`、`--search` 和分页。搜索覆盖 ID、商户、备注、渠道。`--as-of` 默认运行机器的当天日期；自动化调用应明确使用用户时区的日期。延后日期不得早于消费日期或当前提醒日。

CLI 不后台运行，安装 Skill 也不会自动建立定时任务。可以对运行在另一台机器上的 OpenClaw 说：

> 请为 claw-expense 设置每天北京时间 18:00 的待确认账单提醒。使用我指定的账本绝对路径，每次查询 pending due，没有待办就不发送；有待办时合并列出商户、消费日期和原币金额，请我核对银行实际人民币金额。复用已有的同类提醒任务，不要重复创建。

Skill 的 [提醒配置指引](skills/claw-expense/references/pending-expenses.md) 说明了执行主机、时区、固定账本路径、渠道、分页和失败处理。需在实际运行 OpenClaw 的环境配置并验证任务；Gateway 必须运行且能访问该账本。本项目不连接银行、不自动发现银行入账、不计算估计汇率，也不在开发者机器上配置你的远端提醒。

## 分类、筛选与纠错

```bash
claw-expense category list
claw-expense category add 宠物 --kind expense
claw-expense category add 副业 --kind income

claw-expense list --from 2026-09-01 --to 2026-09-22 --category 餐饮
claw-expense list --kind refund --search 朋友 --limit 20 --offset 0
claw-expense summary --month 2026-09 --category 购物

claw-expense edit txn_实际ID --amount 96.50 --note 修正实际金额
claw-expense edit txn_实际ID --note "" --channel ""
claw-expense history txn_实际ID --json
claw-expense void txn_退款ID
claw-expense void txn_原支出ID --cascade-refunds
claw-expense list --include-voided
```

内置支出分类：餐饮、交通、购物、住房、娱乐、医疗、其他支出；收入分类：工资、奖金、其他收入。不指定分类时使用对应的“其他”分类，自定义分类需先创建。相同名称不能同时属于收入和支出。

`--month` 与 `--from/--to` 互斥，起止日期均包含当天。列表默认每页 50 笔、最多 1000 笔；汇总计算全部匹配记录，不受分页影响。净支出 = 支出 − 退款，收支结余 = 收入 − 净支出。这里的结余是所选记录的差额，不是银行账户余额。

修改和作废都保留审计历史。已作废账单不可修改，不进入汇总。原支出仍有有效退款时，直接作废会报错，必须先作废退款或明确指定 `--cascade-refunds`。不提供永久删除命令。

## Agent / JSON 接口

所有命令支持全局 `--json`。成功退出码为 0，结果位于 `data`；失败退出码非 0，错误位于 `error`。帮助和版本命令始终输出普通文本。

```json
{"ok":true,"data":{"income":"0.00","expense":"98.00","refund":"100.00","net_expense":"-2.00","balance":"2.00","currency":"CNY","count":2,"by_category":[]}}
```

上面只用于说明字段形状，实际汇总的 `by_category` 会包含分类数据。

```json
{"ok":false,"error":{"code":"INVALID_INPUT","message":"..."}}
```

`record` 接受单笔 JSON 对象（最多 1 MiB），通过 stdin 或 `--input` 文件读取。金额必须是字符串，未知字段会被拒绝：

```bash
claw-expense record --json --request-id msg-125-expense-1 <<'JSON'
{
  "kind": "expense",
  "amount": "98.01",
  "date": "2026-09-22",
  "category": "购物",
  "note": "代付",
  "channel": "银行卡"
}
JSON
```

退款 JSON 使用 `"kind":"refund"`、`"original_id":"txn_..."`，省略 `category`。收入使用 `"kind":"income"`。`date` 是必填字段。

`add`、`refund`、`record`、`edit`、`void` 支持 `--request-id`。同一请求标识和相同有效载荷重试，会返回首次成功时的结果快照，并设置 `replayed: true`；即使之后账单已经修改或作废，也不会重复执行。需要当前状态时使用 `show`。同一标识用于不同载荷时返回 `IDEMPOTENCY_CONFLICT`。

Agent 应使用来源消息 ID + 操作序号生成稳定请求标识，一条消息多笔账需要不同标识；重试必须保留首次的完整载荷（包括日期）。金额会规范化，所以 `98` 与 `98.00` 等价；其他可选字段是否省略会影响载荷。

项目附带 [OpenClaw Skill](skills/claw-expense/SKILL.md)，安装方法见上文。Skill 自动处理缺失的 CLI；账本路径仍通过 `--db` 或 `CLAW_EXPENSE_DB` 指定，位置以实际运行 CLI 的机器为准。安装不会初始化真实账本。

## 导出、备份与恢复

```bash
claw-expense export --output ledger-export.json
claw-expense backup --output ledger-backup.sqlite
claw-expense --db ./restored.sqlite restore --input ledger-backup.sqlite
claw-expense --db ./restored.sqlite summary
```

所有输出和恢复目标都必须是新文件，不覆盖已有文件。恢复前检查应用标识、数据库版本、完整性和外键关系。SQLite 备份包含数据库结构、全部账单、审计历史和请求去重记录；使用一致性快照创建，包含已经提交的 WAL 数据。

JSON 导出 v2 包含 `categories`、`transactions`、`audit_log`、`idempotency`、`pending_expenses`、`pending_audit_log` 六张业务表。人民币 `transactions.amount_minor` 单位为分；外币 `pending_expenses.amount_minor` 单位由对应币种精度决定。归档通过 `amount_units` 显式说明各字段单位；所有整数也编码为字符串，以保持大整数精度。JSON 导出暂不提供导入命令，恢复使用 SQLite 备份。

打开 v0.1.0 账本会自动应用新增迁移，不修改旧迁移文件或清空已有数据；建议升级前先备份。旧 SQLite 备份只要迁移记录是当前程序的已知完整前缀且校验正确即可恢复，首次打开恢复后的账本时自动升级。新版本已迁移的账本不支持用旧二进制打开；回退请使用升级前备份，不要手工删迁移记录。

账本和备份是本地未加密文件；Unix 上新建数据库及归档文件使用仅当前用户可读写的权限。请保存在自己的数据目录中。

## 开发与验证

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

`src/money.rs` 管理精确金额；`src/store.rs` 管理账单、事务、幂等、审计和统计；`src/main.rs` 管理 Clap 参数与输出；`src/archive.rs` 管理导出和备份。金额写入、审计和去重记录在同一事务中提交。

测试全部使用临时数据库，不会读写默认账本。暂不包含账户余额、转账、预算、多币种账户或汇率换算、旧软件批量导入、网页或 Server。

## 发版

GitHub Actions 在 `main` 的 push 和 Pull Request 上执行 CI；推送 `vX.Y.Z` 标签触发 Release 流水线。流水线在 macOS ARM64 上检查格式、测试、Clippy，构建 `aarch64-apple-darwin` 二进制，再发布：

- `claw-expense-vX.Y.Z-aarch64-apple-darwin.tar.gz`：CLI、MIT LICENSE 和 README。
- `claw-expense-skill-vX.Y.Z.tar.gz`：可单独安装的完整 Skill，包括自动安装脚本。
- `SHA256SUMS`：上述归档的 SHA-256 校验值。

发版前同步修改 `Cargo.toml` 的版本、`Cargo.lock`、`skills/claw-expense/VERSION`，并更新版本示例和变更说明；版本号必须与标签一致。流水线不会覆盖已经存在的 Release。发布构建和打包入口见 [.github/workflows/release.yml](.github/workflows/release.yml) 与 [scripts/package-release.sh](scripts/package-release.sh)。

## 协议

[MIT](LICENSE) © 2026 yuzheng14。
