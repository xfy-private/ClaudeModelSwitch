//! 纯透传核心：`/v1/messages` 与 `/v1/messages/count_tokens`。
//!
//! 处理链（决策 D3/D7/D10）：
//!   ① 解析 body.model，按服务商 id 最长前缀匹配（router）
//!   ② 改写 model 为真实模型名
//!   ③ 剥离 CC 认证头，替换为服务商配置的 Key
//!   ④ 保留 anthropic-version / anthropic-beta（可配剥离）
//!   ⑤ 原样转发到 `{base_url}/v1/messages`，SSE 流式字节级透传
//!   ⑥ 可选：流中把 `"model":"<真实>"` 逐行改回别名（D7）

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::header;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::{unfold, Stream};
use futures_util::TryStreamExt;
use std::error::Error as StdError;
use std::sync::Arc;

use crate::config::AppState;

/// 请求体上限（64MB），防呆。
const MAX_BODY: usize = 64 * 1024 * 1024;

pub type BoxError = Box<dyn StdError + Send + Sync>;

/// 原样转发允许保留的请求头（白名单）；认证头/宿主/长度等 hop-by-hop 一律剥离。
const FORWARD_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "x-stainless-arch",
    "x-stainless-async",
    "x-stainless-lang",
    "x-stainless-os",
    "x-stainless-package-version",
    "x-stainless-retry-count",
    "x-stainless-runtime",
    "x-stainless-runtime-version",
    "x-stainless-helper-method",
    "x-stainless-helper-method-name",
    "x-appl-it-artifact-bucket",
    "x-appl-it-request-id",
];

fn hdr(name: &str) -> HeaderName {
    HeaderName::from_bytes(name.as_bytes()).unwrap_or_else(|_| HeaderName::from_static("x-relay"))
}

pub async fn messages(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    forward(state, req, "messages").await
}

pub async fn count_tokens(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    forward(state, req, "count_tokens").await
}

