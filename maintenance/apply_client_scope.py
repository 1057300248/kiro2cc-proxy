#!/usr/bin/env python3
"""One-shot, exact-source integration on the existing draft branch only.
No runtime deployment, New API write, real credential, or external auth service.
The temporary script/workflow are removed after validation and publication.
"""
from pathlib import Path
changed=set()
def put(path,text):
    p=Path(path); p.parent.mkdir(parents=True,exist_ok=True)
    if not p.exists() or p.read_text()!=text:
        p.write_text(text); changed.add(path)
def rep(path,old,new):
    s=Path(path).read_text()
    if new in s: return
    assert s.count(old)==1,(path,old[:100],s.count(old))
    put(path,s.replace(old,new))

rep('src/model/mod.rs','pub mod config;','pub mod config;\npub(crate) mod client_auth_scope;')
p='src/model/config.rs'
rep(p,'    pub response_store_tenant_header: Option<String>,','''    pub response_store_tenant_header: Option<String>,

    /// 从现有 New API Header Override 转发的客户 Authorization 派生续接范围。
    /// 与 response_store_tenant_header 互斥；HMAC 密钥仅从环境读取，不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_store_client_authorization_header: Option<String>,''')
rep(p,'            response_store_tenant_header: None,','            response_store_tenant_header: None,\n            response_store_client_authorization_header: None,')
rep(p,'    /// - `RESPONSE_STORE_TENANT_HEADER`: 可信反代租户身份请求头','''    /// - `RESPONSE_STORE_TENANT_HEADER`: 可信反代租户身份请求头
    /// - `RESPONSE_STORE_CLIENT_AUTHORIZATION_HEADER`: 内部转发客户凭据头（新模式）
    /// - `RESPONSE_STORE_HMAC_KEY`: 新模式的 32 字节密钥，64 位 hex，由启动逻辑读取''')
rep(p,'        // CacheSimulationConfig 嵌套字段覆盖','''        if let Ok(v) = env::var("RESPONSE_STORE_CLIENT_AUTHORIZATION_HEADER") {
            // 空字符串不得悄悄禁用文件中启用的隔离模式；启动时验证并拒绝。
            self.response_store_client_authorization_header = Some(v);
        }

        // CacheSimulationConfig 嵌套字段覆盖''')
s=Path(p).read_text()
marker='\n#[cfg(test)]\nmod client_scope_config_tests {'
if marker not in s:
    put(p,s+r'''
#[cfg(test)]
mod client_scope_config_tests {
    use super::*;
    #[test]
    fn new_mode_is_opt_in_and_has_no_serialized_secret() {
        let off: Config=serde_json::from_str("{}").unwrap();
        assert!(off.response_store_client_authorization_header.is_none());
        let on: Config=serde_json::from_str(r#"{"responseStoreClientAuthorizationHeader":"X-Kiro2CC-Client-Authorization"}"#).unwrap();
        assert_eq!(on.response_store_client_authorization_header.as_deref(),Some("X-Kiro2CC-Client-Authorization"));
        let encoded=serde_json::to_value(&on).unwrap();
        assert!(encoded.get("responseStoreHmacKey").is_none());
        assert!(serde_json::from_str::<Config>(r#"{"responseStoreClientAuthorizationHeader":false}"#).is_err());
    }
}
''')
p='src/main.rs'
rep(p,'    if let Some(header_name) = config.response_store_tenant_header.as_deref() {','''    anthropic_app_state.response_store_client_auth =
        model::client_auth_scope::ClientAuthScope::from_config(&config)
            .unwrap_or_else(|message| {
                tracing::error!("{}", message);
                std::process::exit(1);
            })
            .map(Arc::new);
    if anthropic_app_state.response_store_client_auth.is_some() {
        tracing::info!("已启用 Kiro 侧客户凭据续接隔离；无需 New API 源码补丁");
    }
    if let Some(header_name) = config.response_store_tenant_header.as_deref() {''')
p='src/anthropic/middleware.rs'
rep(p,'use crate::model::api_key::{ApiKeyAuthResult, ApiKeyManager};','''use crate::model::api_key::{ApiKeyAuthResult, ApiKeyManager};
use crate::model::client_auth_scope::{ClientAuthScope, DEFAULT_CLIENT_AUTH_HEADER};''')
rep(p,'    pub(crate) response_store_tenant_header: Option<String>,','''    pub(crate) response_store_tenant_header: Option<String>,
    /// Optional gateway-only client credential scope; raw material is stripped here.
    pub(crate) response_store_client_auth: Option<Arc<ClientAuthScope>>,''')
