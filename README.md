# claw-expense

给 OpenClaw 和人使用的本地记账 CLI。Rust + Clap + SQLx + SQLite，第一版只处理人民币收支、关联退款和查询汇总。

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

### 更新可用提醒

CLI 在业务命令成功、输出结果后，默认最多每 24 小时向本项目的 GitHub Release API 检查一次最新正式版。发现版本高于当前 CLI 时，在 **stderr** 输出一行：

```text
[WARN] 有更新可用：claw-expense 0.1.0 → 0.2.0；请手动更新：https://github.com/yuzheng14/claw-expense/releases/tag/v0.2.0（不会自动升级）
```

上面是示例，不表示该版本已发布。缓存有效期间不重复联网，但每次成功命令仍会提示缓存中已知的新版，直到升级或关闭提醒。按 SemVer 比较版本，不提示草稿、预发布版、相同版本或降级；构建元数据不影响版本高低。

- **只提醒，不自动下载、替换程序或修改账本。** `--json` 的 stdout 和退出码不受提醒影响，stderr 有 `[WARN]` 不代表记账失败。
- 使用系统 `curl`，仅请求公开 Release 元数据，不使用 GitHub 登录凭据、不发送账单内容、不读取 `.curlrc`。缺少 `curl`、网络错误、API 限流和异常响应均静默跳过。
- 每次联网检查最多等待约 1.5 秒，失败也缓存检查时间；不会在每次记账时反复等待网络。`--help`、`--version`、参数错误和业务失败不触发检查。
- 缓存保存到操作系统的 claw-expense 缓存目录（macOS 为 `~/Library/Caches/claw-expense`），不是 SQLite 账本。可用 `CLAW_EXPENSE_UPDATE_CACHE_DIR` 指定独立目录；缓存不可写时跳过联网检查。

完全离线或不需要提醒时，使用以下任一方式；关闭后不读写检查缓存、不联网，也不显示已缓存的提醒：

```bash
claw-expense --no-update-check --json list
CLAW_EXPENSE_NO_UPDATE_CHECK=1 claw-expense --json list
# Skill 包装脚本也原样传递该选项
bash skills/claw-expense/scripts/run.sh --no-update-check --json list
```

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

交易 ID 由程序返回，例如 `txn_...`。未指定日期时使用运行 CLI 那台机器的本地日期；补记或供 Agent 重试的请求应明确填写 `--date YYYY-MM-DD`。支付/收款渠道通过 `--channel` 保存，仅作为交易信息，不计算账户余额。

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

## 金额精度

金额路径不使用 `f32`、`f64` 或 SQLite 浮点运算。

| 环节 | 规则 |
| --- | --- |
| 输入 | ASCII 十进制字符串，支持 `12`、`12.3`、`12.30` |
| 无效输入 | 零金额、负号、正号、首尾空白、科学计数法、三位及以上小数均拒绝 |
| 存储 | `STRICT` 表的 `INTEGER`，单位为分；单笔范围为 `0.01` 至 `92233720368547758.07` 元 |
| 计算 | Rust `i128` 受检运算汇总；溢出返回错误，禁止截断或回绕 |
| 输出 | 整数格式化为固定两位小数；JSON 金额始终使用字符串 |

`1.005` 和 `1.000` 都不会被自动舍入成 `1.00`。单笔支出、收入、退款均为正金额，由交易类型表达方向；净支出和结余可以为负。汇总不使用 SQLite 的 `SUM`、`TOTAL` 或 `AVG`。

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

JSON 导出是完整的数据归档，包含 `categories`、`transactions`、`audit_log`、`idempotency` 四张业务表。原始 `amount_minor` 单位为分；所有整数也编码为字符串，以保持大整数精度。JSON 导出暂不提供导入命令，恢复使用 SQLite 备份。

账本和备份是本地未加密文件；Unix 上新建数据库及归档文件使用仅当前用户可读写的权限。请保存在自己的数据目录中。

## 开发与验证

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

`src/money.rs` 管理精确金额；`src/store.rs` 管理账单、事务、幂等、审计和统计；`src/main.rs` 管理 Clap 参数与输出；`src/archive.rs` 管理导出和备份。金额写入、审计和去重记录在同一事务中提交。

测试全部使用临时数据库，不会读写默认账本。暂不包含账户余额、转账、预算、多币种、旧软件批量导入、网页或 Server。

## 发版

GitHub Actions 在 `main` 的 push 和 Pull Request 上执行 CI；推送 `vX.Y.Z` 标签触发 Release 流水线。流水线在 macOS ARM64 上检查格式、测试、Clippy，构建 `aarch64-apple-darwin` 二进制，再发布：

- `claw-expense-vX.Y.Z-aarch64-apple-darwin.tar.gz`：CLI、MIT LICENSE 和 README。
- `claw-expense-skill-vX.Y.Z.tar.gz`：可单独安装的完整 Skill，包括自动安装脚本。
- `SHA256SUMS`：上述归档的 SHA-256 校验值。

发版前同步修改 `Cargo.toml` 的版本、`Cargo.lock`、`skills/claw-expense/VERSION`，并更新版本示例和变更说明；版本号必须与标签一致。流水线不会覆盖已经存在的 Release。发布构建和打包入口见 [.github/workflows/release.yml](.github/workflows/release.yml) 与 [scripts/package-release.sh](scripts/package-release.sh)。

## 协议

[MIT](LICENSE) © 2026 yuzheng14。
