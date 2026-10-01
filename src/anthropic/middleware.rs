// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use parking_lot::RwLock;
use sha2::{Digest, Sha256};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;
use crate::model::api_key::{ApiKeyAuthResult, ApiKeyManager};
use crate::model::response_store::ResponseStore;
use crate::model::rpm::RpmTracker;
use crate::model::usage::UsageTracker;

use super::types::{ErrorResponse, Model};

/// `/v1/models` 动态列表缓存条目
///
/// `fetched_at` 用于 TTL 判断（`Instant::elapsed()`）；TTL 秒数取自 `Config::model_cache_ttl_secs`。
#[derive(Clone)]
pub struct CachedModels {
    /// 缓存的模型列表
    pub models: Vec<Model>,
    /// 缓存写入时刻
    pub fetched_at: std::time::Instant,
}

/// 已认证的 API Key 上下文（注入到 request extensions）
#[derive(Clone, Debug)]
pub struct ApiKeyContext {
    /// API Key ID
    pub id: u32,
    /// 绑定的账号 ID 列表，None 表示不限制
    pub bound_credential_ids: Option<Vec<u64>>,
    /// Optional hashed tenant identity supplied by a trusted reverse proxy.
    /// This keeps Responses continuation state isolated when many downstream
    /// users share one authenticated upstream API key.
    pub response_store_scope: Option<String>,
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// Profile ARN（可选，用于请求）
    pub profile_arn: Option<String>,
    /// API Key 管理器（可选，启用多用户 API Key）
    pub api_key_manager: Option<Arc<ApiKeyManager>>,
    /// 用量追踪器（可选，启用用量追踪）
    pub usage_tracker: Option<Arc<UsageTracker>>,
    /// RPM 追踪器（可选，启用 RPM 实时监控）
    pub rpm_tracker: Option<Arc<RpmTracker>>,
    /// Prompt cache 指纹追踪器（替代末层兜底）
    pub fingerprint_tracker: Option<Arc<crate::cache::fingerprint::FingerprintTracker>>,
    /// `/v1/models` 动态列表缓存（TTL 见 `Config::model_cache_ttl_secs`），初始为空
    pub model_cache: Arc<RwLock<Option<CachedModels>>>,
    /// OpenAI Responses continuation store（按 API Key 隔离，可叠加可信租户范围）
    pub(crate) response_store: Arc<ResponseStore>,
    /// Optional trusted proxy header used to scope Responses continuation state.
    pub(crate) response_store_tenant_header: Option<String>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new() -> Self {
        Self {
            kiro_provider: None,
            profile_arn: None,
            api_key_manager: None,
            usage_tracker: None,
            rpm_tracker: None,
            fingerprint_tracker: None,
            model_cache: Arc::new(RwLock::new(None)),
            response_store: Arc::new(ResponseStore::default()),
            response_store_tenant_header: None,
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }

    /// 设置 Profile ARN
    pub fn with_profile_arn(mut self, arn: impl Into<String>) -> Self {
        self.profile_arn = Some(arn.into());
        self
    }

    /// 设置 API Key 管理器
    pub fn with_api_key_manager(mut self, manager: Arc<ApiKeyManager>) -> Self {
        self.api_key_manager = Some(manager);
        self
    }

    /// 设置用量追踪器
    pub fn with_usage_tracker(mut self, tracker: Arc<UsageTracker>) -> Self {
        self.usage_tracker = Some(tracker);
        self
    }

    /// 设置 RPM 追踪器
    pub fn with_rpm_tracker(mut self, tracker: Arc<RpmTracker>) -> Self {
        self.rpm_tracker = Some(tracker);
        self
    }

    /// 设置 Prompt cache 指纹追踪器
    pub fn with_fingerprint_tracker(
        mut self,
        tracker: Arc<crate::cache::fingerprint::FingerprintTracker>,
    ) -> Self {
        self.fingerprint_tracker = Some(tracker);
        self
    }

    /// Configure a trusted reverse-proxy header for Responses tenant isolation.
    pub fn with_response_store_tenant_header(mut self, header_name: impl Into<String>) -> Self {
        let header_name = header_name.into().trim().to_ascii_lowercase();
        if !header_name.is_empty() {
            self.response_store_tenant_header = Some(header_name);
        }
        self
    }
}

/// Hash a trusted proxy tenant value before retaining it in memory or using it
/// as part of a continuation-store key. The raw upstream credential never
/// enters the store key or logs.
fn response_store_scope(request: &Request<Body>, header_name: Option<&str>) -> Option<String> {
    let name = header_name?;
    let mut values = request.headers().get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() || value.is_empty() || value.len() > 1024 || value.contains(',') {
        return None;
    }
    let digest = Sha256::digest(value.as_bytes());
    Some(format!("sha256:{digest:x}"))
}

/// API Key 认证中间件
///
/// 认证优先级：
/// 1. 子 API Key（ApiKeyManager）→ 检查启用/过期/额度
/// 2. 不匹配 → 401
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Some(key) = auth::extract_api_key(&request) else {
        let error = ErrorResponse::authentication_error();
        return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
    };

