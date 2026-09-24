---
name: claw-expense
description: 使用 claw-expense CLI 记录人民币收支、关联退款及待确认人民币金额的外币消费，查询或修正账单。适用于日常记账、代付报销、外币账单补录和待确认提醒。
license: MIT
metadata:
  openclaw:
    os: [darwin]
---

# claw-expense

通过 `exec` 调用本 Skill 的包装脚本，首次运行自动安装缺失的 CLI。所有业务调用加 `--json`，仅当退出码为 0 且 `ok: true` 时报告成功。

## 安装与调用

当前二进制仅支持 **macOS Apple Silicon（ARM64）**。本 Skill 自带安装脚本，不要求用户提前安装 Rust 或 claw-expense，也不需要管理员权限。

每次调用都使用如下入口，`{baseDir}` 由 OpenClaw 替换为当前 Skill 目录：

```bash
bash "{baseDir}/scripts/run.sh" --help
bash "{baseDir}/scripts/run.sh" --json list --month 2026-09
```

包装脚本优先复用 PATH 或安装目录中已有的可用 CLI；若缺失，则从本项目官方 GitHub Release 下载 `VERSION` 文件指定版本（当前 `v0.1.0`）的 `aarch64-apple-darwin` 包，核对 `SHA256SUMS`，验证二进制架构和版本后原子安装。校验失败立即停止，不运行未通过校验的文件。

- 默认安装目录为 `~/.local/bin`。可以通过 `CLAW_EXPENSE_INSTALL_DIR` 指定可写目录，脚本不改 shell 配置；即使该目录不在 PATH，包装脚本仍可正常执行。
- `CLAW_EXPENSE_VERSION=vX.Y.Z` 仅改变缺失 CLI 时下载的版本，不自动升级或覆盖已有 CLI。
- 所有 CLI 参数、stdin 和退出码原样传递。安装诊断写入 stderr，不污染 stdout 中的业务 JSON。
- 下载来源固定为 `https://github.com/yuzheng14/claw-expense/releases`。网络失败或不支持的平台应如实报告，不能改用未知来源或跳过校验。
- 安装和执行发生在 `exec` 所在的机器。Linux 容器或 Intel Mac 不支持自动安装当前 ARM64 包。

下面提到的 `record`、`list` 等命令，都通过 `bash "{baseDir}/scripts/run.sh"` 执行；不要因 CLI 不在 PATH 就跳过 Skill。

## 定位账本

使用用户指定的 `--db` 或执行环境的 `CLAW_EXPENSE_DB`。路径属于运行 CLI 的机器。不存在的账本会返回 `NOT_INITIALIZED`，用户要求建立新账本时才执行 `init`。不要随意更换路径，以免把同一个人的账记到不同文件。

## 精确记录

优先用 `record --json --request-id <稳定标识>`，通过 stdin 传递 JSON；不要把用户备注拼接为未经引用的 shell 代码。

```json
{"kind":"expense","amount":"98.01","date":"2026-09-22","category":"购物","note":"帮朋友代付","channel":"银行卡"}
```

- 金额必须是十进制字符串，正数且最多两位小数。不要用浮点计算、静默舍入或将 JSON 金额转成数字。
- 明确实际发生日期 `YYYY-MM-DD`。用用户的日期和时区解释“昨天”等表达，避免依赖执行机器的当天日期；保留解析结果用于重试。
- 分类省略时使用其他支出/其他收入。自定义分类先用 `category add <名称> --kind expense|income` 创建。
- 从来源消息 ID 与操作序号生成请求标识，例如 `msg-123-expense-1`。相同操作重试使用相同标识和载荷；一条消息多笔账各有标识。
- `IDEMPOTENCY_CONFLICT` 表示标识已用于不同载荷，应核对已有记录，不能通过换标识盲目重试。
- 返回的 `transaction.id` 用于查询、退款、修改和作废；`replayed: true` 表示首次操作的结果快照，当前状态用 `show` 获取。

## 退款和代付返还

商家退款、朋友报销或代付返还都可作为原支出的关联退款。先通过 `list` / `show` 找到原支出 ID；有多个合理候选时澄清，不猜测关联对象。

```json
{"kind":"refund","amount":"100.00","date":"2026-09-23","original_id":"txn_实际ID","note":"朋友凑整报销","channel":"微信"}
```

退款省略 `category`，分类继承原支出；收款渠道独立于原支付渠道。允许部分、多次以及超额退款：支出 98 元、退回 100 元，净支出为 -2 元，不拆成额外收入。

## 外币待确认账单

遇到外币消费但人民币金额尚未确定，或用户要补齐人民币金额、取消/延后提醒时，读取 [references/pending-expenses.md](references/pending-expenses.md)。不要估算成人民币、填 0 元或只记在聊天记忆里。

首次使用此流程先通过包装脚本执行 `pending --help` 检查 CLI 能力。若不支持，应说明需要包含此功能的新版 CLI，不能改用人民币命令硬记。当前发布的 `v0.1.0` 不含此功能；未发版的开发分支需先构建 CLI 并放入 PATH，再使用本 Skill。安装器不会自动覆盖已有 CLI。

自动提醒需要另行配置 OpenClaw 定时任务；仅安装 Skill 或创建待确认记录不会启动后台提醒。用户要求设置提醒时，按上述参考中的配置流程处理，不在本机擅自配置远端 OpenClaw。

## 查询与纠错

- `list --month YYYY-MM`：查询账单，注意分页返回的 `total`，不能将一页当全部账单。
- `summary --month YYYY-MM`：由 CLI 计算收入、支出、退款、净支出和分类汇总，直接使用返回的金额字符串。
- 汇总中的 `pending_count` / `pending_by_currency` 是同一消费期间尚未确认的外币账单，不计入人民币总额；有待确认记录时必须同时告知用户统计尚不完整，不跨币种相加。
- 月报按各条账单的实际发生日期，`show` 的原单净支出则包含所有未作废关联退款，因此跨月时两者统计范围不同。
- `edit <ID> --amount/--date/--category/--note/--channel`：按用户的纠错要求修改，修改后出现超额退款合法。
- `void <ID>`：作废并保留历史；原单有关联退款时需用户意图涵盖这些退款，才使用 `--cascade-refunds` 一并作废。
- 修改和作废也带稳定的 `--request-id`。`history <ID>` 可查看变更记录。
- `backup --output <新文件>` 保存完整账本；`restore --input <备份>` 必须用 `--db` 指定新目标文件。
