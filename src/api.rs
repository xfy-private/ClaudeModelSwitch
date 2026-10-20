//! 路由与辅助端点：`/` 配置页、`/healthz`、`/v1/models`、`/api/config`、`/api/status`。
//! 中转端点 `/v1/messages`、`/v1/messages/count_tokens` 见 proxy.rs。
//! 配置页 HTML 与 `/api/*` 读写见 webui.rs。
//!
//! Phase 18：不再有「paused 停止标志」。托盘「停止」= main 的 run_server 循环直接
//! 释放监听端口、停掉整个服务（浏览器连接被拒），服务恢复只靠托盘「启动」。

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

use crate::config::AppState;
use crate::proxy;
use crate::router;
use crate::webui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route("/api/config", get(config_get).put(webui::config_put))
        .route("/api/status", get(webui::status))
        .route("/api/usage", get(webui::usage))
        .route("/api/balance", get(webui::balance))
        .route("/api/coding-plan", get(webui::coding_plan))
        .route("/api/import-cookie", post(webui::import_cookie))
        .route("/api/usage/reset", post(webui::usage_reset))
        .route("/api/restart", post(webui::restart))
        .route("/api/update-settings", post(webui::update_settings))
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

async fn root() -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        webui::INDEX_HTML,
    )
        .into_response()
}

async fn models(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.cfg.read().await;
    let sep = &cfg.server.separator;
    let aliases: Vec<serde_json::Value> = router::available_aliases(&cfg.providers, sep)
        .into_iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    axum::Json(serde_json::json!({ "data": aliases })).into_response()
}

/// 返回隐藏 api_key 后的配置（供后续 Web UI 使用）。
async fn config_get(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.cfg.read().await;
    let masked = cfg.masked();
    let body = serde_json::to_string(&masked).unwrap_or_default();
    let mut r = (StatusCode::OK, body).into_response();
    if let Ok(v) = HeaderValue::from_str("application/json") {
        r.headers_mut().insert("content-type", v);
    }
    r
}