async fn forward(state: Arc<AppState>, req: Request<Body>, sub: &str) -> Response {
    let (parts, body) = req.into_parts();
    let req_bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    if state.debug {
        eprintln!(
            "[debug] {} {} len={}",
            parts.method,
            parts.uri,
            req_bytes.len()
        );
    }

    // ---- ① 解析 body ----
    let mut json: serde_json::Value = match serde_json::from_slice(&req_bytes) {
        Ok(v) => v,
        Err(e) => {
            if state.debug {
                eprintln!("[debug] body parse FAILED len={}: {e}", req_bytes.len());
            }
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response();
        }
    };
    let model: String = match json.get("model").and_then(|m| m.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            if state.debug {
                eprintln!("[debug] body has NO model field len={}", req_bytes.len());
            }
            return (StatusCode::BAD_REQUEST, "missing model field").into_response();
        }
    };

    // ---- ② 前缀匹配 + 改写（Phase 22：`claude-relay-model[ctx]` 哨兵 → 当前使用模型） ----
    let cfg = state.cfg.read().await;
    let (model_base, _ctx) = crate::router::split_ctx_suffix(&model);
    let resolved = match crate::router::resolve_for_request(&cfg, &model) {
        Ok(r) => r,
        Err(msg) => {
            if state.debug {
                eprintln!("[debug] model resolve FAILED '{model}' len={}", req_bytes.len());
            }
            return (StatusCode::BAD_REQUEST, msg).into_response();
        }
    };
    let provider = &cfg.providers[resolved.provider];
    // Phase 36/56/60：上游协议——模型行可覆盖（resolved.protocol），未指定/为空则用
    // 服务商级 protocol；两者皆空视为 anthropic 纯透传。
    let eff_protocol = resolved
        .protocol
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let p = provider.protocol.trim();
            (!p.is_empty()).then(|| p.to_string())
        });
    let is_openai = eff_protocol.as_deref() == Some("openai");
    // Phase 56：Gemini 原生协议（generateContent）-> 请求/响应翻译
    let is_gemini = eff_protocol.as_deref() == Some("gemini");

    // Phase 26：统计本次调用——请求数按服务商+模型；model 键 = 完整别名剥 `[ctx]` 后缀。
    let provider_id = provider.id.clone();
    let model_key = crate::router::split_ctx_suffix(&resolved.alias).0.to_string();
    {
        // Phase 26/54：请求计数 + 小时/天时间桶（model 键剥 `[ctx]` 后缀）。
        let mut u = state.usage.lock().unwrap();
        u.record(&provider_id, &model_key, 1, 0, 0, crate::config::unix_now());
        // Phase 27：置脏标记，周期落盘任务随后写盘
        state.usage_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    json["model"] = serde_json::Value::String(resolved.real_model.clone());

    // Phase 36：OpenAI 协议无 count_tokens 端点 → 直接启发式回退（D8）
    if sub == "count_tokens" && (is_openai || is_gemini) {
        return heuristic_tokens(&req_bytes, "OpenAI 协议无 count_tokens 端点，回退启发式");
    }

    let base = provider.base_url.trim_end_matches('/');
    let url = if is_openai {
        format!("{base}/chat/completions")
    } else if is_gemini {
        // Phase 56：模型名进 URL 路径；流式走独立端点（body 无 stream 字段）
        let stream = json.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
        if stream {
            format!("{base}/v1beta/models/{}:streamGenerateContent?alt=sse", resolved.real_model)
        } else {
            format!("{base}/v1beta/models/{}:generateContent", resolved.real_model)
        }
    } else if sub == "count_tokens" {
        format!("{base}/v1/messages/count_tokens")
    } else {
        format!("{base}/v1/messages")
    };
    if state.debug {
        let via = if model_base == crate::router::RELAY_MODEL {
            format!(" [哨兵 → 当前模型 '{}']", cfg.default_model())
        } else {
            String::new()
        };
        eprintln!(
            "[debug] model '{model}'{via} -> provider '{}' real='{}' url={url}",
            provider.id, resolved.real_model
        );
    }

    // ---- ③④ 请求头：白名单透传 + 认证替换 ----
    let mut out_headers = HeaderMap::new();
    for &name in FORWARD_HEADERS {
        let hn = hdr(name);
        if let Some(v) = parts.headers.get(&hn) {
            if name == "anthropic-beta" && (provider.strip_beta || is_openai || is_gemini) {
                continue;
            }
            // Phase 36/56：OpenAI / Gemini 协议不认识 anthropic-version / anthropic-beta
            if name == "anthropic-version" && (is_openai || is_gemini) {
                continue;
            }
            out_headers.insert(hn, v.clone());
        }
    }
    let key = provider.api_key.as_str();
    if provider.auth == "x-api-key" {
        out_headers.insert("x-api-key", HeaderValue::from_str(key).unwrap_or(HeaderValue::from_static("")));
    } else if provider.auth == "x-goog-api-key" {
        // Phase 56：Gemini 原生协议认证头（AI Studio API Key）
        out_headers.insert("x-goog-api-key", HeaderValue::from_str(key).unwrap_or(HeaderValue::from_static("")));
    } else {
        let bearer = format!("Bearer {key}");
        out_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&bearer).unwrap_or(HeaderValue::from_static("Bearer ")),
        );
    }

    // Phase 58：OpenCode Go 网关（如 opencode.ai/zen/go/v1）要求每个请求携带 `x-opencode-session`
    // （后端路由 + prompt-cache 亲和，缺失上游 400 `MissingSessionID`），并要求专属 User-Agent
    // （非通用 HTTP/SDK 库名，opencode 靠它识别是典型 agent 流量）。
    //   ① x-opencode-session：取自 CC 原生会话头 `x-claude-code-session-id`（v2.1.86+ 每请求自动发，
    //      每段对话唯一、对话内跨轮/resume 稳定——正是 opencode 想要的「每段对话一个稳定 ID」）；
    //      CC 未发（旧版/辅助路径漏发）→ 用进程级回退值兜底，避免 400。白名单不含此头故不会重复。
    //   ② User-Agent：透传客户端自身 UA（CC 的 `claude-cli/<v> ...` 等 agent 风格），
    //      **通用 HTTP/SDK 库 UA（`Python-urllib`/`curl`/`reqwest`/`Go-http-client`…）不透传**，
    //      覆盖为专属 `ClaudeRelay/<ver>`——opencode 文档明确要求「自标识 agent，而非通用 HTTP 库名」。
    //      仅对 opencode 服务商生效，不影响其它协议。
    if provider.is_opencode() {
        let (sess, src) = opencode_session_value(&parts.headers);
        if state.debug {
            eprintln!(
                "[debug] opencode '{}' x-opencode-session from {src} ({} bytes)",
                provider.id,
                sess.len()
            );
        }
        out_headers.insert(
            "x-opencode-session",
            HeaderValue::from_str(&sess).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        let ua = parts
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty() && !is_generic_lib_ua(s))
            .map(str::to_string)
            .unwrap_or_else(opencode_default_ua);
        out_headers.insert(
            header::USER_AGENT,
            HeaderValue::from_str(&ua).unwrap_or_else(|_| HeaderValue::from_static("ClaudeRelay")),
        );
    }

    // Phase 36：OpenAI 协议 → 请求体翻译为 Chat Completions
    // Phase 36/56：OpenAI / Gemini 协议 -> 请求体翻译
    let body_str = if is_openai {
        crate::translate::anthropic_to_openai(&json, &resolved.real_model).to_string()
    } else if is_gemini {
        crate::gemini::anthropic_to_gemini(&json, &resolved.real_model).to_string()
    } else {
        json.to_string()
    };
    let upstream = state
        .client
        .post(&url)
        .headers(out_headers)
        .body(body_str);

    let up_resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
    };
    let up_status = up_resp.status();
    if state.debug {
        eprintln!("[debug] upstream status {up_status}");
    }

    // ---- Phase 36 修正：OpenAI 协议上游非 2xx 时，错误体是 `{"error":{...}}` 格式。
    // 若按成功响应走 openai_message_to_anthropic 翻译，choices 缺失 → 变成一段空白
    // Anthropic message（`{"content":[{"text":""}],"id":"msg_openai",...}`）并携带上游 4xx
    // 状态回给 CC——表现为 `API Error: 400 {空白消息}`，真实错误原因被吞掉。这里把上游
    // 错误翻译成 CC 能识别的 Anthropic `type:error`，让错误信息可见（含上游原 message）。
    if (is_openai || is_gemini) && !up_status.is_success() {
        let up_body = match up_resp.bytes().await {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
        };
        if state.debug {
            eprintln!(
                "[debug] upstream error {}: {}",
                up_status,
                String::from_utf8_lossy(&up_body).trim().chars().take(300).collect::<String>()
            );
        }
        return openai_error_response(up_status, &up_body).into_response();
    }

    // ---- ⑤⑥ 响应头透传 + 流式透传 ----
    let mut resp_headers = HeaderMap::new();
    for (name, value) in up_resp.headers() {
        let n = name.as_str();
        let keep = n.starts_with("anthropic-")
            || n.starts_with("x-")
            || n == "content-type"
            || n == "retry-after"
            || n == "content-encoding";
        if keep {
            resp_headers.insert(name.clone(), value.clone());
        }
    }

    if sub == "count_tokens" {
        let up_body = match up_resp.bytes().await {
            Ok(b) => b,
            Err(e) => return heuristic_tokens(&req_bytes, e),
        };
        if up_status.is_success() {
            // Phase 26：count_tokens 响应计入输入 tokens
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&up_body) {
                let it = v.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                accumulate_usage(&state.usage, &state.usage_dirty, &provider_id, &model_key, it, 0);
            }
            let mut r = (up_status, resp_headers, Body::from(up_body)).into_response();
            set_ct(r.headers_mut(), "application/json");
            return r;
        }
        // 上游无 count_tokens 端点或报错 → 启发式回退（D8）
        return heuristic_tokens(
            &req_bytes,
            format!("upstream {up_status}: {}", String::from_utf8_lossy(&up_body)),
        );
    }

    // /v1/messages：SSE 流式透传（Phase 26：统一走带用量提取的缓冲流，d7 回写并入其中）；
    // 非 SSE 响应读体解析 usage 后返回。
    let is_sse = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |ct| ct.contains("text/event-stream"));
    let real_model = resolved.real_model.clone();
    let alias = resolved.alias.clone();
    let rewrite = provider.rewrite_model && real_model != alias && is_sse;
    let body = if is_sse {
        let sink = UsageSink {
            usage: state.usage.clone(),
            provider: provider_id.clone(),
            model: model_key.clone(),
            dirty: state.usage_dirty.clone(),
            input_fallback: heuristic_input(&req_bytes), // Phase 41：上游缺输入用量时兜底
        };
        if is_openai {
            // Phase 36：OpenAI SSE chunk → Anthropic 事件序列（含用量提取）
            Body::from_stream(openai_sse_stream(up_resp, alias, sink))
        } else if is_gemini {
            // Phase 56：Gemini SSE（alt=sse，无 [DONE]）-> Anthropic 事件序列（含用量提取）
            Body::from_stream(gemini_sse_stream(up_resp, alias, sink))
        } else {
            Body::from_stream(sse_watch_stream(up_resp, real_model, alias, rewrite, sink))
        }
    } else {
        let up_body = match up_resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response()
            }
        };
        if is_openai {
            // Phase 36：非流式 OpenAI ChatCompletion → Anthropic message（model 回写别名）
            let translated = match serde_json::from_slice::<serde_json::Value>(&up_body) {
                Ok(v) => crate::translate::openai_message_to_anthropic(&v, &alias),
                Err(_) => {
                    return (StatusCode::BAD_GATEWAY, "upstream returned non-JSON").into_response()
                }
            };
            let input = translated["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let output = translated["usage"]["output_tokens"].as_u64().unwrap_or(0);
            accumulate_usage(&state.usage, &state.usage_dirty, &provider_id, &model_key, input, output);
            Body::from(translated.to_string())
        } else if is_gemini {
            // Phase 56：非流式 GenerateContentResponse -> Anthropic message（model 回写别名）
            let translated = match serde_json::from_slice::<serde_json::Value>(&up_body) {
                Ok(v) => crate::gemini::gemini_message_to_anthropic(&v, &alias),
                Err(_) => {
                    return (StatusCode::BAD_GATEWAY, "upstream returned non-JSON").into_response()
                }
            };
            let input = translated["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let output = translated["usage"]["output_tokens"].as_u64().unwrap_or(0);
            accumulate_usage(&state.usage, &state.usage_dirty, &provider_id, &model_key, input, output);
            Body::from(translated.to_string())
        } else {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&up_body) {
                let mut input = v
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                if input == 0 {
                    input = heuristic_input(&req_bytes); // Phase 41：非流式同样兜底
                }
                let output = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                accumulate_usage(&state.usage, &state.usage_dirty, &provider_id, &model_key, input, output);
            }
            Body::from(up_body)
        }
    };

    (up_status, resp_headers, body).into_response()
}