rep(p,'            response_store_tenant_header: None,','            response_store_tenant_header: None,\n            response_store_client_auth: None,')
rep(p,'    let Some(key) = auth::extract_api_key(&request) else {','''    let gateway_key = auth::extract_api_key(&request);
    // Consume the sensitive header before any handler/forwarder sees this request.
    // Authentication still uses the Kiro gateway key, never the downstream key.
    let client_scope = if let Some(mode) = &state.response_store_client_auth {
        Some(mode.take_scope(request.headers_mut(), gateway_key.as_deref()))
    } else if request.headers().contains_key(DEFAULT_CLIENT_AUTH_HEADER) {
        request.headers_mut().remove(DEFAULT_CLIENT_AUTH_HEADER);
        Some(Err("Client authorization forwarding was received but its Kiro isolation mode is not enabled."))
    } else {
        None
    };
    let Some(key) = gateway_key else {''')
rep(p,'                // 懒激活：首次使用时激活 key','''                let response_store_scope = match client_scope {
                    Some(Ok(scope)) => scope,
                    Some(Err(message)) => {
                        return (StatusCode::BAD_REQUEST, Json(ErrorResponse::new(
                            "invalid_request_error", message,
                        ))).into_response();
                    }
                    None => response_store_scope(&request, state.response_store_tenant_header.as_deref()),
                };
                // 懒激活：首次使用时激活 key''')
rep(p,'''                let response_store_scope =
                    response_store_scope(&request, state.response_store_tenant_header.as_deref());
                request.extensions_mut().insert(ApiKeyContext {''','''                request.extensions_mut().insert(ApiKeyContext {''')
p='src/openai/handlers.rs'
rep(p,'    if state.response_store_tenant_header.is_some() && owner_scope.is_none() {','''    if state.response_store_client_auth.is_some() {
        if let Err(message) = crate::model::client_auth_scope::check_request_scope(&incoming, owner_scope) {
            return error::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                message,
                Some("client_identity_required"),
            );
        }
    } else if state.response_store_tenant_header.is_some() && owner_scope.is_none() {''')
p='src/model/client_auth_scope.rs'
s=Path(p).read_text()
if 'mod integration_tests;' not in s:
    put(p,s+'\n#[cfg(test)]\n#[path = "client_auth_scope_integration_tests.rs"]\nmod integration_tests;\n')
changed.update(['src/model/client_auth_scope.rs','src/model/client_auth_scope_integration_tests.rs'])

# Replace both previous required-companion recipes, rather than appending a disclaimer.
zh='''### 通过 new-api 反代并共享上游 Key

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

'''
en='''### Reverse proxy through new-api with a shared upstream key

**No New API source patch, rebuild, or companion PR is required.** Its existing Header Override copies client Authorization; Kiro alone canonicalizes and derives the continuation scope.

Kiro config.json (remove the legacy `responseStoreTenantHeader`; the modes are mutually exclusive):

```json
{"responseStoreClientAuthorizationHeader":"X-Kiro2CC-Client-Authorization"}
```

Set Kiro's `RESPONSE_STORE_HMAC_KEY` to a securely generated, stable **64-character hexadecimal key** (32 bytes), for example using `openssl rand -hex 32` on the server and protected secret/environment injection. Never commit it. `RESPONSE_STORE_CLIENT_AUTHORIZATION_HEADER` may configure the header name. Bad/empty header configuration, missing/invalid key, and dual modes fail startup instead of disabling isolation.

Use this existing New API channel Header Override:

```json
{"X-Kiro2CC-Client-Authorization":"{client_header:Authorization}"}
```

Keep normal upstream Authorization set to the Kiro channel credential. Never use `{api_key}` for the client header. **Do not enable wildcard/regex/pass_headers or dynamic parameter-header mutations for this internal header**: an absent source Authorization causes the old gateway to skip its explicit override. Prefer exactly the single override above for this channel. Remove the old X-Kiro2CC-Tenant / `{authenticated_tenant}` recipe; companion New API PR #12 is unnecessary.

The supported grammar deliberately matches only the verified standard Authorization subset: Bearer/bearer or a raw key, optional sk- prefix, a case-sensitive 32–128 character ASCII alphanumeric base key. The forwarded Authorization must be the credential that authenticated the request. Alternate midjourney-proxy/mj-api-secret authentication, channel-selection suffixes, or unverified route transformations are rejected/not supported; Kiro does not infer identity from body user/metadata, IP, or arbitrary tenant headers.

Storage (default store=true) and ALL continuations require a client identity. Missing credentials permit only explicit store=false with no previous_response_id/item_reference; use Chat or models for channel self-tests. Malformed, duplicated, coalesced, oversized, literal-placeholder, or gateway-key-as-client values fail even for stateless requests.

Kiro strips the raw client credential header at its authentication boundary, retains only an HMAC scope in request identity/store, and never sends the header to model upstreams. This requires a trusted private gateway deployment and protected transport; HMAC is a namespace, not independent authentication or protection against a misconfigured gateway. Do not log/capture complete headers in proxies/APM.

Use the actual reachable Kiro address/port without /v1 (default 8080, not the old example 5678). Keep disable_store off; use store=true for responses to resume. State remains process-local: restarts lose histories and multiple instances need affinity. HMAC key rotation changes scopes; different downstream API keys remain isolated even under one user. Shared Kiro-key usage/RPM/quota remain aggregated, with end-user billing and limits owned by New API.

See [config-only integration](docs/new-api-config-only.md) and [integration audit](docs/stateful-integration-audit.md).

'''
for path,start,end,body in [
    ('README.md','### 通过 new-api 反代并共享上游 Key','### 已知限制',zh),
    ('README.en.md','### Reverse proxy through new-api with a shared upstream key','### Known Limitations',en),
]:
    s=Path(path).read_text(); a=s.index(start); b=s.index(end,a); put(path,s[:a]+body+s[b:])
