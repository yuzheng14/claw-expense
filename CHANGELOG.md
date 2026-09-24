# Changelog

## Unreleased

- CLI 成功执行后检查官方 Release，新版可用时仅向 stderr 输出 `[WARN]`，不自动升级。
- 检查间隔 24 小时、联网超时约 1.5 秒，失败静默降级；支持 `--no-update-check` / `CLAW_EXPENSE_NO_UPDATE_CHECK=1` 完全关闭。
- 使用精确的 SemVer 版本比较，忽略预发布版和构建元数据差异，不影响业务 JSON、账本或退出码。

## v0.1.0

- 初始 Rust CLI：收入、支出、分类、查询、月度汇总、修改、作废与审计。
- 关联退款支持部分、多次和超额退回，收款渠道独立记录。
- 整数分存储、受检 i128 汇总、JSON 金额字符串，拒绝隐式舍入。
- 幂等请求、一致性备份恢复和完整 JSON 导出。
- 提供 macOS ARM64 二进制及 SHA-256 校验文件。
- 提供可单独安装的 OpenClaw Skill，缺少 CLI 时自动从官方 Release 安装。
- 使用 MIT 协议。
