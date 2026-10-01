### 通过 new-api 反代并共享上游 Key

**New API 不需要改源码或合入专用 PR。**只使用已有的渠道 Header Override；身份规范化、HMAC 与续接隔离全部在 Kiro 内完成。

在 Kiro 的 `config.json` 设置（移除旧 `responseStoreTenantHeader`，两种模式不能并用）：

```json
{"responseStoreClientAuthorizationHeader":"X-Kiro2CC-Client-Authorization"}
```

设置 Kiro 环境变量 `RESPONSE_STORE_HMAC_KEY` 为随机生成并妥善保存的 **64 位十六进制字符串**（32 字节），例如在服务器执行 `openssl rand -hex 32` 后通过密钥管理或受保护的环境文件注入。不要提交密钥到仓库。头名称也可用 `RESPONSE_STORE_CLIENT_AUTHORIZATION_HEADER` 配置；空值、无效头、缺失/非法密钥或双模式会拒绝启动，不降级到共享范围。

在 New API 的 Kiro 渠道中，Header Override 使用下面的明确配置：

```json
{"X-Kiro2CC-Client-Authorization":"{client_header:Authorization}"}
```

正常上游 `Authorization` 仍由渠道设置生成，使用 Kiro 网关 Key。不要覆盖它，也不要把 `{api_key}` 当作客户凭据。**禁止该内部头的通配、正则、pass_headers 或动态参数覆盖透传**，因为源 Authorization 缺失时旧网关会跳过显式覆盖；推荐此渠道的整个头覆盖仅使用上述一项。清除旧 `X-Kiro2CC-Tenant`/`{authenticated_tenant}` 配置，无需合入 New API PR #12。

只支持经核对的标准 New API Authorization 子集：`Bearer`/`bearer` 或裸 Key，可带 `sk-`，基础 Key 为 32–128 位 ASCII 字母数字且区分大小写。保留该路径实际完成鉴权的前提；`midjourney-proxy`/`mj-api-secret` 备用鉴权、渠道选择后缀、特殊路由转换不猜测支持。不通过 `user`、IP、body metadata 或任意租户头补全身份。

有状态请求（默认 `store=true`）和任何续接均需要有效客户身份。缺失头时只允许明确 `store=false`、无 previous_response_id/item_reference 的无状态请求；渠道自检优先使用 Chat 或 models。畸形、重复、逗号合并、超长、未展开占位符或误传网关 Key 会拒绝，不能伪装成缺失身份继续。

Kiro 在鉴权边界生成派生范围后移除原始凭据头，不传给模型上游、不存入续接历史或错误信息。Kiro 必须是可信内部服务，公网无法绕过 New API 直接调用；跨主机使用受保护的传输。HMAC 是命名空间派生，不是独立鉴权，也不能弥补错误网关配置。日志/APM 不应捕获完整请求头。

Base URL 使用实际 Kiro 地址与端口，不追加 `/v1`；默认端口为 8080，5678 仅为旧示例。保持 `disable_store` 关闭，对需要续接的轮次使用 `store=true`。状态仍为进程内存，多实例需同实例粘滞；重启会丢历史。HMAC 密钥轮换会改变范围；同一客户不同 API Key 不共享历史。Kiro 的共享 Key 用量/RPM/额度仍聚合，New API 继续负责终端计费和限流。

更多迁移与边界见 [零 New API 源码改动接入说明](docs/new-api-config-only.md) 和 [功能集成审计](docs/stateful-integration-audit.md)。

## 回归与信任边界

本模式是 Kiro 在可信网关后独立派生客户凭据指纹，不读取 New API 数据库、不调用其鉴权接口、不复制钱包/计费逻辑，也不要求客户双 Key。New API 原本的鉴权、额度和路由行为不改变。它不是对任意版本或任意自定义认证插件的通用兼容声明；认证优先级变化应重新核对。

网关必须覆盖内部头且禁止其源值缺失时的透传路径；Kiro 无法从已覆盖为共享 Key 的请求恢复不存在的客户身份，也无法辨识网关错误地声称的身份。不要把该头送给不受你控制的上游服务。

回归测试包含规范化等价形式、不同客户/大小写隔离、RFC 4231 HMAC 向量、错误配置拒绝、凭据剥离、真实 Kiro 鉴权中间件+存储的循环测试、跨租户 continuation 拒绝、store=false 不保存及不能绕过读取校验、重复头和伪造 body/租户字段、无配置收到凭据头的失败闭合。

原有 Responses 终态、EOF、工具 JSON、引用去重、压缩替换、工具名还原和 donor 集成不回退。当前轮测试结果以最终提交的 CI 日志为准；历史 f5958804 的日志实际是 724 passed，先前 PR 说明中的 727 为误报，不能沿用。

本说明不宣称共享/持久化 store、真实上游模型联调、生产部署或交互式 AWS SSO 已完成。旧式 trusted tenant header 模式仅为兼容其他真正产生已认证身份的网关保留，不是此接入方式。
