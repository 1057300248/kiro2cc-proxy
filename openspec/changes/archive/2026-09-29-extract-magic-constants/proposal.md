# 变更提案：extract-magic-constants

## 背景

全仓库存在大量散落的数字/字符串魔法字面量，分布于 7 个核心文件，维护者阅读代码时须靠注释猜测含义，且同一语义值在多处重复（如 `"aws-sdk-js/1.0.27"` 在 headers.rs 中重复 4 次、auth 请求超时 `60` 秒重复 4 次），未来升级时容易漏改。本次将这些字面量统一抽取为具名常量，不改变任何运行时行为。

## 目标范围

**在范围内：**
- `src/kiro/provider/headers.rs` — SDK 版本字符串、UA 协议版本、`amz-sdk-request` 中的 max=3 引用
- `src/kiro/provider/errors.rs` — throttle_delay / retry_delay / wait_for_rpm_gate 中的限流退避参数
- `src/kiro/token_manager/refresh.rs` — token 过期判定分钟数、最小 token 长度、HTTP 超时秒数、expires_in 默认值、固定请求头字符串
- `src/anthropic/converter/fields.rs` — max_tokens 上下限值
- `src/anthropic/handlers/post_messages_cc/mod.rs` — 流式 deadline、cache 模拟比例
- `src/model/config.rs` — fingerprint TTL 默认值改写为乘法表达式
- `src/main.rs` — LogCapture ring buffer 容量

**不在范围内：**
- 测试文件中的字面量
- 已是具名常量的值（`CLIENT_TOKEN_DISPLAY_SCALE`、`CLIENT_ASSUMED_CONTEXT_WINDOW`、`MAX_RETRIES_PER_CREDENTIAL`、`UPSTREAM_TIMEOUT_SECS` 等）
- 单字符/字节解析值（`b'\n'` 等）
- 前端源码

## 技术方案

每个文件独立修改：在文件顶部或使用位置附近添加 `const` 声明，替换所有引用位置。
- `headers.rs` 中 `max=3` 改为引用已存在的 `MAX_RETRIES_PER_CREDENTIAL`（来自 `core.rs`，可见性 `pub(crate)`，无需改可见性）
- `model/config.rs` 的 TTL 默认函数返回值改写为可读的 `5 * 60` / `60 * 60` 形式；`fingerprint_max_breakpoints` 默认值提取为 `const DEFAULT_FINGERPRINT_MAX_BREAKPOINTS: usize = 256`（该值非自明数字，需具名）

## 预期影响

- **行为无变化**：所有替换为等值常量，无逻辑改动
- **编译验证**：`cargo check` + `cargo clippy` clean
- **测试无需变更**：引用侧均为纯字面量替换，不影响函数签名或公开接口

## 风险

低。纯重构，值不变，有编译器和 clippy 兜底。  
唯一需注意：`headers.rs` 新增对 `super::core::MAX_RETRIES_PER_CREDENTIAL` 的跨文件引用，该常量已标注 `pub(crate)`，crate 内可直接访问，无需修改可见性。