fn set_ct(h: &mut HeaderMap, ct: &str) {
    if let Ok(v) = HeaderValue::from_str(ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
}

/// Phase 58：OpenCode 网关会话头取值——入站 CC 原生会话头 `x-claude-code-session-id` 优先
/// （每段对话唯一、对话内稳定），缺失时回退进程级稳定值（OnceLock 生成一次）。返回
/// (取值, 来源说明)，供 debug 日志与单测。
fn opencode_session_value(headers: &HeaderMap) -> (String, &'static str) {
    if let Some(v) = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return (v.to_string(), "x-claude-code-session-id");
    }
    (opencode_fallback_session(), "process-fallback")
}

/// Phase 58：OpenCode 会话头进程级回退值——CC 未带 `x-claude-code-session-id`（旧版 CC /
/// 辅助请求漏发）时用同一值，保证请求不 400。进程内所有兜底请求共享（OnceLock）。
/// 真实 CC 会话请求总会走 `x-claude-code-session-id`，此值仅兜底。
fn opencode_fallback_session() -> String {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("claude-relay-{:x}-{:x}", std::process::id(), nanos)
    })
    .clone()
}

/// Phase 58：OpenCode 网关专属 UA 兜底（无入站 UA 时）——项目自标识，非通用 HTTP 库名。
fn opencode_default_ua() -> String {
    format!("ClaudeRelay/{}", crate::api::VERSION)
}

/// Phase 58：判定入站 User-Agent 是否是「通用 HTTP/SDK 库名」（不含版本，前缀/词首匹配，
/// 大小写不敏感）。OpenCode 文档要求自标识 agent（如 `my-coding-agent/1.0`），而非这些库名。
/// agent 风格（`claude-cli/<v>`、`my-coding-agent/1.0`…）返回 false（应透传）。
fn is_generic_lib_ua(ua: &str) -> bool {
    let lower = ua.to_ascii_lowercase();
    const GENERIC: &[&str] = &[
        "python",      // Python-urllib / python-requests / Python/3.x
        "go-http-client",
        "curl",
        "wget",
        "reqwest",
        "hyper",
        "node",        // node/<v>、node-fetch、undici
        "axios",
        "aiohttp",
        "httpx",
        "requests",
        "java",
        "okhttp",
        "httpclient",
        "apache",
        "dotnet",
        "powershell",
        "insomnia",
        "postmanruntime",
        "libwww",
    ];
    // 词首匹配：库名往往出现在 UA 开头，或紧随空格/`(` 之后（防误伤 agent 描述里的词）
    for g in GENERIC {
        let gb = g.as_bytes();
        let lb = lower.as_bytes();
        let mut i = 0;
        while i + gb.len() <= lb.len() {
            if &lb[i..i + gb.len()] == gb {
                let at_start = i == 0;
                let prev = if i == 0 { b' ' } else { lb[i - 1] };
                if at_start || prev == b' ' || prev == b'/' || prev == b'(' {
                    return true;
                }
            }
            i += 1;
        }
    }
    false
}

/// count_tokens 启发式回退：`max(1, 字符数/4)`（D8）。
fn heuristic_tokens(req_body: &[u8], why: impl std::fmt::Display) -> Response {
    let tokens = heuristic_input(req_body);
    let json = serde_json::json!({ "input_tokens": tokens }).to_string();
    let mut r = (StatusCode::OK, Body::from(json)).into_response();
    set_ct(r.headers_mut(), "application/json");
    if std::env::var_os("CLAUDE_RELAY_DEBUG_HEURISTIC").is_some() {
        eprintln!("[debug] count_tokens heuristic fallback: {why}");
    }
    r
}

/// OpenAI 协议上游非 2xx 错误 → 构造 CC 可识别的 Anthropic `type:error` 响应。
/// 解析 OpenAI `{"error":{"message","type"}}`，解析不出则用原始 body 文本截断。
/// 返回上游原始状态码（400/429/5xx 语义保留，CC 会据此重试 429/5xx）。
fn openai_error_response(status: StatusCode, body: &[u8]) -> Response {
    let (status, value) = openai_error(status, body);
    let mut r = (status, Body::from(value.to_string())).into_response();
    set_ct(r.headers_mut(), "application/json");
    r
}

/// `openai_error_response` 的纯逻辑部分（供测试同步调用）：返回 (状态码, 错误 JSON)。
fn openai_error(status: StatusCode, body: &[u8]) -> (StatusCode, serde_json::Value) {
    // 兼容三种错误体：纯 JSON、`data: {json}` SSE 单行、非 JSON 文本。都取 error.message
    let mut raw = String::from_utf8_lossy(body).trim().to_string();
    if let Some(stripped) = raw.strip_prefix("data:") {
        raw = stripped.trim().to_string();
    }
    // 提取上游错误信息：`{"error":{"message","type"}}`；非 JSON / 缺 message 用原始文本截断
    let (msg, oa_type) = serde_json::from_str::<serde_json::Value>(&raw)
        .map(|v| {
            let err = v.get("error").unwrap_or(&v);
            let m = err
                .get("message")
                .and_then(|x| x.as_str())
                .map(String::from)
                .unwrap_or_else(|| raw.chars().take(500).collect());
            let t = err
                .get("type")
                .and_then(|x| x.as_str())
                // Phase 56：Gemini 错误体用 error.status（如 INVALID_ARGUMENT），无 type
                .or_else(|| err.get("status").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            (m, t)
        })
        .unwrap_or_else(|_| (raw.chars().take(500).collect(), String::new()));
    // 按状态码映射为 CC 认识的 Anthropic error type；上游自己的 type 附进 message 便于排查
    let err_type = match status.as_u16() {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    };
    let suffix = if oa_type.is_empty() {
        String::new()
    } else {
        format!(" (upstream type: {oa_type})")
    };
    (
        status,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": err_type,
                "message": format!("upstream {status}: {msg}{suffix}"),
            },
        }),
    )
}

