# 任务清单：extract-magic-constants

## 状态：ARCHIVED

## 任务

- [x] **T1** `headers.rs`：定义 `KIRO_AWS_SDK_JS_VERSION = "1.0.27"` / `AWS_SDK_UA_PROTOCOL_VERSION = "2.1"`，替换 4 处 + 2 处字面量；`amz-sdk-request` 格式串中的 `3` 改为引用 `super::core::MAX_RETRIES_PER_CREDENTIAL`（验证：`cargo check` clean）
- [x] **T2** `errors.rs`：定义 `THROTTLE_BASE_MS: u64 = 2000` / `THROTTLE_STEP_MS: u64 = 1000` / `THROTTLE_MAX_MS: u64 = 8_000` / `THROTTLE_JITTER_MAX_MS: u64 = 1500` / `RETRY_BACKOFF_MAX_EXPONENT: usize = 6` / `RPM_GATE_DEFAULT_WAIT_SECS: u64 = 3` / `RPM_GATE_MAX_WAIT_SECS: u64 = 5`，替换对应字面量（验证：`cargo check` clean）
- [x] **T3** `refresh.rs`：定义 `TOKEN_EXPIRY_LEAD_MINS: i64 = 5` / `TOKEN_EXPIRING_SOON_MINS: i64 = 10` / `REFRESH_TOKEN_MIN_LENGTH: usize = 100` / `KIRO_HTTP_SHORT_TIMEOUT_SECS: u64 = 60`（覆盖 auth/IdC/external_idp/getUsageLimits 四处调用）/ `DEFAULT_TOKEN_EXPIRES_IN_SECS: i64 = 3600` / `AMZ_SDK_REQUEST_SINGLE_ATTEMPT: &str = "attempt=1; max=1"` / `LIST_MODELS_TIMEOUT_SECS: u64 = 15`，替换 8 处字面量（4 次 `60` 超时、1 次 `3600`、1 次 `15`、1 处 `"attempt=1; max=1"` 字符串）（验证：`cargo check` clean）
- [x] **T4** `converter/fields.rs`：定义 `MAX_OUTPUT_TOKENS_LARGE_WINDOW: i32 = 128_000` / `MAX_OUTPUT_TOKENS_STANDARD: i32 = 64_000` / `KIRO_MIN_MAX_TOKENS: i32 = 1_024`，替换 3 处字面量（验证：`cargo check` clean）
- [x] **T5** `post_messages_cc/mod.rs`：定义 `CC_STREAM_DEADLINE_SECS = 300u64` / `CC_CACHE_READ_RATIO = 0.85_f64` / `CC_CACHE_CREATION_RATIO = 0.1_f64`，替换 3 处字面量（验证：`cargo check` clean）
- [x] **T6** `model/config.rs`：① 将 `default_fingerprint_ttl_5m` 返回值改为 `5 * 60`，`default_fingerprint_ttl_1h` 改为 `60 * 60`（可读性改写，不提取为常量）；② 提取 `const DEFAULT_FINGERPRINT_MAX_BREAKPOINTS: usize = 256`，将 `default_fingerprint_max_breakpoints` 返回值改为引用该常量（该值非自明，需具名）（验证：`cargo check` clean）
- [x] **T7** `main.rs`：定义 `const LOG_CAPTURE_CAPACITY: usize = 1000`，替换 `LogCapture::new(1000)`（验证：`cargo check` clean）
- [x] **T8** 全量验证：`cargo fmt && cargo clippy -- -D warnings && cargo test`

## 验收标准

- [ ] `cargo fmt` 无 diff
- [ ] `cargo clippy -- -D warnings` 零告警
- [ ] `cargo test` 全部通过
- [ ] 无任何运行时行为变更（纯字面量替换为等值常量）