    // 1. 尝试子 API Key 认证
    if let Some(manager) = &state.api_key_manager {
        match manager.authenticate(&key) {
            ApiKeyAuthResult::Valid {
                id,
                name,
                spending_limit,
                limit_unit,
                bound_credential_ids,
            } => {
                // 懒激活：首次使用时激活 key
                if let Err(e) = manager.activate_key(id) {
                    tracing::warn!(api_key_id = id, error = %e, "激活 API Key 失败");
                }

                // 额度检查：limit_unit = "credits" 按真实 credits 累计计量，否则按美元估算计量
                if let (Some(limit), Some(tracker)) = (spending_limit, &state.usage_tracker) {
                    let is_credits = limit_unit.eq_ignore_ascii_case("credits");
                    let used = if is_credits {
                        tracker.get_total_credits(id)
                    } else {
                        tracker.get_total_cost(id)
                    };
                    if used >= limit {
                        tracing::warn!(
                            api_key_id = id,
                            api_key_name = %name,
                            used = used,
                            spending_limit = limit,
                            limit_unit = %limit_unit,
                            "API Key 额度已用尽"
                        );
                        let message = if is_credits {
                            format!(
                                "API key spending limit exceeded. Used: {:.2} credits, Limit: {:.2} credits",
                                used, limit
                            )
                        } else {
                            format!(
                                "API key spending limit exceeded. Used: ${:.2}, Limit: ${:.2}",
                                used, limit
                            )
                        };
                        let error = ErrorResponse::new("forbidden", message);
                        return (StatusCode::FORBIDDEN, Json(error)).into_response();
                    }
                }

                tracing::debug!(api_key_id = id, api_key_name = %name, "子 API Key 认证通过");
                let response_store_scope =
                    response_store_scope(&request, state.response_store_tenant_header.as_deref());
                request.extensions_mut().insert(ApiKeyContext {
                    id,
                    bound_credential_ids,
                    response_store_scope,
                });
                return next.run(request).await;
            }
            ApiKeyAuthResult::Expired => {
                let error = ErrorResponse::new(
                    "forbidden",
                    "API key has expired. Please contact the administrator to renew it.",
                );
                return (StatusCode::FORBIDDEN, Json(error)).into_response();
            }
            ApiKeyAuthResult::Disabled => {
                let error = ErrorResponse::new(
                    "forbidden",
                    "API key has been disabled. Please contact the administrator.",
                );
                return (StatusCode::FORBIDDEN, Json(error)).into_response();
            }
            ApiKeyAuthResult::NotFound => {
                // 继续到下面的通用 401
            }
        }
    }

    // 2. 不匹配
    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http::Request;

    use super::{AppState, response_store_scope};

    #[test]
    fn response_store_scope_trims_values_and_separates_tenants() {
        let tenant_a = Request::builder()
            .header("X-Kiro2CC-Tenant", " tenant-a ")
            .body(Body::empty())
            .unwrap();
        let tenant_a_without_padding = Request::builder()
            .header("x-kiro2cc-tenant", "tenant-a")
            .body(Body::empty())
            .unwrap();
        let tenant_b = Request::builder()
            .header("X-Kiro2CC-Tenant", "tenant-b")
            .body(Body::empty())
            .unwrap();

        let scope_a = response_store_scope(&tenant_a, Some("x-kiro2cc-tenant"));
        assert_eq!(
            scope_a,
            response_store_scope(&tenant_a_without_padding, Some("x-kiro2cc-tenant"))
        );
        assert_ne!(
            scope_a,
            response_store_scope(&tenant_b, Some("x-kiro2cc-tenant"))
        );
        assert!(response_store_scope(&tenant_a, None).is_none());
    }

    #[test]
    fn response_store_tenant_header_name_is_normalized() {
        let state = AppState::new().with_response_store_tenant_header(" X-Kiro2CC-Tenant ");
        assert_eq!(
            state.response_store_tenant_header.as_deref(),
            Some("x-kiro2cc-tenant")
        );
    }
}
#[cfg(test)]
mod review_20261001 {
    use super::*;
    #[test]
    fn duplicate_and_coalesced_tenant_headers_are_rejected() {
        let mut req = Request::builder().body(Body::empty()).unwrap();
        req.headers_mut().append("x-tenant", "a".parse().unwrap());
        req.headers_mut().append("x-tenant", "b".parse().unwrap());
        assert!(response_store_scope(&req, Some("x-tenant")).is_none());
        req.headers_mut().insert("x-tenant", "a,b".parse().unwrap());
        assert!(response_store_scope(&req, Some("x-tenant")).is_none());
    }
}