/// Phase 26：用量收集目标——共享统计 + 本流所属服务商/模型键。
/// Phase 36：`pub(crate)`——translate.rs 的 OpenAI 流翻译器复用同一收集器。
/// Phase 41：`input_fallback`——上游不在响应里返回输入用量（讯飞星辰 Anthropic 端点）时的启发式估算。
pub(crate) struct UsageSink {
    pub(crate) usage: Arc<std::sync::Mutex<crate::config::UsageStats>>,
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Phase 27：置脏标记，周期落盘任务随后写盘。
    pub(crate) dirty: Arc<std::sync::atomic::AtomicBool>,
    /// Phase 41：本请求输入 tokens 启发式估算（字符/4，同 count_tokens 兜底），上游缺用量时兜底。
    pub(crate) input_fallback: u64,
}

/// 输入 tokens 启发式估算（`字符数/4`，与 count_tokens 兜底规则一致）——讯飞星辰等上游
/// 不在 `message_start` 返回 `usage.input_tokens` 时，用请求体估算补上输入统计。
fn heuristic_input(req_body: &[u8]) -> u64 {
    let chars = String::from_utf8_lossy(req_body).chars().count();
    ((chars / 4).max(1)) as u64
}

/// 累加一次用量（输入/输出 tokens）到共享统计（0+0 直接跳过）。
/// Phase 54：收口到 `UsageStats::record`——累计值 + 小时/天时间桶（translate.rs finish 走同一函数）。
pub(crate) fn accumulate_usage(
    usage: &Arc<std::sync::Mutex<crate::config::UsageStats>>,
    dirty: &Arc<std::sync::atomic::AtomicBool>,
    provider: &str,
    model: &str,
    input: u64,
    output: u64,
) {
    if input == 0 && output == 0 {
        return;
    }
    usage
        .lock()
        .unwrap()
        .record(provider, model, 0, input, output, crate::config::unix_now());
    dirty.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Phase 59：上游中途断流时发给 CC 的协议规范终止事件（Anthropic SSE `error` 事件）。
///
/// 旧行为把上游 Err 原样抛进响应体 → hyper 异常截断 chunked 响应（不发终止块），
/// CC 只能看到 "The response stream was malformed"（无法诊断）。现在转为
/// error 事件 + 正常结束流，CC 显示真实错误信息。
fn sse_error_chunk(msg: &str) -> Bytes {
    let data = serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": msg }
    });
    Bytes::from(format!("event: error\ndata: {data}\n\n"))
}

/// 把 SSE 数据流逐行处理：`"model":"<真实名>"` 回写为别名（D7，`do_rewrite` 时），
/// 并提取用量（Phase 26）——`message_start` 的 input_tokens、`message_delta` 的 output_tokens。
/// 所有 SSE 流统一走这条缓冲路径（`do_rewrite=false` 时纯透传 + 仅用量提取）。
fn sse_watch_stream(
    up_resp: reqwest::Response,
    real: String,
    alias: String,
    do_rewrite: bool,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    sse_watch_stream_inner(
        up_resp.bytes_stream().map_err(|e| Box::new(e) as BoxError),
        real,
        alias,
        do_rewrite,
        sink,
    )
}

/// 内核与 `reqwest::Response` 解耦：可注入任意字节流，供「上游断流」单测使用。
fn sse_watch_stream_inner(
    src: impl Stream<Item = Result<Bytes, BoxError>> + std::marker::Unpin,
    real: String,
    alias: String,
    do_rewrite: bool,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    let rw = ModelRewriter { buf: Vec::new(), real, alias, do_rewrite, sink, saw_stop: false };

    unfold((src, rw, false, false), |(mut s, mut r, mut flushed, mut errored)| async move {
        if errored {
            return None; // 断流 error 事件已发出（上一轮 Ok），本轮干净收尾
        }
        loop {
            match s.try_next().await {
                Ok(Some(chunk)) => {
                    let mut out = Vec::new();
                    r.push(&chunk, &mut out);
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, r, flushed, errored)));
                }
                Ok(None) => {
                    if !flushed {
                        flushed = true;
                        if !r.saw_stop {
                            // Phase 61：上游干净断流（EOF 但未发 message_stop）——聚合网关
                            // 对长空闲常 FIN 掐流，reqwest 只返回 Ok(None)（非 Err）。
                            // 不能伪装正常结束（CC 收到无终止事件流 → malformed）：
                            // 发规范 error 事件 + 干净收尾 + 日志。
                            eprintln!(
                                "[warn] 上游流提前结束(未见 message_stop, 疑似网关空闲超时): {}",
                                r.alias
                            );
                            errored = true;
                            let out = sse_error_chunk(&format!(
                                "上游流提前结束(未收到完整回复): {}",
                                r.alias
                            ));
                            return Some((Ok::<Bytes, BoxError>(out), (s, r, flushed, errored)));
                        }
                        let mut out = Vec::new();
                        r.finish(&mut out);
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, r, flushed, errored)));
                    }
                    return None;
                }
                Err(e) => {
                    // Phase 59：上游中途断流——不向响应体抛 Err（hyper 会异常截断
                    // chunked 响应，CC 报 "response stream was malformed"）。
                    // 发协议规范 error 事件 + 正常结束流，CC 显示真实错误。
                    eprintln!("[warn] 上游流中断: {e}");
                    errored = true;
                    let out = sse_error_chunk(&format!("上游流中断: {e}"));
                    return Some((Ok::<Bytes, BoxError>(out), (s, r, flushed, errored)));
                }
            }
        }
    })
}

/// Phase 36：OpenAI 流式 SSE → Anthropic 事件序列（translate::OpenAiStreamTranslator）。
fn openai_sse_stream(
    up_resp: reqwest::Response,
    alias: String,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    openai_sse_stream_inner(
        up_resp.bytes_stream().map_err(|e| Box::new(e) as BoxError),
        alias,
        sink,
    )
}