put('docs/new-api-config-only.md',zh+'''## 回归与信任边界

本模式是 Kiro 在可信网关后独立派生客户凭据指纹，不读取 New API 数据库、不调用其鉴权接口、不复制钱包/计费逻辑，也不要求客户双 Key。New API 原本的鉴权、额度和路由行为不改变。它不是对任意版本或任意自定义认证插件的通用兼容声明；认证优先级变化应重新核对。

网关必须覆盖内部头且禁止其源值缺失时的透传路径；Kiro 无法从已覆盖为共享 Key 的请求恢复不存在的客户身份，也无法辨识网关错误地声称的身份。不要把该头送给不受你控制的上游服务。

回归测试包含规范化等价形式、不同客户/大小写隔离、RFC 4231 HMAC 向量、错误配置拒绝、凭据剥离、真实 Kiro 鉴权中间件+存储的循环测试、跨租户 continuation 拒绝、store=false 不保存及不能绕过读取校验、重复头和伪造 body/租户字段、无配置收到凭据头的失败闭合。

原有 Responses 终态、EOF、工具 JSON、引用去重、压缩替换、工具名还原和 donor 集成不回退。当前轮测试结果以最终提交的 CI 日志为准；历史 f5958804 的日志实际是 724 passed，先前 PR 说明中的 727 为误报，不能沿用。

本说明不宣称共享/持久化 store、真实上游模型联调、生产部署或交互式 AWS SSO 已完成。旧式 trusted tenant header 模式仅为兼容其他真正产生已认证身份的网关保留，不是此接入方式。
''')
p='docs/stateful-integration-audit.md'; s=Path(p).read_text()
s=s.replace('1. Trusted tenant identity is generated by the companion New API change from authenticated user/token context using `{authenticated_tenant}`. Raw client auth headers are not identities. The Kiro receiver rejects ambiguous tenant headers.', '1. Superseded companion dependency: Kiro now supports opt-in config-only New API integration via responseStoreClientAuthorizationHeader and an environment-only HMAC key. Only the inspected standard Authorization grammar is canonicalized; special authentication paths fail closed. The gateway must overwrite the internal header from its authenticated credential and disable passthrough fallbacks. No New API source patch is required. See docs/new-api-config-only.md for the precise trust boundary.')
s=s.replace('Scope is per authenticated downstream API token, not automatically per human/user across different tokens.', 'Scope is per canonical downstream API credential in the new config-only mode, plus the authenticated Kiro API-key ID; it is not per human/user across different tokens. It relies on correct trusted-gateway forwarding and is not an independent verifier of New API credentials.')
put(p,s)
p='openspec/specs/openai-compat/spec.md'; s=Path(p).read_text()
# Previous instructions are historical, but must not remain deployment requirements.
s=s.replace('{authenticated_tenant}', '{client_header:Authorization}')
s += '''\n\n### Requirement: Kiro-only client credential continuation scope\n\n- New API requires no code patch; only its existing explicit Header Override is used.\n- Kiro enables responseStoreClientAuthorizationHeader and requires an environment-only RESPONSE_STORE_HMAC_KEY (32 bytes / 64 hex). Legacy trusted-tenant mode is mutually exclusive.\n- The configured internal header is consumed at the authentication boundary, canonicalized only for the documented standard New API Authorization grammar, and HMAC-scoped. Raw credentials are not logged, forwarded to model upstreams, or persisted in continuation records.\n- Missing identity allows only explicit store=false with no previous_response_id or item_reference. Every continuation requires identity even if store=false. Invalid values never degrade to missing/shared identity.\n- Duplicate/coalesced values, unsupported authentication sentinels, routing suffixes and copied gateway keys are rejected. No user/body metadata/IP/tenant-header fallback is permitted.\n- The gateway MUST overwrite this internal header from the credential it actually authenticated and MUST NOT allow wildcard/regex/dynamic-header passthrough fallbacks. Kiro must only be accessible through the trusted gateway. This is a deployment precondition, not an authentication property created by hashing.\n'''
put(p,s)
Path('.client-scope-changed-paths').write_text('\n'.join(sorted(changed))+'\n')
print('Kiro-only changed files:\n'+'\n'.join(sorted(changed)))