/// 内核与 `reqwest::Response` 解耦：可注入任意字节流，供「上游断流」单测使用。
fn openai_sse_stream_inner(
    src: impl Stream<Item = Result<Bytes, BoxError>> + std::marker::Unpin,
    alias: String,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    let tr = crate::translate::OpenAiStreamTranslator::new(alias, sink);

    unfold((src, tr, false, false), |(mut s, mut tr, mut flushed, mut errored)| async move {
        if errored {
            return None; // 断流 error 事件已发出（上一轮 Ok），本轮干净收尾
        }
        loop {
            match s.try_next().await {
                Ok(Some(chunk)) => {
                    let mut out = Vec::new();
                    tr.push(&chunk, &mut out);
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, tr, flushed, errored)));
                }
                Ok(None) => {
                    if !flushed {
                        flushed = true;
                        // Phase 61 终止判据：收到过 finish_reason(_STOP)，或已按 [DONE]/
                        // finishReason finalize。两者皆无 + EOF = 上游被掐断（聚合网关对
                        // 长空闲常 FIN 掐流，reqwest 只返回 Ok(None) 非 Err）。不能伪装
                        // 正常结束（CC 收到无终止事件流 → malformed）：发规范 error 事件。
                        if !tr.stream_terminated() {
                            eprintln!(
                                "[warn] 上游流提前结束(未收到终止事件, 疑似网关空闲超时): {}",
                                tr.alias()
                            );
                            errored = true;
                            let out = sse_error_chunk(&format!(
                                "上游流提前结束(未收到完整回复): {}",
                                tr.alias()
                            ));
                            return Some((Ok::<Bytes, BoxError>(out), (s, tr, flushed, errored)));
                        }
                        let mut out = Vec::new();
                        tr.finish(&mut out);
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, tr, flushed, errored)));
                    }
                    return None;
                }
                Err(e) => {
                    // Phase 59：上游中途断流——不向响应体抛 Err（hyper 会异常截断
                    // chunked 响应，CC 报 "response stream was malformed"）。
                    // 发协议规范 error 事件 + 正常结束流，CC 显示真实错误。
                    eprintln!("[warn] 上游流中断: {e}");
                    errored = true;
                    let out = sse_error_chunk(&format!("上游流中断: {e}"));
                    return Some((Ok::<Bytes, BoxError>(out), (s, tr, flushed, errored)));
                }
            }
        }
    })
}

/// Phase 56：Gemini 流式 SSE -> Anthropic 事件序列（gemini::GeminiStreamTranslator）。
fn gemini_sse_stream(
    up_resp: reqwest::Response,
    alias: String,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    gemini_sse_stream_inner(
        up_resp.bytes_stream().map_err(|e| Box::new(e) as BoxError),
        alias,
        sink,
    )
}

/// 内核与 `reqwest::Response` 解耦：可注入任意字节流，供「上游断流」单测使用。
fn gemini_sse_stream_inner(
    src: impl Stream<Item = Result<Bytes, BoxError>> + std::marker::Unpin,
    alias: String,
    sink: UsageSink,
) -> impl Stream<Item = Result<Bytes, BoxError>> {
    let tr = crate::gemini::GeminiStreamTranslator::new(alias, sink);

    unfold((src, tr, false, false), |(mut s, mut tr, mut flushed, mut errored)| async move {
        if errored {
            return None; // 断流 error 事件已发出（上一轮 Ok），本轮干净收尾
        }
        loop {
            match s.try_next().await {
                Ok(Some(chunk)) => {
                    let mut out = Vec::new();
                    tr.push(&chunk, &mut out);
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, tr, flushed, errored)));
                }
                Ok(None) => {
                    if !flushed {
                        flushed = true;
                        // Phase 61 终止判据：收到过 finish_reason(_STOP)，或已按 [DONE]/
                        // finishReason finalize。两者皆无 + EOF = 上游被掐断（聚合网关对
                        // 长空闲常 FIN 掐流，reqwest 只返回 Ok(None) 非 Err）。不能伪装
                        // 正常结束（CC 收到无终止事件流 → malformed）：发规范 error 事件。
                        if !tr.stream_terminated() {
                            eprintln!(
                                "[warn] 上游流提前结束(未收到终止事件, 疑似网关空闲超时): {}",
                                tr.alias()
                            );
                            errored = true;
                            let out = sse_error_chunk(&format!(
                                "上游流提前结束(未收到完整回复): {}",
                                tr.alias()
                            ));
                            return Some((Ok::<Bytes, BoxError>(out), (s, tr, flushed, errored)));
                        }
                        let mut out = Vec::new();
                        tr.finish(&mut out);
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok::<Bytes, BoxError>(Bytes::from(out)), (s, tr, flushed, errored)));
                    }
                    return None;
                }
                Err(e) => {
                    // Phase 59：上游中途断流——不向响应体抛 Err（hyper 会异常截断
                    // chunked 响应，CC 报 "response stream was malformed"）。
                    // 发协议规范 error 事件 + 正常结束流，CC 显示真实错误。
                    eprintln!("[warn] 上游流中断: {e}");
                    errored = true;
                    let out = sse_error_chunk(&format!("上游流中断: {e}"));
                    return Some((Ok::<Bytes, BoxError>(out), (s, tr, flushed, errored)));
                }
            }
        }
    })
}

/// 按 `\n` 分行的缓冲改写/用量提取器：只在 `data:` 行内处理。
///
/// - 改写（D7）：兼容 Anthropic 官方紧凑 `"model":"x"`（字节替换快路径）与带空格的
///   `"model": "x"`（如 Python json.dumps，解析 JSON 后递归改写任意嵌套层）。
/// - 用量（Phase 26）：含 `usage` 子串的行必须解析 JSON（跳过字节快路径防漏计），
///   `message_start` 累加输入、`message_delta` 累加输出；不含 usage 且不改写时零解析（纯透传）。
struct ModelRewriter {
    buf: Vec<u8>,
    real: String,
    alias: String,
    do_rewrite: bool,
    sink: UsageSink,
    /// Phase 61：是否已见到规范终止事件 `message_stop`（EOF 是否截断的判据）。
    saw_stop: bool,
}

impl ModelRewriter {
    /// 追加一块数据，产出完整行（含换行）的改写结果。
    fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.rewrite_line(&mut line);
            out.extend_from_slice(&line);
        }
    }

    /// 流结束：冲刷剩余不完整行。
    fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.buf.is_empty() {
            let mut line = std::mem::take(&mut self.buf);
            self.rewrite_line(&mut line);
            out.extend_from_slice(&line);
        }
    }

    fn rewrite_line(&mut self, line: &mut Vec<u8>) {
        // Phase 61：字节级检测规范终止事件（零解析，先于快/慢路径），供 EOF 截断判据。
        if !self.saw_stop && line.windows(12).any(|w| w == b"message_stop".as_slice()) {
            self.saw_stop = true;
        }
        if !line.starts_with(b"data:") {
            return;
        }
        let has_usage = line.windows(5).any(|w| w == b"usage".as_slice());
        // Phase 41：message_start 行即使无 usage 也要进慢路径累计——讯飞星辰等上游不在
        // message_start 返回 usage，若只看 has_usage 则输入统计整体漏记（快路径直接 return）。
        let is_message_start = line.windows(13).any(|w| w == b"message_start".as_slice());
        if !has_usage && !is_message_start {
            if self.do_rewrite {
                // 快路径：紧凑格式字节替换（可扩容）
                let from = format!("\"model\":\"{}\"", self.real);
                let to = format!("\"model\":\"{}\"", self.alias);
                if replace_bytes(line, from.as_bytes(), to.as_bytes()) {
                    return;
                }
            } else {
                return; // 纯透传且无用量：零解析
            }
        }
        // 慢路径：解析 JSON，递归改写任意嵌套层的 `model` 字段（容忍任意空白，
        // 如 `"model": "x"`。注意 SSE 里 model 常在 `message` 对象内层）+ 提取用量。
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let s = String::from_utf8_lossy(payload).trim().to_string();
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&s) else {
            return;
        };
        if self.do_rewrite {
            rewrite_json_model(&mut v, &self.real, &self.alias);
        }
        if has_usage || is_message_start {
            self.accumulate(&v);
        }
        if self.do_rewrite {
            if let Ok(out) = serde_json::to_string(&v) {
                *line = format!("data:{out}\n").into_bytes();
            }
        }
    }

    /// 从 `message_start` / `message_delta` 事件提取并累加输入/输出 tokens（Phase 26）。
    fn accumulate(&self, v: &serde_json::Value) {
        let Some(typ) = v.get("type").and_then(|t| t.as_str()) else {
            return;
        };
        let (mut input, mut output) = (0u64, 0u64);
        if typ == "message_start" {
            if let Some(n) = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(|x| x.as_u64())
            {
                input = n;
            } else {
                // Phase 41：讯飞星辰等上游不在 message_start 返回 input_tokens → 启发式兜底
                input = self.sink.input_fallback;
            }
        } else if typ == "message_delta" {
            if let Some(n) = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|x| x.as_u64())
            {
                output = n;
            }
        }
        accumulate_usage(
            &self.sink.usage,
            &self.sink.dirty,
            &self.sink.provider,
            &self.sink.model,
            input,
            output,
        );
    }
}

/// 递归把值等于 `real` 的 `model` 字段改为 `alias`；返回是否改动。
fn rewrite_json_model(v: &mut serde_json::Value, real: &str, alias: &str) -> bool {
    let mut changed = false;
    match v {
        serde_json::Value::Object(map) => {
            if let Some(s) = map.get("model").and_then(|m| m.as_str()) {
                if s == real {
                    map.insert("model".into(), serde_json::Value::String(alias.into()));
                    changed = true;
                }
            }
            for val in map.values_mut() {
                changed |= rewrite_json_model(val, real, alias);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr.iter_mut() {
                changed |= rewrite_json_model(val, real, alias);
            }
        }
        _ => {}
    }
    changed
}

/// 在 Vec 中替换第一处子串；命中返回 true（原地可扩容）。
fn replace_bytes(hay: &mut Vec<u8>, from: &[u8], to: &[u8]) -> bool {
    if from.is_empty() || from.len() > hay.len() {
        return false;
    }
    let mut pos = None;
    for i in 0..=hay.len() - from.len() {
        if &hay[i..i + from.len()] == from {
            pos = Some(i);
            break;
        }
    }
    let Some(pos) = pos else { return false };
    let mut out = Vec::with_capacity(hay.len() + to.len().saturating_sub(from.len()));
    out.extend_from_slice(&hay[..pos]);
    out.extend_from_slice(to);
    out.extend_from_slice(&hay[pos + from.len()..]);
    *hay = out;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UsageStats;

    /// 测试用 UsageSink + 共享统计（provider=mock, model=mock-chat）。
    fn sink() -> (UsageSink, Arc<std::sync::Mutex<UsageStats>>) {
        let usage = Arc::new(std::sync::Mutex::new(UsageStats::default()));
        let s = UsageSink {
            usage: usage.clone(),
            provider: "mock".into(),
            model: "mock-chat".into(),
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            input_fallback: 0,
        };
        (s, usage)
    }

    #[test]
    fn usage_fallback_when_upstream_missing_input() {
        // Phase 41：讯飞星辰等上游不在 message_start 返回 input_tokens → 用请求体启发式兜底
        let (mut s, usage) = sink();
        s.input_fallback = 50;
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "mock-chat".into(),
            alias: "ds-mock-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        // message_start 无 usage 字段；message_delta 有 output_tokens
        let input = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":22}}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        let u = usage.lock().unwrap();
        let pu = &u.providers["mock"];
        assert_eq!(pu.input_tokens, 50, "缺失输入用量时用启发式兜底");
        assert_eq!(pu.output_tokens, 22);
    }

    #[test]
    fn usage_uses_real_input_when_present() {
        // Phase 41 兜底不覆盖上游真实返回值：message_start 有 usage 时用真实值
        let (s, usage) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "mock-chat".into(),
            alias: "ds-mock-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let input = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\",\"usage\":{\"input_tokens\":8,\"output_tokens\":1}}}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        let u = usage.lock().unwrap();
        assert_eq!(u.providers["mock"].input_tokens, 8);
    }

    #[test]
    fn rewrite_in_data_line() {
        let (s, _) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "deepseek-chat".into(),
            alias: "ds-deepseek-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let input = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"deepseek-chat\"}}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        assert!(String::from_utf8_lossy(&out).contains("\"model\":\"ds-deepseek-chat\""));
        assert!(!String::from_utf8_lossy(&out).contains("\"model\":\"deepseek-chat\""));
    }

    #[test]
    fn chunk_boundary_split() {
        let (s, _) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "deepseek-chat".into(),
            alias: "ds-deepseek-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        // 把 `"model"` 拆在两个 chunk 之间
        let a = b"data: {\"type\":\"message_start\",\"mes";
        let b = b"sage\":{\"model\":\"deepseek-chat\"}}\n\n";
        let mut out = Vec::new();
        r.push(a, &mut out);
        assert!(out.is_empty(), "未到换行不应产出");
        r.push(b, &mut out);
        assert!(String::from_utf8_lossy(&out).contains("\"model\":\"ds-deepseek-chat\""));
    }

    #[test]
    fn non_data_lines_untouched() {
        let (s, _) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "deepseek-chat".into(),
            alias: "ds-deepseek-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let mut out = Vec::new();
        r.push(b"event: message_start\n", &mut out);
        assert_eq!(String::from_utf8_lossy(&out), "event: message_start\n");
    }

    #[test]
    fn spaced_json_slow_path_nested() {
        // Python json.dumps 风格：带空格 + model 嵌套在 message 内（真实 SSE 形状）
        let (s, _) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "mock-chat".into(),
            alias: "mock-mock-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let input = b"data: {\"type\": \"message_start\", \"message\": {\"id\": \"m1\", \"model\": \"mock-chat\"}}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\"model\":\"mock-mock-chat\""), "got: {s}");
        assert!(!s.contains("\"model\": \"mock-chat\""), "got: {s}");
    }

    #[test]
    fn alias_grows_line() {
        // 别名比真实名更长 → 行变长，验证扩容路径
        let (s, _) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "chat".into(),
            alias: "mock-mock-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let input = b"data: {\"model\":\"chat\"}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        assert!(String::from_utf8_lossy(&out).contains("\"model\":\"mock-mock-chat\""));
    }

    #[test]
    fn usage_accumulates_from_sse() {
        // Phase 26：SSE message_start(input) + message_delta(output) → 按服务商/模型累加
        let (s, usage) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "mock-chat".into(),
            alias: "mock-mock-chat".into(),
            do_rewrite: true,
            sink: s,
            saw_stop: false,
        };
        let input = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":22}}\n\n";
        let mut out = Vec::new();
        r.push(input, &mut out);
        let u = usage.lock().unwrap();
        let pu = u.providers.get("mock").expect("provider usage");
        assert_eq!(pu.input_tokens, 5);
        assert_eq!(pu.output_tokens, 22);
        let mu = pu.by_model.get("mock-chat").expect("model usage");
        assert_eq!(mu.input_tokens, 5);
        assert_eq!(mu.output_tokens, 22);
    }

    #[test]
    fn usage_ignores_non_usage_lines_when_passthrough() {
        // 纯透传（do_rewrite=false）且行内无 usage → 不产生统计条目（0+0 跳过）
        let (s, usage) = sink();
        let mut r = ModelRewriter {
            buf: Vec::new(),
            real: "mock-chat".into(),
            alias: "mock-mock-chat".into(),
            do_rewrite: false,
            sink: s,
            saw_stop: false,
        };
        let mut out = Vec::new();
        r.push(
            b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            &mut out,
        );
        let u = usage.lock().unwrap();
        assert!(u.providers.is_empty());
    }

    #[test]
    fn heuristic_counts() {
        let body = b"{\"model\":\"ds-x\",\"messages\":[{\"role\":\"user\",\"content\":\"hello world\"}]}";
        let r = heuristic_tokens(body, "test");
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn opencode_session_uses_client_header_when_present() {
        // Phase 58: inbound CC native session header wins; empty -> process fallback
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", HeaderValue::from_static("session-abc-123"));
        let (v, src) = opencode_session_value(&h);
        assert_eq!(v, "session-abc-123");
        assert_eq!(src, "x-claude-code-session-id");
        let mut h2 = HeaderMap::new();
        h2.insert("x-claude-code-session-id", HeaderValue::from_static("  "));
        let (v2, src2) = opencode_session_value(&h2);
        assert_eq!(src2, "process-fallback");
        assert!(!v2.is_empty());
    }

    #[test]
    fn opencode_session_fallback_is_stable_nonempty() {
        // Phase 58: no inbound session header -> stable non-empty process fallback
        let h = HeaderMap::new();
        let (a, src_a) = opencode_session_value(&h);
        let (b, src_b) = opencode_session_value(&h);
        assert_eq!(src_a, "process-fallback");
        assert_eq!(src_b, "process-fallback");
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert!(opencode_default_ua().starts_with("ClaudeRelay/"), "{}", opencode_default_ua());
    }

    #[test]
    fn generic_lib_ua_detection() {
        // Phase 58: generic HTTP/SDK library UAs are covered, agent-style UAs pass through
        for ua in ["Python-urllib/3.12", "python-requests/2.31", "curl/8.5.0",
                   "reqwest/0.12", "Go-http-client/1.1", "axios/1.6", "node-fetch/2.7",
                   "Java/17.0.1", "Apache-HttpClient/4.5", "PostmanRuntime/7.37.0"] {
            assert!(is_generic_lib_ua(ua), "should be generic: {ua}");
        }
        for ua in ["claude-cli/2.1.96 (external, cli)", "claude-code/2.1.96 (external, cli)",
                   "my-coding-agent/1.0", "ClaudeRelay/0.1.0", "opencode-cli/0.1.0",
                   "HermesAgent/0.21.0"] {
            assert!(!is_generic_lib_ua(ua), "should pass through: {ua}");
        }
        assert!(!is_generic_lib_ua(""), "empty should not be generic");
    }

    #[test]
    fn openai_error_response_translates_upstream_error() {
        // Phase 36 修正：上游 400 错误体不再被翻译成空白 message（choices 缺失），
        // 而是映射为 CC 可识别的 Anthropic type:error，并保留真实错误信息
        let body =
            br#"{"error":{"message":"max_tokens exceeds model limit","type":"invalid_request_error"}}"#;
        let (status, v) = openai_error(StatusCode::BAD_REQUEST, body);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_tokens exceeds model limit"));
        // 429 → rate_limit_error，语义保留
        let (status, v) = openai_error(StatusCode::TOO_MANY_REQUESTS, b"{\"error\":{\"message\":\"slow down\"}}");
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(v["error"]["type"], "rate_limit_error");
        // SSE 格式错误体（`data: {json}`）→ 剥前缀后同样解析出 message
        let (_, v) = openai_error(StatusCode::BAD_REQUEST, b"data: {\"error\":{\"message\":\"bad tool call\"}}\n\n");
        assert!(v["error"]["message"].as_str().unwrap().contains("bad tool call"));
        // 非 JSON 错误体 → 用原始文本截断，不 panic
        let (_, v2) = openai_error(StatusCode::BAD_GATEWAY, b"<html>bad gateway</html>");
        assert!(v2["error"]["message"].as_str().unwrap().contains("bad gateway"));
        // Phase 56：Gemini 错误体（error.status 而非 error.type）-> 同样提取 message + status
        let (_, v3) = openai_error(
            StatusCode::BAD_REQUEST,
            br#"{"error":{"code":400,"message":"API key not valid","status":"INVALID_ARGUMENT"}}"#,
        );
        assert_eq!(v3["error"]["type"], "invalid_request_error");
        let m = v3["error"]["message"].as_str().unwrap();
        assert!(m.contains("API key not valid"), "got: {m}");
        assert!(m.contains("INVALID_ARGUMENT"), "got: {m}");
    }

    #[test]
    fn sse_error_chunk_format() {
        // Phase 59：断流终止事件 = 协议规范 Anthropic error 事件
        let b = sse_error_chunk("上游流中断: boom");
        let s = String::from_utf8(b.to_vec()).unwrap();
        assert!(s.starts_with("event: error\ndata: "), "格式: {s}");
        let payload = s.trim_start_matches("event: error\ndata: ").trim_end();
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "api_error");
        assert_eq!(v["error"]["message"], "上游流中断: boom");
    }

    #[test]
    fn openai_stream_upstream_error_emits_error_event_and_closes() {
        // Phase 59：上游中途断流 → 已产出事件照常下发 + 规范 error 事件 + 干净结束；
        // 错误不得向下游传播（否则 hyper 截断响应体 → CC malformed）
        let (s, _usage) = sink();
        let src = futures_util::stream::iter(vec![
            Ok::<Bytes, BoxError>(Bytes::from_static(
                b"data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            )),
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "boom",
            )) as BoxError),
        ]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            let mut st = std::pin::pin!(openai_sse_stream_inner(src, "mock-ot-chat".into(), s));
            let mut acc: Vec<u8> = Vec::new();
            while let Some(item) = st.next().await {
                acc.extend_from_slice(&item.expect("断流错误不应向下游传播"));
            }
            let text = String::from_utf8(acc).unwrap();
            assert!(text.contains("event: message_start"), "缺 message_start: {text}");
            assert!(text.contains("text_delta"), "缺文本增量: {text}");
            assert!(text.contains("event: error"), "缺 error 事件: {text}");
            assert!(text.contains("api_error"), "缺 api_error: {text}");
            assert!(text.contains("上游流中断: boom"), "缺原始错误: {text}");
            assert!(!text.contains("message_stop"), "error 终止后不应有 message_stop: {text}");
        });
    }

    #[test]
    fn passthrough_stream_upstream_error_emits_error_event() {
        // Phase 59：透传路径同样不抛 Err，error 事件收尾
        let (s, _usage) = sink();
        let src = futures_util::stream::iter(vec![
            Ok::<Bytes, BoxError>(Bytes::from_static(
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\"}}\n\n",
            )),
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            )) as BoxError),
        ]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            let mut st = std::pin::pin!(sse_watch_stream_inner(
                src,
                "mock-chat".into(),
                "mock-mock-chat".into(),
                false,
                s,
            ));
            let mut acc: Vec<u8> = Vec::new();
            while let Some(item) = st.next().await {
                acc.extend_from_slice(&item.expect("断流错误不应向下游传播"));
            }
            let text = String::from_utf8(acc).unwrap();
            assert!(text.contains("event: message_start"), "缺 message_start: {text}");
            assert!(text.contains("event: error"), "缺 error 事件: {text}");
            assert!(text.contains("上游流中断: reset"), "缺原始错误: {text}");
        });
    }

    #[test]
    fn passthrough_stream_premature_eof_emits_error_event() {
        // Phase 61：上游干净断流（EOF 而非 Err——网关 FIN 掐流，reqwest 返回 Ok(None)）：
        // 未见 message_stop 时不得伪装正常结束（CC 会报 malformed），发规范 error 事件。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            // 截断流：message_start 后直接 EOF
            let (s, _usage) = sink();
            let src = futures_util::stream::iter(vec![
                Ok::<Bytes, BoxError>(Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\"}}\n\n",
                )),
            ]);
            let mut st = std::pin::pin!(sse_watch_stream_inner(
                src,
                "mock-chat".into(),
                "mock-mock-chat".into(),
                false,
                s,
            ));
            let mut acc: Vec<u8> = Vec::new();
            while let Some(item) = st.next().await {
                acc.extend_from_slice(&item.expect("EOF 不应向下游传播 Err"));
            }
            let text = String::from_utf8(acc).unwrap();
            assert!(text.contains("event: message_start"), "缺 message_start: {text}");
            assert!(text.contains("event: error"), "缺 error 事件: {text}");
            assert!(text.contains("上游流提前结束"), "缺提前结束提示: {text}");
            assert!(!text.contains("message_stop"), "截断流不应有 message_stop: {text}");

            // 正常流（含 message_stop）EOF → 无 error（回归）
            let (s2, _usage) = sink();
            let src2 = futures_util::stream::iter(vec![
                Ok::<Bytes, BoxError>(Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"mock-chat\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                )),
            ]);
            let mut st2 = std::pin::pin!(sse_watch_stream_inner(
                src2,
                "mock-chat".into(),
                "mock-mock-chat".into(),
                false,
                s2,
            ));
            let mut acc2: Vec<u8> = Vec::new();
            while let Some(item) = st2.next().await {
                acc2.extend_from_slice(&item.expect("正常流不应出错"));
            }
            let text2 = String::from_utf8(acc2).unwrap();
            assert!(text2.contains("event: message_stop"), "正常流应透传 message_stop: {text2}");
            assert!(!text2.contains("event: error"), "正常流不应产生 error: {text2}");
        });
    }

    #[test]
    fn openai_stream_premature_eof_emits_error_event() {
        // Phase 61：OpenAI 流 EOF（无 finish_reason 也无 [DONE]）→ error 事件而非伪造收尾
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            let (s, _usage) = sink();
            let src = futures_util::stream::iter(vec![
                Ok::<Bytes, BoxError>(Bytes::from_static(
                    b"data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
                )),
            ]);
            let mut st = std::pin::pin!(openai_sse_stream_inner(src, "mock-ot-chat".into(), s));
            let mut acc: Vec<u8> = Vec::new();
            while let Some(item) = st.next().await {
                acc.extend_from_slice(&item.expect("EOF 不应向下游传播 Err"));
            }
            let text = String::from_utf8(acc).unwrap();
            assert!(text.contains("event: error"), "缺 error 事件: {text}");
            assert!(text.contains("上游流提前结束"), "缺提前结束提示: {text}");
            assert!(!text.contains("message_stop"), "截断流不应伪造 message_stop: {text}");

            // 正常流：finish_reason + [DONE] → 无 error（回归，兼容缺 [DONE] 站点）
            for tail in [
                "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            ] {
                let (s2, _usage) = sink();
                let mut full = b"data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
                full.extend_from_slice(tail.as_bytes());
                let src2 = futures_util::stream::iter(vec![Ok::<Bytes, BoxError>(Bytes::from(full))]);
                let mut st2 = std::pin::pin!(openai_sse_stream_inner(src2, "mock-ot-chat".into(), s2));
                let mut acc2: Vec<u8> = Vec::new();
                while let Some(item) = st2.next().await {
                    acc2.extend_from_slice(&item.expect("正常流不应出错"));
                }
                let text2 = String::from_utf8(acc2).unwrap();
                assert!(text2.contains("event: message_stop"), "正常流应有 message_stop: {text2}");
                assert!(!text2.contains("event: error"), "正常流不应产生 error: {text2}");
            }
        });
    }

    #[test]
    fn gemini_stream_premature_eof_emits_error_event() {
        // Phase 61：Gemini 流 EOF（未收到 finishReason 块）→ error 事件而非伪造收尾
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            let (s, _usage) = sink();
            let src = futures_util::stream::iter(vec![
                Ok::<Bytes, BoxError>(Bytes::from_static(
                    b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
                )),
            ]);
            let mut st = std::pin::pin!(gemini_sse_stream_inner(src, "mock-gem-chat".into(), s));
            let mut acc: Vec<u8> = Vec::new();
            while let Some(item) = st.next().await {
                acc.extend_from_slice(&item.expect("EOF 不应向下游传播 Err"));
            }
            let text = String::from_utf8(acc).unwrap();
            assert!(text.contains("event: error"), "缺 error 事件: {text}");
            assert!(text.contains("上游流提前结束"), "缺提前结束提示: {text}");
            assert!(!text.contains("message_stop"), "截断流不应伪造 message_stop: {text}");

            // 正常流（finishReason STOP）EOF → 无 error（回归）
            let (s2, _usage) = sink();
            let src2 = futures_util::stream::iter(vec![
                Ok::<Bytes, BoxError>(Bytes::from_static(
                    b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":2}}\n\n",
                )),
            ]);
            let mut st2 = std::pin::pin!(gemini_sse_stream_inner(src2, "mock-gem-chat".into(), s2));
            let mut acc2: Vec<u8> = Vec::new();
            while let Some(item) = st2.next().await {
                acc2.extend_from_slice(&item.expect("正常流不应出错"));
            }
            let text2 = String::from_utf8(acc2).unwrap();
            assert!(text2.contains("event: message_stop"), "正常流应有 message_stop: {text2}");
            assert!(!text2.contains("event: error"), "正常流不应产生 error: {text2}");
        });
    }
}
