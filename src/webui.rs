//! Web UI：内嵌配置页 `webui/index.html` + `/api/config` 写 + `/api/status` 读。
//!
//! `/api/config` GET（掩码 Key）在 api.rs；PUT 在此：解析 → 掩码 Key 复用 →
//! 校验 → 写 config.json → 热重载（Arc<RwLock> 原地替换，下一请求即生效）。

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

use crate::config::{AppState, Config};
use crate::router;

/// 内嵌的纯前端配置页（单文件、无外部依赖）。
pub const INDEX_HTML: &str = include_str!("../webui/index.html");

fn json_response(status: StatusCode, v: serde_json::Value) -> Response {
    let body = serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
    let mut r = (status, body).into_response();
    if let Ok(ct) = HeaderValue::from_str("application/json") {
        r.headers_mut().insert("content-type", ct);
    }
    r
}

/// PUT /api/config：保存配置并热重载。
pub async fn config_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let mut cfg: Config = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": format!("invalid config JSON: {e}") }),
            )
        }
    };

    // 掩码 Key 复用：GET 返回的 Key 带 `*`，未改动时按 id 用回当前真实 Key；
    // 新服务商带掩码 Key 属误填，直接拒绝。
    // Phase 28：console_cookie 同机制（掩码往返，未改复用真实值）。
    // Phase 49：掩码判据用 `starts_with('*')`——`contains('*')` 会把真实含 `*` 的
    // cookie/key 误判为掩码（复用旧值 → 保存不生效）。掩码输出必然以 `*` 开头。
    {
        let current = state.cfg.read().await;
        for p in &mut cfg.providers {
            if is_masked(&p.api_key) {
                match current.providers.iter().find(|op| op.id == p.id) {
                    Some(op) => p.api_key = op.api_key.clone(),
                    None => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            serde_json::json!({
                                "ok": false,
                                "error": format!(
                                    "provider '{}': api_key 是掩码值，新服务商必须填写真实 Key",
                                    p.id
                                ),
                            }),
                        )
                    }
                }
            }
            // Phase 53c：console_cookie 明文展示/提交，PUT 直接存前端传回的明文（无需掩码复用）
        }
    }

    let mut warnings = match cfg.validate() {
        Ok(w) => w,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": e }),
            )
        }
    };

    if let Err(e) = Config::save(&state.config_path, &cfg) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": format!("save config: {e}") }),
        );
    }

    *state.cfg.write().await = cfg.clone();

    // 开机自启开关保存即生效：写/删 HKCU Run 键（幂等，失败仅告警不阻断保存）
    if let Err(e) = crate::autostart::apply(cfg.server.auto_start) {
        warnings.push(format!(
            "开机自启{}失败: {e}",
            if cfg.server.auto_start { "注册" } else { "移除" }
        ));
    }

    let aliases = router::available_aliases(&cfg.providers, &cfg.server.separator);
    if state.debug {
        eprintln!(
            "[config] saved {} providers, {} aliases → {}",
            cfg.providers.len(),
            aliases.len(),
            state.config_path.display()
        );
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "warnings": warnings,
            "providers": cfg.providers.len(),
            "aliases": aliases,
        }),
    )
}

/// POST /api/restart：保存配置后重启 —— 拉起一个同参数的新进程，然后本进程退出。
///
/// 时序：spawn 子进程 → 返回响应（客户端先拿到 `{ok,restarting}`）→ 600ms 后
/// `exit(0)` 让出端口。新进程由 main.rs 的 `bind_with_retry` 扛过旧进程未退出瞬间。
/// 子进程继承本进程的控制台/stdout（重定向到日志文件时也一并继承），无新窗口弹出。
///
/// **关键（Phase 20 修复）**：spawn 参数追加 `--restart`。否则新进程启动时
/// `port_is_busy` 检测到旧进程尚在占用端口（600ms 后才退出）→ 误判"已有 relay 在运行"
/// → 直接退出 → 旧进程随后退出 → 服务整体被杀。`--restart` 让新进程跳过忙端口检查，
/// 由 `bind_with_retry`（20×150ms）扛过新旧进程交接瞬间。
pub async fn restart(State(_state): State<Arc<AppState>>) -> Response {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "ok": false, "error": format!("current_exe: {e}") }),
            )
        }
    };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    args.push("--restart".into());
    if let Err(e) = std::process::Command::new(&exe).args(&args).spawn() {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": format!("restart spawn: {e}") }),
        );
    }
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        std::process::exit(0);
    });
    json_response(StatusCode::OK, serde_json::json!({ "ok": true, "restarting": true }))
}

/// POST /api/update-settings：把 relay 的 BASE_URL/API_KEY/MODEL + 三个任务槽 env 合并写入
/// Claude settings 文件（默认 `~/.claude/settings.json`，尊重 `CLAUDE_CONFIG_DIR`）。
/// **Phase 22（用户确认）**：`ANTHROPIC_MODEL` 写**哨兵值** `claude-relay-model[上下文]`
/// （CC 发回哨兵后由中继替换为「当前使用模型」），且**顶层 `model` 键 + env 三键都写**
/// （实测会话用的是顶层 model 键，仅写 env 不生效）。文件其余内容全部保留。
/// **Phase 24**：三个任务槽 `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` 也写哨兵值——
/// CC 的子代理 / `/compact` / `/loop` 等后台任务走独立槽位（不跟 `/model`），裸名
/// （`claude-opus-5` 等）中继无法路由会 400，映射到哨兵后跟随页面「当前使用模型」。
///
/// **Phase 42**：核心逻辑抽到同步 `write_claude_settings`，async handler 与托盘线程
/// （`update_settings_blocking`）共用。
pub async fn update_settings(State(state): State<Arc<AppState>>) -> Response {
    let (host, port, model) = {
        let cfg = state.cfg.read().await;
        (
            cfg.server.host.clone(),
            cfg.server.port,
            cfg.sentinel_model(),
        )
    };
    let path = match claude_settings_path() {
        Some(p) => p,
        None => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "ok": false, "error":
                    "cannot determine settings path (no HOME/USERPROFILE/CLAUDE_CONFIG_DIR)" }),
            )
        }
    };
    match write_claude_settings(&host, port, &model, &path) {
        Ok((base_url, model)) => {
            if state.debug {
                eprintln!("[config] settings updated → {}", path.display());
            }
            json_response(
                StatusCode::OK,
                serde_json::json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "base_url": base_url,
                    "api_key": "relay-dummy",
                    "model": model,
                }),
            )
        }
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "ok": false, "error": e })),
    }
}

/// Phase 42：托盘线程阻塞版——从当前配置快照写 claude settings（std 线程不能 await，
/// 用 `RwLock::blocking_read`，同 Phase 25 `set_default_model_blocking` 的 `blocking_write` 模式）。
/// 返回 `(base_url, model, settings_path)` 供托盘气泡/日志反馈。
pub fn update_settings_blocking(state: &std::sync::Arc<AppState>) -> Result<(String, String, String), String> {
    let (host, port, model) = {
        let cfg = state.cfg.blocking_read();
        (
            cfg.server.host.clone(),
            cfg.server.port,
            cfg.sentinel_model(),
        )
    };
    let path = claude_settings_path().ok_or_else(|| {
        "cannot determine settings path (no HOME/USERPROFILE/CLAUDE_CONFIG_DIR)".to_string()
    })?;
    let (base_url, model) = write_claude_settings(&host, port, &model, &path)?;
    Ok((base_url, model, path.display().to_string()))
}

/// 写 claude settings 文件的同步核心（async handler 与托盘线程共用）：
/// 读入现有文件（缺失→空）→ 合并 env 6 键 + 顶层 model → 写回，其余内容全部保留。
fn write_claude_settings(
    host: &str,
    port: u16,
    model: &str,
    path: &std::path::Path,
) -> Result<(String, String), String> {
    let base_url = format!("http://{host}:{port}");
    let api_key = "relay-dummy"; // 中继惯例：CC 发这个假 Key，中继替换为服务商真实 Key

    let mut doc: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!("{} 不是合法 JSON: {e}", path.display()))
            }
        },
        Err(_) => serde_json::json!({}),
    };

    if !doc.get("env").map_or(false, |e| e.is_object()) {
        doc["env"] = serde_json::json!({});
    }
    if let Some(env) = doc.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.insert("ANTHROPIC_BASE_URL".into(), serde_json::Value::String(base_url.clone()));
        env.insert("ANTHROPIC_API_KEY".into(), serde_json::Value::String(api_key.into()));
        env.insert("ANTHROPIC_MODEL".into(), serde_json::Value::String(model.to_string()));
        // Phase 24：三个任务槽也指哨兵——CC 的子代理/compact/loop 走独立槽位（不跟 /model），
        // 裸名（claude-opus-5 等）中继无法路由会 400；映射到哨兵后跟随页面「当前使用模型」。
        env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), serde_json::Value::String(model.to_string()));
        env.insert("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), serde_json::Value::String(model.to_string()));
        env.insert("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), serde_json::Value::String(model.to_string()));
    }
    // Phase 22：顶层 `model` 键也写哨兵值（实测 CC 会话用的是顶层 model 键，优先于 env）。
    doc["model"] = serde_json::Value::String(model.to_string());

    let out = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("serialize: {e}"))?;
    // Phase 59：统一走自愈式原子写——只读属性自动解除、父目录自动创建（新装机器 ~/.claude 首写）、
    // 原子替换（写一半崩溃不损坏 settings）。
    crate::config::write_file_atomic(path, &out)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok((base_url, model.to_string()))
}

/// 解析 claude settings 文件路径：优先 `CLAUDE_CONFIG_DIR`，否则 `~/.claude/settings.json`。
fn claude_settings_path() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Some(std::path::PathBuf::from(dir).join("settings.json"));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(std::path::PathBuf::from(home).join(".claude").join("settings.json"))
}

/// GET /api/status：运行状态 + 已注册模型别名列表 + 模型详情（含上下文大小）+ 用量统计。
pub async fn status(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.cfg.read().await;
    let aliases = router::available_aliases(&cfg.providers, &cfg.server.separator);
    // 模型详情（provider/alias/real/context/full）——供 UI 展示上下文大小、默认模型下拉用。
    let details: Vec<serde_json::Value> = cfg
        .providers
        .iter()
        .flat_map(|p| {
            p.models.iter().map(|m| {
                serde_json::json!({
                    "provider": p.id,
                    "alias": m.alias,
                    "real": m.real,
                    "context": m.context,
                    "full": format!("{}{}{}", p.id, cfg.server.separator, m.alias),
                })
            })
        })
        .collect();
    // Phase 26：用量统计（全部已配置服务商，无记录填 0；自启动以来，重启清零）。
    // Phase 57：hide_usage 的服务商不进 usage（模型详情/下拉不受影响）。
    let usage: Vec<serde_json::Value> = cfg
        .providers
        .iter()
        .filter(|p| !p.hide_usage)
        .map(|p| {
            let pu = state.usage.lock().unwrap().providers.get(&p.id).cloned();
            let (requests, input_tokens, output_tokens) = pu
                .as_ref()
                .map_or((0, 0, 0), |x| (x.requests, x.input_tokens, x.output_tokens));
            let models: Vec<serde_json::Value> = pu
                .map(|x| {
                    x.by_model
                        .iter()
                        .map(|(m, mu)| {
                            serde_json::json!({
                                "model": m,
                                "requests": mu.requests,
                                "input_tokens": mu.input_tokens,
                                "output_tokens": mu.output_tokens,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "provider": p.id,
                "requests": requests,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "models": models,
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "version": crate::api::VERSION,
            "running": true,
            "host": cfg.server.host,
            "port": cfg.server.port,
            "separator": cfg.server.separator,
            "providers": cfg.providers.len(),
            "models": aliases,
            "model_details": details,
            "usage": usage,
            "uptime_secs": state.started.elapsed().as_secs(),
        }),
    )
}

/// GET /api/usage：Phase 54——用量统计含时间桶（每小时/每日），供「使用情况」卡片、
/// 模型详情弹窗与大屏展示。与 `/api/status` 的 `usage` 同构，但每个模型多 `time` 字段。
pub async fn usage(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.cfg.read().await;
    let usage = state.usage.lock().unwrap().clone();
    // Phase 57：hide_usage 的服务商不进时间桶列表（与大屏/使用情况卡片口径一致）
    let list: Vec<serde_json::Value> = cfg
        .providers
        .iter()
        .filter(|p| !p.hide_usage)
        .map(|p| {
            let pu = usage.providers.get(&p.id).cloned();
            let (requests, input_tokens, output_tokens) = pu
                .as_ref()
                .map_or((0, 0, 0), |x| (x.requests, x.input_tokens, x.output_tokens));
            let models: Vec<serde_json::Value> = pu
                .map(|x| {
                    x.by_model
                        .iter()
                        .map(|(m, mu)| {
                            let mut hours: Vec<serde_json::Value> = mu
                                .time
                                .hours
                                .iter()
                                .map(|b| {
                                    serde_json::json!({
                                        "ts": b.ts,
                                        "requests": b.requests,
                                        "input_tokens": b.input_tokens,
                                        "output_tokens": b.output_tokens,
                                    })
                                })
                                .collect();
                            hours.sort_by_key(|v| v["ts"].as_i64().unwrap_or(0));
                            let mut days: Vec<serde_json::Value> = mu
                                .time
                                .days
                                .iter()
                                .map(|b| {
                                    serde_json::json!({
                                        "ts": b.ts,
                                        "requests": b.requests,
                                        "input_tokens": b.input_tokens,
                                        "output_tokens": b.output_tokens,
                                    })
                                })
                                .collect();
                            days.sort_by_key(|v| v["ts"].as_i64().unwrap_or(0));
                            serde_json::json!({
                                "model": m,
                                "requests": mu.requests,
                                "input_tokens": mu.input_tokens,
                                "output_tokens": mu.output_tokens,
                                "time": { "hours": hours, "days": days },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "provider": p.id,
                "requests": requests,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "models": models,
            })
        })
        .collect();
    json_response(StatusCode::OK, serde_json::json!({ "ok": true, "usage": list }))
}

/// GET /api/balance：查询各服务商余额（Phase 26）。
/// **只返回有简单余额接口的服务商**（Phase 29：火山/讯飞等无接口的不再列出「无简单余额接口」，
/// 待服务商提供可查询 API 后在 `balance_root` 加识别即自动出现）。目前仅 DeepSeek——
/// `GET {主机根}/user/balance`（同款 key，真机实测可用，余额接口在主机根而非 `/anthropic` 子路径）。
/// 单次查询 8s 超时，不缓存（UI 手动刷新）。
pub async fn balance(State(state): State<Arc<AppState>>) -> Response {
    let providers = state.cfg.read().await.providers.clone();
    let client = &state.client;
    let mut out = Vec::new();
    for p in &providers {
        if p.hide_usage {
            continue; // Phase 57：隐藏余额/用量的服务商不查询、不展示
        }
        let Some(root) = balance_root(&p.base_url) else {
            continue; // 无简单余额接口：不展示，待有 API 支持再加
        };
        let url = format!("{root}/user/balance");
        let mut req = client.get(&url);
        req = if p.auth == "x-api-key" {
            req.header("x-api-key", &p.api_key)
        } else {
            req.bearer_auth(&p.api_key)
        };
        let entry = match tokio::time::timeout(std::time::Duration::from_secs(8), req.send()).await {
            Ok(Ok(r)) if r.status().is_success() => match r.bytes().await {
                Ok(b) => match serde_json::from_slice::<serde_json::Value>(&b) {
                    Ok(v) => serde_json::json!({
                        "provider": p.id,
                        "name": p.name,
                        "available": v.get("is_available").cloned().unwrap_or(serde_json::Value::Null),
                        "balance_infos": v.get("balance_infos").cloned().unwrap_or(serde_json::json!([])),
                        "error": serde_json::Value::Null,
                    }),
                    Err(e) => serde_json::json!({
                        "provider": p.id, "name": p.name, "error": format!("响应解析失败: {e}"),
                    }),
                },
                Err(e) => serde_json::json!({
                    "provider": p.id, "name": p.name, "error": format!("读取响应失败: {e}"),
                }),
            },
            Ok(Ok(r)) => serde_json::json!({
                "provider": p.id, "name": p.name, "error": format!("HTTP {}", r.status()),
            }),
            Ok(Err(e)) => serde_json::json!({
                "provider": p.id, "name": p.name, "error": e.to_string(),
            }),
            Err(_) => serde_json::json!({
                "provider": p.id, "name": p.name, "error": "查询超时（8s）",
            }),
        };
        out.push(entry);
    }
    json_response(StatusCode::OK, serde_json::json!({ "balances": out }))
}

/// 从 base_url 提取主机根（剥路径）：`https://api.deepseek.com/anthropic` → `https://api.deepseek.com`。
/// 仅 DeepSeek（`api.deepseek.com`）返回 Some——其余服务商余额无简单 GET 接口。
fn balance_root(base: &str) -> Option<String> {
    if !base.contains("api.deepseek.com") {
        return None;
    }
    let root = match base.find("://") {
        Some(pos) => {
            let rest = &base[pos + 3..];
            match rest.find('/') {
                Some(slash) => format!("{}{}", &base[..pos + 3], &rest[..slash]),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    };
    Some(root)
}

/// GET /api/coding-plan：Coding Plan 用量（Phase 28 火山方舟 5小时/一周/一月 百分比 + 重置时间；
/// Phase 30 讯飞星辰——按 rows 的 `codingPlanAppCredentialDTO.apiKey` 匹配服务商配置的 `api_key`，
/// 多个配置服务商/多个匹配行 → 显示多个套餐）。
///
/// Cookie 源：服务商 `console_cookie`（手动粘贴/「导入 Cookie」的存档）优先；
/// 为空时兜底自动读浏览器（Chrome/Edge）对应控制台域的 Cookie。
/// 服务商按 `console_kind` 分派：火山方舟类 → ark 流程（GetCodingPlanUsage）；讯飞类 → xf 流程（coding-plan/list）。
/// `renew=0`：只读查询（跳过 Set-Cookie 续期写盘）——供桌面卡片这类后台监控调用，
/// 避免只读轮询意外改配置；页面/手动刷新默认续期保鲜。
#[derive(serde::Deserialize, Default)]
pub struct CodingPlanQuery {
    renew: Option<String>,
}

pub async fn coding_plan(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CodingPlanQuery>,
) -> Response {
    let renew = q.renew.as_deref() != Some("0");
    let providers = state.cfg.read().await.providers.clone();
    let client = &state.client;
    let mut out = Vec::new();
    // Phase 53：成功响应带 Set-Cookie → 续期存档 Cookie（页面刷新也能保鲜，不必等每小时任务）
    let mut renewals: Vec<(String, String)> = Vec::new();
    for p in &providers {
        if p.hide_usage {
            continue; // Phase 57：隐藏余额/用量的服务商不查询 Coding Plan（不展示）
        }
        let Some(kind) = console_kind(p) else { continue; };
        let url = match kind {
            ConsoleKind::Ark => Ok(console_usage_url(p)),
            ConsoleKind::Xf => Ok(xf_usage_url(p)),
            ConsoleKind::Oc => oc_usage_url(p), // OpenCode 必须配 /go 页地址（含工作区 id）
        };
        let url = match url {
            Ok(u) => u,
            Err(e) => {
                out.push(serde_json::json!({
                    "provider": p.id, "name": p.name, "ok": false, "error": e,
                }));
                continue;
            }
        };
        let referer = match kind {
            ConsoleKind::Ark => "https://console.volcengine.com/",
            ConsoleKind::Xf => "https://maas.xfyun.cn/",
            ConsoleKind::Oc => "https://opencode.ai/",
        };
        let (cookie, cookie_source) = match provider_cookie(kind, p) {
            Ok(c) => c,
            Err(e) => {
                out.push(serde_json::json!({
                    "provider": p.id, "name": p.name, "ok": false,
                    "error": format!("{e}（点「导入 Cookie」或手动粘贴）"),
                }));
                continue;
            }
        };
        let req = client
            .get(&url)
            .header("cookie", &cookie)
            .header("referer", referer)
            .header(
                "accept",
                if kind == ConsoleKind::Oc {
                    "text/html,application/xhtml+xml"
                } else {
                    "application/json"
                },
            )
            .header("user-agent", format!("Mozilla/5.0 ClaudeRelay/{}", crate::api::VERSION));
        let req = oc_console_request(req, p);
        let entry = match tokio::time::timeout(std::time::Duration::from_secs(8), req.send()).await {
            Ok(Ok(r)) if r.status().is_success() => {
                // Phase 53：先捕获 Set-Cookie（续期用），再读 body 解析；顺带打印续期状态 + 诊断 JSON。
                // Phase 53d：按 key 合并——只更新响应 Set-Cookie 里的 key，旧存档其它 key 保留
                let renew = merge_cookies(&cookie, &parse_set_cookies(r.headers()));
                log_cookie_renewal(&p.id, &cookie, &renew);
                let renewal_status = if renew.is_empty() {
                    "none"
                } else if renew == cookie {
                    "same"
                } else {
                    "renewed"
                };
                let mut renewal = renewal_json(renewal_status, Some(200), &cookie, Some(&renew));
                // Phase 53e：浏览器 console 也展示逐 key 对比明细
                if !renew.is_empty() {
                    renewal["detail"] = serde_json::json!(renewal_detail_lines(&cookie, &renew));
                }
                if !renew.is_empty() && renew != cookie {
                    renewals.push((p.id.clone(), renew));
                }
                match r.bytes().await {
                Ok(b) => match kind {
                    ConsoleKind::Ark => match parse_coding_plan(&b) {
                        Ok(levels) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": true,
                            "cookie_source": cookie_source, "kind": kind.as_str(), "levels": levels,
                            "renewal": renewal,
                        }),
                        Err(e) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": false,
                            "error": format!("响应解析失败: {e}"), "renewal": renewal,
                        }),
                    },
                    ConsoleKind::Xf => match parse_xf_coding_plan(&b, &p.api_key) {
                        Ok(plans) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": true,
                            "cookie_source": cookie_source, "kind": kind.as_str(), "plans": plans,
                            "renewal": renewal,
                        }),
                        Err(e) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": false,
                            "error": format!("响应解析失败: {e}"), "renewal": renewal,
                        }),
                    },
                    ConsoleKind::Oc => match parse_oc_usage(&b) {
                        Ok(levels) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": true,
                            "cookie_source": cookie_source, "kind": kind.as_str(), "levels": levels,
                            "renewal": renewal,
                        }),
                        Err(e) => serde_json::json!({
                            "provider": p.id, "name": p.name, "ok": false,
                            "error": format!("响应解析失败: {e}"), "renewal": renewal,
                        }),
                    },
                },
                Err(e) => serde_json::json!({
                    "provider": p.id, "name": p.name, "ok": false, "error": format!("读取响应失败: {e}"),
                    "renewal": renewal,
                }),
                } // 内层 match r.bytes().await（块尾表达式，无需逗号）
            }, // 外层 Ok(Ok(r)) if success 块（含 Set-Cookie 捕获）
            Ok(Ok(r)) => {
                let status = r.status();
                // Phase 53c：非 2xx（如火山方舟 401）→ 读 body（截断 200 字符）放 error，
                // 便于诊断原因（如「未登录/登录失效」）。401 时上游不会回填 Set-Cookie。
                let preview = match r.text().await {
                    Ok(t) => {
                        let t = t.trim();
                        if t.len() > 200 {
                            format!("{}…", &t[..200])
                        } else {
                            t.to_string()
                        }
                    }
                    Err(_) => String::new(),
                };
                serde_json::json!({
                    "provider": p.id, "name": p.name, "ok": false,
                    "error": if preview.is_empty() {
                        format!("HTTP {}", status)
                    } else {
                        format!("HTTP {}: {}", status, preview)
                    },
                    "renewal": renewal_json("http_error", Some(status.as_u16()), &cookie, None),
                })
            }
            Ok(Err(e)) => serde_json::json!({
                "provider": p.id, "name": p.name, "ok": false, "error": e.to_string(),
                "renewal": renewal_json("request_failed", None, &cookie, None),
            }),
            Err(_) => serde_json::json!({
                "provider": p.id, "name": p.name, "ok": false, "error": "查询超时（8s）",
                "renewal": renewal_json("request_failed", None, &cookie, None),
            }),
        };
        out.push(entry);
    }
    // Phase 53：把本次查询捕获的 Set-Cookie 续期写回存档（有变化才写盘）。
    // `renew=0`（桌面卡片只读轮询）时跳过——只读监控不改配置。
    if renew && !renewals.is_empty() {
        apply_cookie_renewals(&state, renewals).await;
    }
    json_response(StatusCode::OK, serde_json::json!({ "coding_plans": out }))
}

/// Coding Plan 用量查询的服务商类别（Phase 28/30）：火山方舟（ark）与讯飞星辰（xf）各有独立端点/响应格式；
/// Phase 48：OpenCode（oc）用量在 `/go` 页面 HTML 里。
#[derive(Clone, Copy, PartialEq, Debug)]
enum ConsoleKind {
    Ark,
    Xf,
    Oc,
}
impl ConsoleKind {
    fn as_str(self) -> &'static str {
        match self {
            ConsoleKind::Ark => "ark",
            ConsoleKind::Xf => "xf",
            ConsoleKind::Oc => "oc",
        }
    }
}

/// 判断服务商属于哪类 Coding Plan 控制台（None = 无用量可查）：
/// 优先按 base_url 域名；未识别域名时按 `console_usage_url` 路径（含 `coding-plan/list` → xf，
/// 含 `/workspace/` 或 `opencode` → oc，其余默认 ark——供 e2e 让 mock 指到本地端点）；
/// 最后存档了 Cookie 也默认 ark（Phase 28 向后兼容）。
fn console_kind(p: &crate::config::ProviderConfig) -> Option<ConsoleKind> {
    // 讯飞星辰：控制台 `maas.xfyun.cn`、Anthropic 端点 `*.xf-yun.com`（不同域，都属讯飞）
    if p.base_url.contains("xfyun.cn") || p.base_url.contains("xf-yun.com") {
        return Some(ConsoleKind::Xf);
    }
    if p.base_url.contains("volces.com") || p.base_url.contains("volcengine.com") {
        return Some(ConsoleKind::Ark);
    }
    if p.base_url.to_ascii_lowercase().contains("opencode") {
        return Some(ConsoleKind::Oc);
    }
    let url = p.console_usage_url.trim();
    if !url.is_empty() {
        let lower = url.to_ascii_lowercase();
        if lower.contains("/workspace/") || lower.contains("opencode") {
            return Some(ConsoleKind::Oc);
        }
        return Some(if url.contains("coding-plan/list") {
            ConsoleKind::Xf
        } else {
            ConsoleKind::Ark
        });
    }
    if !p.console_cookie.trim().is_empty() {
        return Some(ConsoleKind::Ark);
    }
    None
}

/// Phase 49：判断某个字符串是不是「掩码展示值」（`masked()` 输出）。
/// 掩码串必然以 `*` 开头（`*`×n + 尾 8 真实字符）；真实 cookie/key 值几乎不会以 `*` 开头，
/// 而 `contains('*')` 会把真实含 `*` 的值误判为掩码 → 复用旧值导致保存不生效。
fn is_masked(s: &str) -> bool {
    s.starts_with('*')
}

/// 取服务商的控制台 Cookie：存档 `console_cookie` 优先；否则自动读浏览器对应控制台域。
fn provider_cookie(
    kind: ConsoleKind,
    p: &crate::config::ProviderConfig,
) -> Result<(String, &'static str), String> {
    if !p.console_cookie.trim().is_empty() {
        return Ok((p.console_cookie.clone(), "saved"));
    }
    let c = read_browser_cookie(kind)?;
    Ok((c, "browser"))
}

/// 从浏览器（Chrome/Edge）读取某服务商控制台域的 Cookie（按类别分派）。
/// 供 Coding Plan 查询兜底（provider_cookie：`console_cookie` 为空时）、手动「导入 Cookie」（import_cookie）。
fn read_browser_cookie(kind: ConsoleKind) -> Result<String, String> {
    match kind {
        ConsoleKind::Ark => crate::cookies::read_volcengine_console_cookie(),
        ConsoleKind::Xf => crate::cookies::read_xf_console_cookie(),
        ConsoleKind::Oc => crate::cookies::read_opencode_console_cookie(),
    }
}

/// Phase 53：每小时自动刷新 Coding Plan 控制台 Cookie——用**存档 Cookie**（浏览器没登录也能用）
/// 请求各服务商用量接口，从响应 `Set-Cookie` 头取新 Cookie 续期，保持不过期（隔天不必重新登录）。
///
/// 流程（async，供后台任务 tokio::spawn 调用）：读配置快照 → 对每个有 `console_kind` 的服务商
/// 用 `provider_cookie`（存档优先，空则兜底浏览器）发用量请求 → 成功响应带 `Set-Cookie` 且与
/// 存档不同 → 统一写盘+热重载。浏览器未登录/无存档、请求失败、无 Set-Cookie、值未变化 → 保留原值零写盘。
///
/// 返回实际更新的服务商 id 列表（空 = 无变化）。
pub async fn refresh_console_cookies(state: Arc<AppState>) -> Vec<String> {
    let providers = state.cfg.read().await.providers.clone();
    let mut renewals: Vec<(String, String)> = Vec::new();
    for p in &providers {
        let Some(kind) = console_kind(p) else { continue; };
        let url = match kind {
            ConsoleKind::Ark => console_usage_url(p),
            ConsoleKind::Xf => xf_usage_url(p),
            ConsoleKind::Oc => match oc_usage_url(p) {
                Ok(u) => u,
                Err(_) => continue, // 未配 /go 页地址 → 跳过
            },
        };
        // 存档 Cookie 优先；无存档才兜底浏览器（浏览器没登录则 provider_cookie Err → 跳过）
        let (cookie, _src) = match provider_cookie(kind, p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let referer = match kind {
            ConsoleKind::Ark => "https://console.volcengine.com/",
            ConsoleKind::Xf => "https://maas.xfyun.cn/",
            ConsoleKind::Oc => "https://opencode.ai/",
        };
        let req = state
            .client
            .get(&url)
            .header("cookie", &cookie)
            .header("referer", referer)
            .header(
                "accept",
                if kind == ConsoleKind::Oc {
                    "text/html,application/xhtml+xml"
                } else {
                    "application/json"
                },
            )
            .header("user-agent", format!("Mozilla/5.0 ClaudeRelay/{}", crate::api::VERSION));
        // 成功响应才续期——401/超时说明 Cookie 已失效或网络问题，不把好 Cookie 覆盖成坏的
        let req = oc_console_request(req, p);
        let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(8), req.send()).await
        else {
            continue;
        };
        if !r.status().is_success() {
            continue;
        }
        // Phase 53d：按 key 合并——只更新响应 Set-Cookie 里的 key，旧存档其它 key 保留
        let renew = merge_cookies(&cookie, &parse_set_cookies(r.headers()));
        log_cookie_renewal(&p.id, &cookie, &renew);
        if !renew.is_empty() && renew != cookie {
            renewals.push((p.id.clone(), renew));
        }
    }
    apply_cookie_renewals(&state, renewals).await
}

/// 从响应 `Set-Cookie` 头解析 `(name, value)` 对：每个 `Set-Cookie` 取 `;` 前第一段 `name=value`
/// （去 Path/HttpOnly 等属性）；同名后者覆盖（HTTP 语义）；**值为空 = 上游在清除该 Cookie**
/// （如 `sessionid=; Expires=...`）→ 跳过，勿把空值存进存档。无有效 `Set-Cookie` → 空 Vec。
fn parse_set_cookies(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    for v in headers.get_all("set-cookie") {
        let Ok(s) = v.to_str() else { continue };
        let Some(pair) = s.split(';').next() else { continue };
        let pair = pair.trim();
        let Some((name, value)) = pair.split_once('=') else { continue }; // 无 `=` 不是 Cookie
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        if name.is_empty() || value.is_empty() {
            continue; // 清除型 Set-Cookie（空值）跳过
        }
        if let Some(existing) = parts.iter_mut().find(|(n, _)| *n == name) {
            existing.1 = value;
        } else {
            parts.push((name, value));
        }
    }
    parts
}

/// 解析 Cookie 串（存档/手动粘贴，`name=value; name2=value2`，可能带属性）→ `(name, value)` 对。
/// 容忍属性段（无 `=` 跳过）、空白。值里含 `=` 取第一个（cookie value 不应含 `=`）。
fn parse_cookie_str(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for seg in s.split(';') {
        let seg = seg.trim();
        let Some((name, value)) = seg.split_once('=') else { continue };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), value.to_string()));
    }
    out
}

/// Phase 53d：把响应 Set-Cookie 的 `(name, value)` 更新合并进旧存档 Cookie —— **按 key 合并**：
/// 只更新 `updates` 里出现的 key（同名覆盖、新 key 追加），旧存档里其它 key 原样保留
/// （上游通常只续期 session 相关几个 cookie，整体替换会丢其它 key 导致后续请求出问题）。
/// `updates` 为空（无 Set-Cookie）→ 返回空串（表示未回填）。
fn merge_cookies(old: &str, updates: &[(String, String)]) -> String {
    if updates.is_empty() {
        return String::new();
    }
    let mut map = parse_cookie_str(old);
    for (name, value) in updates {
        match map.iter_mut().find(|(n, _)| *n == *name) {
            Some(e) => e.1 = value.clone(),
            None => map.push((name.clone(), value.clone())),
        }
    }
    map.into_iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Phase 53b/53c/53e：打印上游 Set-Cookie 续期状态 + **逐 key 对比明细**（每次回填的 key、
/// 新 value、旧 value 对比），控制台可观测。Phase 53c：明文展示（用户要求便于调试核对）。
fn log_cookie_renewal(pid: &str, old_cookie: &str, new_cookie: &str) {
    if new_cookie.is_empty() {
        eprintln!("[cookie] 上游未回填 Set-Cookie: {pid}（存档 Cookie 不会自动续期）");
    } else if new_cookie == old_cookie {
        eprintln!("[cookie] 上游回填 Cookie 与存档相同: {pid}");
    } else {
        eprintln!("[cookie] 上游回填 Set-Cookie 已合并续期: {pid}");
    }
    if !new_cookie.is_empty() {
        for line in renewal_detail_lines(old_cookie, new_cookie) {
            eprintln!("{line}");
        }
    }
}

/// 生成续期逐 key 对比明细行（供日志打印 / 浏览器 console 诊断，纯函数可单测）：
/// 每次 Set-Cookie 返回的每个 key → 新 value 与旧存档对应 key 的 value 对比。
/// `未变`=旧存档有且值相同（保留）；`(新增)`=旧存档没有；`(删除)`=旧存档有但新串没了（正常不出现）。
fn renewal_detail_lines(old: &str, new: &str) -> Vec<String> {
    let old_map: std::collections::HashMap<String, String> = parse_cookie_str(old)
        .into_iter()
        .collect();
    let new_map: std::collections::HashMap<String, String> = parse_cookie_str(new)
        .into_iter()
        .collect();
    let mut keys: Vec<&String> = old_map.keys().chain(new_map.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .map(|k| {
            let ov = old_map.get(k);
            let nv = new_map.get(k);
            match (ov, nv) {
                (Some(o), Some(n)) if o == n => format!("  {k}: {o}（未变）"),
                (Some(o), Some(n)) => format!("  {k}: {o} → {n}"),
                (None, Some(n)) => format!("  {k}: (新增) → {n}"),
                (Some(o), None) => format!("  {k}: {o} → (删除)"),
                (None, None) => unreachable!(),
            }
        })
        .collect()
}

/// Phase 53b/53c：构建续期诊断 JSON（前端打印到浏览器 console，方便调试）。
/// `status`：`renewed`（回填且不同，将续期）/ `same`（回填但相同）/ `none`（未回填）/
/// `http_error`（上游非 2xx，如 401，未续期）/ `request_failed`（网络错误/超时，未续期）。
/// Phase 53c：`old`/`new` **明文**展示（用户要求便于判断存档 Cookie 是否有效）。
fn renewal_json(status: &str, http: Option<u16>, old: &str, new: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "status": status,
        "old": old,
    });
    if let Some(h) = http {
        v["http"] = serde_json::json!(h);
    }
    if let Some(n) = new {
        v["new"] = serde_json::json!(n);
    }
    v
}

/// 把 `(provider_id, new_cookie)` 续期应用到配置：非空且与存档不同 → 更新。
/// 有变化才写盘+热重载（返回实际更新的服务商 id，空 = 无变化零 IO）。
/// async：从后台任务/handler（tokio 上下文）调用，用 `.read()/.write().await`（非 blocking 版）。
async fn apply_cookie_renewals(
    state: &AppState,
    renewals: Vec<(String, String)>,
) -> Vec<String> {
    if renewals.is_empty() {
        return Vec::new();
    }
    let mut cfg = state.cfg.read().await.clone();
    let mut ids = Vec::new();
    let mut dirty = false;
    for (pid, newc) in &renewals {
        if newc.trim().is_empty() {
            continue;
        }
        for p in &mut cfg.providers {
            if p.id == *pid && p.console_cookie != *newc {
                p.console_cookie = newc.clone();
                dirty = true;
                if !ids.contains(pid) {
                    ids.push(pid.clone());
                }
            }
        }
    }
    if !dirty {
        return Vec::new();
    }
    if let Err(e) = crate::config::Config::save(&state.config_path, &cfg) {
        eprintln!("[warn] 保存续期 Cookie 失败: {e}");
        return Vec::new();
    }
    *state.cfg.write().await = cfg;
    ids
}

/// 火山方舟用量查询 URL：优先服务商配置 `console_usage_url`；否则按 base_url 区域推导。
fn console_usage_url(p: &crate::config::ProviderConfig) -> String {
    if !p.console_usage_url.trim().is_empty() {
        return p.console_usage_url.trim().to_string();
    }
    let region = base_url_region(&p.base_url);
    format!("https://console.volcengine.com/api/top/ark/{region}/2024-01-01/GetCodingPlanUsage")
}

/// 讯飞星辰 Coding Plan 列表 URL（Phase 30）：优先服务商配置 `console_usage_url`（e2e 指本地 mock）；
/// 否则官方控制台端点 `https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`。
fn xf_usage_url(p: &crate::config::ProviderConfig) -> String {
    if !p.console_usage_url.trim().is_empty() {
        return p.console_usage_url.trim().to_string();
    }
    "https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6".into()
}

/// OpenCode 用量查询 URL（Phase 48）：必须配置 `console_usage_url`——它是含工作区 id 的
/// 新版 `/console/{工作区ID}/go` 是 SPA，改查页面使用的同域 JSON 接口；旧 HTML 地址保持兼容。
fn oc_usage_url(p: &crate::config::ProviderConfig) -> Result<String, String> {
    if oc_workspace_id(&p.console_usage_url).is_some() {
        return Ok("https://opencode.ai/console/api/go/status".into());
    }
    if !p.console_usage_url.trim().is_empty() {
        return Ok(p.console_usage_url.trim().to_string());
    }
    Err("OpenCode 需填写「用量查询 URL」为工作区 /go 页地址（https://opencode.ai/console/{工作区ID}/go）".into())
}

fn oc_workspace_id(page: &str) -> Option<String> {
    let url = reqwest::Url::parse(page.trim()).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("opencode.ai") { return None; }
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if parts.len() != 3 || parts[0] != "console" || parts[2] != "go" { return None; }
    let id = parts[1];
    ((id.starts_with("wrk_") || id.starts_with("org_"))
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .then(|| id.to_string())
}

/// 查询与 Cookie 续期共用新版控制台所需的工作区作用域。
fn oc_console_request(req: reqwest::RequestBuilder, p: &crate::config::ProviderConfig) -> reqwest::RequestBuilder {
    if console_kind(p) == Some(ConsoleKind::Oc) {
        if let Some(id) = oc_workspace_id(&p.console_usage_url) {
            return req.header("x-org-id", id)
                .header("referer", p.console_usage_url.trim())
                .header("accept", "application/json");
        }
    }
    req
}

/// 解析 OpenCode `/go` 页面 HTML（Phase 48）→ 滚动/每周/每月用量 levels。
/// 页面是 SolidStart 服务端渲染，用量数据内嵌在 `lite.subscription.get` 序列化脚本里：
/// `rollingUsage/weeklyUsage/monthlyUsage` 各含 `{status, resetInSec, usagePercent}`。
/// `resetInSec` 是「距重置剩余秒数」→ 换算为绝对时间戳 `reset_ts`。
/// Phase 52：同一 key 可能出现在页面多处（如其它对象里的 `monthlyUsage: null` 占位）——
/// 用 `find_usage_pos` 只取「含 usagePercent 的真实用量对象」；`resetInSec` 用量为 0 时
/// 上游不返回 → 可选，缺失则 `reset_ts=0`（前端 Phase 50 显示「还未使用」）。
fn parse_oc_usage(body: &[u8]) -> Result<Vec<serde_json::Value>, String> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        return parse_oc_status(&value);
    }
    let html = String::from_utf8_lossy(body);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut levels = Vec::new();
    for (key, level, label) in [
        ("rollingUsage", "session", "5小时"),
        ("weeklyUsage", "weekly", "一周"),
        ("monthlyUsage", "monthly", "一月"),
    ] {
        let Some(pos) = find_usage_pos(&html, key) else {
            return Err(format!("页面缺 {key} 用量对象（登录态失效或地址不是 /go 页？）"));
        };
        // 从该 marker 向后取一段，解析对象内字段（顺序无关）
        let end = html.len().min(pos + 300);
        let head = &html[pos..end];
        let reset_ts = grab_int(head, "resetInSec").map(|sec| now + sec).unwrap_or(0);
        let percent = grab_int(head, "usagePercent").unwrap_or(0);
        levels.push(serde_json::json!({
            "level": level,
            "label": label,
            "percent": percent as f64,
            "reset_ts": reset_ts,
        }));
    }
    Ok(levels)
}

/// 新控制台的金额为十进制字符串（MicroCents），三个窗口分别计算已用比例。
fn parse_oc_status(value: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    let meters = value.pointer("/access/meters")
        .ok_or("OpenCode 无有效 Go 套餐用量（登录态失效或订阅已结束）")?;
    let mut levels = Vec::new();
    for (key, level, label) in [("fiveHour", "session", "5小时"), ("week", "weekly", "一周"), ("month", "monthly", "一月")] {
        let meter = &meters[key];
        let number = |field: &str| meter[field].as_u64()
            .or_else(|| meter[field].as_str()?.parse::<u64>().ok())
            .ok_or_else(|| format!("OpenCode {key} 缺少有效 {field}"));
        let used = number("usedMicroCents")?;
        let limit = number("limitMicroCents")?;
        if limit == 0 { return Err(format!("OpenCode {key} 配额为零")); }
        let reset = match meter.get("resetsAt") {
            Some(serde_json::Value::Null) => 0,
            Some(v) => v.as_str().and_then(oc_utc_timestamp)
                .ok_or_else(|| format!("OpenCode {key} 重置时间格式无效"))?,
            None => return Err(format!("OpenCode {key} 缺少重置时间")),
        };
        let percent = ((used as f64 * 100.0 / limit as f64).min(100.0) * 100.0).round() / 100.0;
        levels.push(serde_json::json!({"level":level, "label":label, "percent":percent, "reset_ts":reset}));
    }
    Ok(levels)
}

/// 控制台返回 UTC ISO 时间（可带毫秒）；只需秒精度，无额外日期依赖。
fn oc_utc_timestamp(input: &str) -> Option<u64> {
    let (date, time) = input.strip_suffix('Z')?.split_once('T')?;
    let date: Vec<u64> = date.split('-').map(str::parse).collect::<Result<_, _>>().ok()?;
    let time: Vec<_> = time.split(':').collect();
    if date.len() != 3 || time.len() != 3 { return None; }
    let (year, month, day) = (date[0], date[1], date[2]);
    let hour = time[0].parse::<u64>().ok()?;
    let minute = time[1].parse::<u64>().ok()?;
    let mut seconds = time[2].split('.');
    let second = seconds.next()?.parse::<u64>().ok()?;
    if let Some(fraction) = seconds.next() {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) { return None; }
    }
    if seconds.next().is_some() || !(1970..=9999).contains(&year) || !(1..=12).contains(&month)
        || hour > 23 || minute > 59 || second > 59 { return None; }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day == 0 || day > months[(month - 1) as usize] { return None; }
    let leap_days = |y: u64| y / 4 - y / 100 + y / 400;
    let days = 365 * (year - 1970) + leap_days(year - 1) - leap_days(1969)
        + months[..(month - 1) as usize].iter().sum::<u64>() + day - 1;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

/// 定位 `{key}` 后真正的用量对象：页面里该 key 可能有 `: null` 占位（其它对象里的字段），
/// 依次扫描每个出现位置，跳过冒号后为 `null` 的占位，返回第一个非 null 的真实对象。
fn find_usage_pos(html: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(pos) = html[from..].find(key) {
        let pos = from + pos;
        // `{key}:` 后跳过空白：`null` 是占位，真实对象是 `$R[N] = { ... }` 等非 null 值
        let after = html[pos + key.len()..]
            .strip_prefix(':')
            .map(|s| s.trim_start())
            .unwrap_or("");
        if !after.is_empty() && !after.starts_with("null") {
            return Some(pos);
        }
        from = pos + key.len();
    }
    None
}

/// 从 JS 对象字面量片段提取 `{key}: <十进制整数>`（冒号后可带空白，如 `resetInSec: 12114,`）。
fn grab_int(s: &str, key: &str) -> Option<u64> {
    let pat = format!("{key}:");
    let pos = s.find(&pat)?;
    let rest = &s[pos + pat.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_ascii_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// 从 base_url 提取火山区域：`https://ark.cn-beijing.volces.com/...` → `cn-beijing`；默认 `cn-beijing`。
fn base_url_region(base: &str) -> String {
    if let Some(pos) = base.find("ark.") {
        let rest = &base[pos + 4..];
        let end = rest.find('.').unwrap_or(rest.len());
        if !rest[..end].is_empty() {
            return rest[..end].to_string();
        }
    }
    "cn-beijing".into()
}

/// 解析 GetCodingPlanUsage 响应 → `[{level, label, percent, reset_ts}]`。
fn parse_coding_plan(body: &[u8]) -> Result<Vec<serde_json::Value>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let result = v.get("Result").ok_or("响应无 Result")?;
    let quota = result
        .get("QuotaUsage")
        .and_then(|q| q.as_array())
        .ok_or("响应无 Result.QuotaUsage")?;
    let mut levels = Vec::new();
    for lv in quota {
        let level = lv
            .get("Level")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let label = match level.as_str() {
            "session" => "5小时",
            "weekly" => "一周",
            "monthly" => "一月",
            other => other,
        };
        let percent = lv.get("Percent").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let reset_ts = lv.get("ResetTimestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        levels.push(serde_json::json!({
            "level": level,
            "label": label,
            "percent": (percent * 10.0).round() / 10.0,
            "reset_ts": reset_ts,
        }));
    }
    Ok(levels)
}

/// 解析讯飞星辰 `coding-plan/list` 响应（Phase 30）→ 按 `codingPlanAppCredentialDTO.apiKey` 匹配
/// 服务商配置的 `api_key`，返回匹配套餐列表。每项含套餐名/到期/三档配额（**次数**）：
/// `packageLimit/Usage/Left`=一个月总/已用/剩余次数，`rpwLimit/Usage`=一周总/已用，`rp5hLimit/Usage`=5小时总/已用，
/// `percent`=包月已用百分比。无匹配行 → Err（UI 提示 apiKey 不匹配）。
fn parse_xf_coding_plan(body: &[u8], api_key: &str) -> Result<Vec<serde_json::Value>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let rows = v
        .get("data")
        .and_then(|d| d.get("rows"))
        .and_then(|r| r.as_array())
        .ok_or("响应无 data.rows")?;
    let mut plans = Vec::new();
    for row in rows {
        // 只显示属于本服务商 api_key 的套餐（用户要求：按 apiKey 判断，配置多个就显示多个）
        let row_key = row
            .get("codingPlanAppCredentialDTO")
            .and_then(|c| c.get("apiKey"))
            .and_then(|k| k.as_str())
            .unwrap_or("");
        if row_key != api_key {
            continue;
        }
        let u = row.get("codingPlanUsageDTO");
        let f = |name: &str| {
            u.and_then(|x| x.get(name))
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0)
        };
        let package_usage = f("packageUsage");
        let package_limit = f("packageLimit");
        let percent = if package_limit > 0.0 {
            package_usage / package_limit * 100.0
        } else {
            0.0
        };
        plans.push(serde_json::json!({
            "plan": row.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            "channel": u.and_then(|x| x.get("channel")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            "expires_at": row.get("expiresAt").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            "package_usage": round1(package_usage),
            "package_limit": round1(package_limit),
            "package_left": round1(f("packageLeft")),
            "rp5h_usage": round1(f("rp5hUsage")),
            "rp5h_limit": round1(f("rp5hLimit")),
            "rpw_usage": round1(f("rpwUsage")),
            "rpw_limit": round1(f("rpwLimit")),
            "percent": round1(percent),
        }));
    }
    if plans.is_empty() {
        return Err(format!(
            "未找到 apiKey 匹配的 Coding Plan（期望 apiKey={api_key}；请核对服务商 API Key 与该套餐凭证是否一致，或检查控制台 Cookie 是否有效）"
        ));
    }
    Ok(plans)
}

/// 保留 1 位小数的数值（与 ark percent 归一化一致）。
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// POST /api/import-cookie：从浏览器读取对应控制台域的 Cookie（火山方舟 `console.volcengine.com` /
/// 讯飞星辰 `maas.xfyun.cn`，Phase 30 按服务商类别分派）→ 存服务商 console_cookie → 写盘 + 热重载。
/// Body: `{ "provider": "hs" }`。返回掩码 Cookie 供 UI 确认（不泄露真实值）。
pub async fn import_cookie(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let pid = body
        .get("provider")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let kind = {
        let cfg = state.cfg.read().await;
        if !cfg.providers.iter().any(|p| p.id == pid) {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": format!("服务商 '{pid}' 不存在") }),
            );
        }
        cfg.providers
            .iter()
            .find(|p| p.id == pid)
            .and_then(console_kind)
            .unwrap_or(ConsoleKind::Ark)
    };
    let cookie = read_browser_cookie(kind);
    let cookie = match cookie {
        Ok(c) => c,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": e }),
            )
        }
    };
    let mut cfg = state.cfg.read().await.clone();
    let mut found = false;
    for p in &mut cfg.providers {
        if p.id == pid {
            p.console_cookie = cookie.clone();
            found = true;
            break;
        }
    }
    if !found {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "error": format!("服务商 '{pid}' 不存在") }),
        );
    }
    if let Err(e) = Config::save(&state.config_path, &cfg) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": format!("save config: {e}") }),
        );
    }
    *state.cfg.write().await = cfg;
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "provider": pid,
            // Phase 53c：明文返回完整 Cookie（用户要求便于核对），不再掩码
            "cookie": cookie,
        }),
    )
}

/// POST /api/usage/reset：清零用量统计（Phase 35）。
/// 清空 UsageStats（请求数/输入/输出 tokens，全部服务商+模型）并**立即落盘**
/// `*-usage.json`——周期落盘任务只写脏标记，这里显式 save 让「清零」对重启也生效。
pub async fn usage_reset(State(state): State<Arc<AppState>>) -> Response {
    {
        let mut u = state.usage.lock().unwrap();
        u.providers.clear();
    }
    state
        .usage_dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let path = crate::usage_path_of(&state.config_path);
    if let Err(e) = state.usage.lock().unwrap().save(&path) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": format!("save usage: {e}") }),
        );
    }
    json_response(StatusCode::OK, serde_json::json!({ "ok": true, "cleared": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    #[test]
    fn balance_root_detects_deepseek_only() {
        // Phase 26：只有 api.deepseek.com 有简单 GET 余额接口，且剥 `/anthropic` 取主机根
        assert_eq!(
            balance_root("https://api.deepseek.com/anthropic").as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(
            balance_root("https://api.deepseek.com").as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(balance_root("https://ark.cn-beijing.volces.com/api/coding"), None);
        assert_eq!(balance_root("https://maas-coding-api.cn-huabei-1.xf-yun.com/anthropic"), None);
    }

    fn ark_provider() -> ProviderConfig {
        ProviderConfig {
            id: "hs".into(),
            name: String::new(),
            base_url: "https://ark.cn-beijing.volces.com/api/coding".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }
    }

    fn xf_provider() -> ProviderConfig {
        ProviderConfig {
            id: "xf".into(),
            name: String::new(),
            base_url: "https://maas-coding-api.cn-huabei-1.xf-yun.com/anthropic".into(),
            api_key: "XF_KEY".into(),
            auth: "bearer".into(),
            models: vec![],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }
    }

    #[test]
    fn parse_coding_plan_normalizes() {
        // Phase 28：用户提供样例 → 归一化为 5小时/一周/一月 + 百分比 + 重置时间
        let body = br#"{"ResponseMetadata":{"Action":"GetCodingPlanUsage"},"Result":{"Status":"Running","UpdateTimestamp":1785826887,"QuotaUsage":[{"Level":"session","Percent":3.487035,"ResetTimestamp":1785841012},{"Level":"weekly","Percent":3.5022089999999997,"ResetTimestamp":1786291200},{"Level":"monthly","Percent":1.7511044999999998,"ResetTimestamp":1788451199}]}}"#;
        let lv = parse_coding_plan(body).unwrap();
        assert_eq!(lv.len(), 3);
        assert_eq!(lv[0]["label"], "5小时");
        assert!((lv[0]["percent"].as_f64().unwrap() - 3.5).abs() < 1e-9);
        assert_eq!(lv[0]["reset_ts"].as_u64(), Some(1785841012));
        assert_eq!(lv[1]["label"], "一周");
        assert_eq!(lv[2]["label"], "一月");
    }

    #[test]
    fn parse_coding_plan_missing_errors() {
        assert!(parse_coding_plan(br#"{"Result":{}}"#).is_err());
        assert!(parse_coding_plan(b"not json").is_err());
    }

    #[test]
    fn console_usage_url_derivation_and_override() {
        // Phase 28：默认按 base_url 区域推导；配置了 URL 则优先（e2e 指向 mock）
        let p = ark_provider();
        assert_eq!(base_url_region(&p.base_url), "cn-beijing");
        assert_eq!(
            console_usage_url(&p),
            "https://console.volcengine.com/api/top/ark/cn-beijing/2024-01-01/GetCodingPlanUsage"
        );
        let mut p2 = ark_provider();
        p2.console_usage_url = "http://127.0.0.1:9099/GetCodingPlanUsage".into();
        assert_eq!(console_usage_url(&p2), "http://127.0.0.1:9099/GetCodingPlanUsage");
        assert_eq!(console_kind(&p), Some(ConsoleKind::Ark));
        assert_eq!(console_kind(&p2), Some(ConsoleKind::Ark));
    }

    #[test]
    fn console_kind_dispatches_xf() {
        // Phase 30：按 base_url 域名 → xf；未识别域名按 console_usage_url 路径分派；无任何痕迹 → None
        assert_eq!(console_kind(&xf_provider()), Some(ConsoleKind::Xf));
        let mut p = xf_provider();
        // e2e mock 指到本地 xf 端点 → 按 URL 路径识别为 xf
        p.base_url = "http://127.0.0.1:9099".into();
        p.console_usage_url = "http://127.0.0.1:9099/coding-plan/list".into();
        assert_eq!(console_kind(&p), Some(ConsoleKind::Xf));
        // 存档了 Cookie 也默认 ark（Phase 28 向后兼容）
        let mut p2 = ark_provider();
        p2.console_cookie = "sessionid=x".into();
        assert_eq!(console_kind(&p2), Some(ConsoleKind::Ark));
        // 无任何痕迹 → None（不查询）
        let mut p3 = ark_provider();
        p3.base_url = "http://127.0.0.1:9099".into();
        assert_eq!(console_kind(&p3), None);
    }

    #[test]
    fn xf_usage_url_default_and_override() {
        // Phase 30：讯飞默认官方端点；配置了 URL 则优先（e2e 指向 mock）
        assert_eq!(
            xf_usage_url(&xf_provider()),
            "https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6"
        );
        let mut p = xf_provider();
        p.console_usage_url = "http://127.0.0.1:9099/coding-plan/list".into();
        assert_eq!(xf_usage_url(&p), "http://127.0.0.1:9099/coding-plan/list");
    }

    fn oc_provider() -> ProviderConfig {
        ProviderConfig {
            id: "oc".into(),
            name: String::new(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![],
            strip_beta: false,
            rewrite_model: true,
            protocol: "openai".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }
    }

    #[test]
    fn console_kind_detects_opencode() {
        // Phase 48：base_url 含 opencode → oc；或 console_usage_url 含 /workspace/ → oc
        let p = oc_provider();
        assert_eq!(console_kind(&p), Some(ConsoleKind::Oc));
        let mut p2 = oc_provider();
        p2.base_url = "http://127.0.0.1:9099".into();
        p2.console_usage_url = "https://opencode.ai/workspace/wrk_01KZQ6TA0DCPT2KZ1H43BG23R5/go".into();
        assert_eq!(console_kind(&p2), Some(ConsoleKind::Oc));
        // 无任何痕迹 → None
        let mut p3 = oc_provider();
        p3.base_url = "http://127.0.0.1:9099".into();
        assert_eq!(console_kind(&p3), None);
    }

    #[test]
    fn oc_usage_url_requires_workspace_page() {
        // Phase 48：OpenCode 的用量 URL 必须配 /go 页地址（工作区 id 在其中），空 → Err 提示
        assert!(oc_usage_url(&oc_provider()).is_err());
        let mut p = oc_provider();
        p.console_usage_url = "https://opencode.ai/workspace/wrk_01KZQ6TA0DCPT2KZ1H43BG23R5/go".into();
        assert_eq!(
            oc_usage_url(&p).unwrap(),
            "https://opencode.ai/workspace/wrk_01KZQ6TA0DCPT2KZ1H43BG23R5/go"
        );
    }

    #[test]
    fn oc_console_page_uses_scoped_status_request() {
        let mut p = oc_provider();
        p.console_usage_url = "https://opencode.ai/console/wrk_example/go/?view=usage".into();
        assert_eq!(oc_usage_url(&p).unwrap(), "https://opencode.ai/console/api/go/status");
        let request = oc_console_request(reqwest::Client::new().get(oc_usage_url(&p).unwrap()), &p).build().unwrap();
        assert_eq!(request.headers()["x-org-id"], "wrk_example");
        assert_eq!(request.headers()["referer"], p.console_usage_url);
        assert_eq!(request.headers()["accept"], "application/json");
        assert!(oc_workspace_id("https://opencode.ai.evil.test/console/wrk_example/go").is_none());
        assert!(oc_workspace_id("http://opencode.ai/console/wrk_example/go").is_none());
        assert!(oc_workspace_id("https://opencode.ai/console/api/go/status").is_none());
        assert!(oc_workspace_id("https://opencode.ai/workspace/wrk_example/go").is_none());
    }

    #[test]
    fn oc_status_parses_precise_usage_and_resets() {
        let body = serde_json::json!({"access":{"meters":{
            "fiveHour":{"usedMicroCents":"1195046","limitMicroCents":"1200000000","resetsAt":"2026-10-20T14:22:32.000Z"},
            "week":{"usedMicroCents":300,"limitMicroCents":200,"resetsAt":"1970-01-01T00:00:00Z"},
            "month":{"usedMicroCents":"0","limitMicroCents":"6000000000","resetsAt":null}
        }}});
        let levels = parse_oc_usage(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(levels[0]["percent"], 0.10);
        assert_eq!(levels[0]["reset_ts"], 1792506152u64);
        assert_eq!(levels[1]["percent"], 100.0);
        assert_eq!(levels[2]["percent"], 0.0);
        assert_eq!(levels[2]["reset_ts"], 0);
        let mut invalid = body.clone();
        invalid["access"]["meters"]["fiveHour"]["limitMicroCents"] = serde_json::json!(0);
        assert!(parse_oc_status(&invalid).is_err());
        invalid["access"]["meters"]["fiveHour"]["limitMicroCents"] = serde_json::json!(100);
        invalid["access"]["meters"]["fiveHour"]["usedMicroCents"] = serde_json::Value::Null;
        assert!(parse_oc_status(&invalid).is_err());
        assert!(parse_oc_status(&serde_json::json!({"access":null})).is_err());
        assert!(parse_oc_status(&serde_json::Value::Null).is_err());
    }

    #[test]
    fn oc_reset_timestamp_validates_calendar_dates() {
        assert_eq!(oc_utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(oc_utc_timestamp("2000-02-29T00:00:00.123Z"), Some(951782400));
        for invalid in ["2026-02-29T00:00:00Z", "2100-02-29T00:00:00Z", "2026-10-20T24:00:00Z", "2026-10-20T00:00:00.badZ", "2026-10-20T00:00:00+08:00", "9999999999-01-01T00:00:00Z"] {
            assert_eq!(oc_utc_timestamp(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn parse_oc_usage_extracts_levels() {
        // Phase 48：从 opencode.html 样例（lite.subscription.get 内嵌数据）提取 滚动/每周/每月 用量。
        // resetInSec 是距重置剩余秒数 → reset_ts = now + resetInSec。
        // Phase 52：页面另有 `monthlyUsage: null` 占位（其它对象里），find_usage_pos 须跳过它
        // 命中真实用量对象——否则会误报「monthlyUsage 缺 resetInSec」。
        let body = r#"<html><body><script>
          $R[24] = { balance: 0, monthlyUsage: null, subscription: null };
          $R[28]($R[18], $R[34] = {
              mine: !0,
              useBalance: !1,
              region: $R[35] = ["us", "eu", "sg", "cn"],
              rollingUsage: $R[36] = { status: "ok", resetInSec: 12114, usagePercent: 2 },
              weeklyUsage: $R[37] = { status: "ok", resetInSec: 421682, usagePercent: 15 },
              monthlyUsage: $R[38] = { status: "ok", resetInSec: 2587650, usagePercent: 7 }
          });
        </script></body></html>"#;
        let levels = parse_oc_usage(body.as_bytes()).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0]["level"], "session");
        assert_eq!(levels[0]["label"], "5小时");
        assert_eq!(levels[0]["percent"].as_f64(), Some(2.0));
        assert_eq!(levels[1]["label"], "一周");
        assert_eq!(levels[1]["percent"].as_f64(), Some(15.0));
        assert_eq!(levels[2]["label"], "一月");
        assert_eq!(levels[2]["percent"].as_f64(), Some(7.0));
        // reset_ts = now + resetInSec（秒级允许 2s 误差）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let r0 = levels[0]["reset_ts"].as_u64().unwrap();
        assert!((r0 as i64 - (now + 12114) as i64).abs() <= 2, "rolling reset={r0} now={now}");
    }

    #[test]
    fn parse_oc_usage_zero_usage_missing_reset_ok() {
        // Phase 52：用量为 0 时上游不返回 resetInSec → 不报错，reset_ts=0（前端显示「还未使用」）
        let body = r#"<script>
          rollingUsage: $R[36] = { status: "ok", usagePercent: 0 },
          weeklyUsage: $R[37] = { status: "ok", usagePercent: 0 },
          monthlyUsage: $R[38] = { status: "ok", usagePercent: 0 }
        </script>"#;
        let levels = parse_oc_usage(body.as_bytes()).unwrap();
        assert_eq!(levels.len(), 3);
        for lv in &levels {
            assert_eq!(lv["reset_ts"].as_u64(), Some(0), "{lv}");
            assert_eq!(lv["percent"].as_f64(), Some(0.0));
        }
    }

    #[test]
    fn parse_oc_usage_missing_marker_errors() {
        assert!(parse_oc_usage(b"<html>no data</html>").is_err());
        // 页面只出现 `monthlyUsage: null` 占位、无真实用量对象 → Err
        let only_null = r#"<script>$R[24] = { balance: 0, monthlyUsage: null, rollingUsage: null, weeklyUsage: null }</script>"#;
        assert!(parse_oc_usage(only_null.as_bytes()).is_err());
        // grab_int：冒号后取十进制整数
        assert_eq!(grab_int("resetInSec: 12114,", "resetInSec"), Some(12114));
        assert_eq!(grab_int("x: abc", "x"), None);
    }

    #[test]
    fn parse_set_cookies_extracts_and_dedupes() {
        // Phase 53：从响应 Set-Cookie 头解析 (name,value) 对（去 Path/HttpOnly 属性，同名后者覆盖）
        use axum::http::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        h.append("set-cookie", HeaderValue::from_static("sessionid=abc; Path=/; HttpOnly"));
        h.append("set-cookie", HeaderValue::from_static("csrf=xyz; Path=/; SameSite=Lax"));
        assert_eq!(
            parse_set_cookies(&h),
            vec![("sessionid".into(), "abc".into()), ("csrf".into(), "xyz".into())]
        );

        // 同名覆盖：后者生效
        let mut h2 = HeaderMap::new();
        h2.append("set-cookie", HeaderValue::from_static("sessionid=old; Path=/"));
        h2.append("set-cookie", HeaderValue::from_static("sessionid=new; Path=/"));
        assert_eq!(parse_set_cookies(&h2), vec![("sessionid".into(), "new".into())]);

        // 无 `=` 的 Set-Cookie（非 Cookie）跳过
        let mut h3 = HeaderMap::new();
        h3.append("set-cookie", HeaderValue::from_static("HttpOnly"));
        assert!(parse_set_cookies(&h3).is_empty());

        // 值为空的 Set-Cookie = 上游在清除 Cookie（sessionid=）→ 跳过，勿把空值存进存档
        let mut h4 = HeaderMap::new();
        h4.append("set-cookie", HeaderValue::from_static("sessionid=; Expires=Thu, 01 Jan 1970; Path=/"));
        h4.append("set-cookie", HeaderValue::from_static("csrf=x; Path=/"));
        assert_eq!(parse_set_cookies(&h4), vec![("csrf".into(), "x".into())]);

        // 无 Set-Cookie → 空 Vec
        assert!(parse_set_cookies(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn merge_cookies_updates_by_key_keeps_others() {
        // Phase 53d：续期按 key 合并——只更新响应 Set-Cookie 里的 key，旧存档其它 key 原样保留
        // 旧存档：sessionid=abc + csrf=xyz；响应只回填 sessionid + 新增 theme
        let old = "sessionid=abc; csrf=xyz";
        let updates = vec![("sessionid".into(), "abc2".into()), ("theme".into(), "dark".into())];
        assert_eq!(merge_cookies(old, &updates), "sessionid=abc2; csrf=xyz; theme=dark");

        // 同名值不变 → 合并后 == 旧存档（调用方据此判定「相同，不续期」）
        assert_eq!(merge_cookies(old, &[("csrf".into(), "xyz".into())]), old);

        // 无更新（上游没回填 Set-Cookie）→ 空串
        assert_eq!(merge_cookies(old, &[]), "");

        // 旧存档为空 + 有更新 → 从零建立
        assert_eq!(
            merge_cookies("", &[("sessionid".into(), "n1".into())]),
            "sessionid=n1"
        );

        // parse_cookie_str：容忍空白、值含 `=` 取第一个（cookie value 不应含 `=`）
        assert_eq!(
            parse_cookie_str("sessionid=abc;  theme=dark=extra"),
            vec![("sessionid".into(), "abc".into()), ("theme".into(), "dark=extra".into())]
        );
        // 属性段（无 `=`）跳过
        assert_eq!(
            parse_cookie_str("a=1; HttpOnly; b=2"),
            vec![("a".into(), "1".into()), ("b".into(), "2".into())]
        );
    }

    #[test]
    fn renewal_detail_lists_per_key_comparison() {
        // Phase 53e：逐 key 对比明细——响应 Set-Cookie 的 key + 新 value + 旧 value 对比
        // 旧存档 sessionid=abc + region=cn；响应回填 sessionid(变) + theme(新增)；region 保留
        let detail = renewal_detail_lines(
            "sessionid=abc; region=cn",
            "sessionid=abc2; region=cn; theme=dark",
        );
        assert_eq!(
            detail,
            vec![
                "  region: cn（未变）".to_string(),
                "  sessionid: abc → abc2".to_string(),
                "  theme: (新增) → dark".to_string(),
            ]
        );
        // 旧存档为空 + 全新增
        let d2 = renewal_detail_lines("", "sessionid=n1; csrf=x");
        assert_eq!(
            d2,
            vec![
                "  csrf: (新增) → x".to_string(),
                "  sessionid: (新增) → n1".to_string(),
            ]
        );
        // 相同串 → 全「未变」
        let d3 = renewal_detail_lines("a=1; b=2", "a=1; b=2");
        assert!(d3.iter().all(|l| l.ends_with("（未变）")), "{d3:?}");
        assert_eq!(d3.len(), 2);
    }

    #[test]
    fn cookie_renewal_applies_only_changed() {
        // Phase 53：续期逻辑——非空且与存档不同 → 更新；空/相同 → 保留
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("claude-relay-cookie-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.json");
            let mut cfg = Config::default();
            let mut oc = oc_provider();
            oc.console_cookie = "sessionid=old".into();
            cfg.providers = vec![oc];
            Config::save(&path, &cfg).unwrap();
            let state = AppState {
                cfg: Arc::new(tokio::sync::RwLock::new(cfg.clone())),
                client: reqwest::Client::new(),
                debug: false,
                config_path: path.clone(),
                started: std::time::Instant::now(),
                usage: Arc::new(std::sync::Mutex::new(crate::config::UsageStats::default())),
                usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };

            // 新值不同 → 更新（返回 id）
            let changed = apply_cookie_renewals(&state, vec![("oc".into(), "sessionid=new".into())]).await;
            assert_eq!(changed, vec!["oc"]);
            assert_eq!(state.cfg.read().await.providers[0].console_cookie, "sessionid=new");
            let disk = Config::load(&path).unwrap();
            assert_eq!(disk.providers[0].console_cookie, "sessionid=new");

            // 值相同 → 不更新（返回空，零写盘）
            let changed = apply_cookie_renewals(&state, vec![("oc".into(), "sessionid=new".into())]).await;
            assert!(changed.is_empty());

            // 空串 → 不更新
            let changed = apply_cookie_renewals(&state, vec![("oc".into(), "  ".into())]).await;
            assert!(changed.is_empty());

            // 未知服务商 → 不更新
            let changed = apply_cookie_renewals(&state, vec![("nope".into(), "sessionid=x".into())]).await;
            assert!(changed.is_empty());

            std::fs::remove_dir_all(&dir).unwrap();
        });
    }

    #[test]
    fn parse_xf_coding_plan_filters_by_api_key() {
        // Phase 30：rows 里按 codingPlanAppCredentialDTO.apiKey 匹配服务商 api_key——只显示匹配行，
        // 且多个配置服务商（多个 key）→ 各查各的（这里模拟一个 key 只命中一行，另一行被过滤）
        let body = r#"{"code":0,"data":{"page":1,"rows":[
          {"appId":"mcc165a0","codingPlanAppCredentialDTO":{"apiKey":"XF_KEY"},
           "codingPlanUsageDTO":{"appId":"mcc165a0","channel":"astron-code-latest","dailyLimit":null,"dailyUsage":null,"packageLeft":13346.6,"packageLimit":18000,"packageUsage":4653.4,"rp5hLimit":1200,"rp5hUsage":22,"rpwLimit":9000,"rpwUsage":1940.4},
           "createBy":1517199,"createTime":"2026-07-14 10:00:14","expiresAt":"2026-08-14 10:00:36","name":"专业版-包月"},
          {"appId":"other","codingPlanAppCredentialDTO":{"apiKey":"OTHER_KEY"},
           "codingPlanUsageDTO":{"packageLeft":0,"packageLimit":0,"packageUsage":0},
           "name":"其他套餐"}
        ],"size":6,"total":2},"succeed":true,"message":"OK"}"#
            .as_bytes();
        let plans = parse_xf_coding_plan(body, "XF_KEY").unwrap();
        assert_eq!(plans.len(), 1, "只应返回 apiKey 匹配的行");
        let p0 = &plans[0];
        assert_eq!(p0["plan"], "专业版-包月");
        assert_eq!(p0["channel"], "astron-code-latest");
        assert_eq!(p0["expires_at"], "2026-08-14 10:00:36");
        assert!((p0["percent"].as_f64().unwrap() - 25.9).abs() < 0.01, "4653.4/18000 → 25.9%");
        assert_eq!(p0["package_left"], 13346.6);
        assert_eq!(p0["rp5h_usage"], 22.0);
        assert_eq!(p0["rpw_limit"], 9000.0);
        // 无匹配 key → Err（提示 apiKey 不匹配）
        let e = parse_xf_coding_plan(body, "NOPE_KEY").unwrap_err();
        assert!(e.contains("apiKey"), "got: {e}");
        // 响应缺 data.rows → Err
        assert!(parse_xf_coding_plan(br#"{"code":1}"#, "XF_KEY").is_err());
        // 非 JSON → Err
        assert!(parse_xf_coding_plan(b"not json", "XF_KEY").is_err());
    }

    #[test]
    fn masked_detection_uses_star_prefix() {
        // Phase 49：掩码判据 starts_with('*')——真实 cookie/key 里含 `*` 不再误判为掩码
        // （掩码输出必然以 `*` 开头，如 "****TH_12345"）
        assert!(is_masked("****TH_12345"));
        assert!(is_masked("**"));
        // 真实值含 `*`（如 session/token cookie）→ 不算掩码，应原样保存
        assert!(!is_masked("sessionid=TEST*auth123"));
        assert!(!is_masked("sk-abc*def123"));
        assert!(!is_masked(""));
    }

    #[test]
    fn usage_reset_clears_and_saves() {
        // Phase 35：POST /api/usage/reset——内存用量清空 + 立即落盘 `*-usage.json`（重启也不回来）
        use std::sync::Arc;
        use crate::config::{AppState, UsageStats};

        let stem = format!("relay_usage_reset_{}", std::process::id());
        let cfg_path = std::env::temp_dir().join(format!("{stem}.json"));
        let usage_file = std::env::temp_dir().join(format!("{stem}-usage.json"));
        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&usage_file);

        let cfg = Config { server: Default::default(), providers: vec![ProviderConfig {
            id: "ds".into(), name: String::new(), base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(), auth: "bearer".into(), models: vec![],
            strip_beta: false, rewrite_model: true, protocol: "anthropic".into(),
            console_cookie: String::new(), console_usage_url: String::new(), hide_usage: false,
        }] };
        let mut usage = UsageStats::default();
        let pu = usage.providers.entry("ds".into()).or_default();
        pu.requests = 5; pu.input_tokens = 100; pu.output_tokens = 50;
        let state = Arc::new(AppState {
            cfg: Arc::new(tokio::sync::RwLock::new(cfg)),
            client: reqwest::Client::new(),
            debug: false,
            config_path: cfg_path.clone(),
            started: std::time::Instant::now(),
            usage: Arc::new(std::sync::Mutex::new(usage)),
            usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let resp = usage_reset(axum::extract::State(state.clone())).await;
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
        });
        // 内存清空
        assert!(state.usage.lock().unwrap().providers.is_empty());
        // 落盘文件被清空（读回为空统计）
        let ondisk: UsageStats =
            serde_json::from_str(&std::fs::read_to_string(&usage_file).unwrap()).unwrap();
        assert!(ondisk.providers.is_empty());

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&usage_file);
        let _ = std::fs::remove_file(usage_file.with_extension("tmp"));
    }

    #[test]
    fn write_claude_settings_merges_env_and_top_model() {
        // Phase 42：同步核心（async handler 与托盘线程共用）——合并 env 6 键 + 顶层 model，
        // 其余内容（FOO/theme）全部保留；缺失文件→新建。
        let dir = std::env::temp_dir().join(format!("relay_settings_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"env":{"FOO":"bar","ANTHROPIC_MODEL":"old"},"theme":"dark"}"#).unwrap();

        let (base_url, model) =
            write_claude_settings("127.0.0.1", 8787, "claude-relay-model[200k]", &path).unwrap();
        assert_eq!(base_url, "http://127.0.0.1:8787");
        assert_eq!(model, "claude-relay-model[200k]");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["theme"], "dark", "非 env 键保留");
        assert_eq!(doc["env"]["FOO"], "bar", "既有 env 键保留");
        assert_eq!(doc["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(doc["env"]["ANTHROPIC_API_KEY"], "relay-dummy");
        assert_eq!(doc["env"]["ANTHROPIC_MODEL"], "claude-relay-model[200k]");
        assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-relay-model[200k]");
        assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-relay-model[200k]");
        assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-relay-model[200k]");
        assert_eq!(doc["model"], "claude-relay-model[200k]", "顶层 model 也写哨兵");

        // 缺失文件 → 新建（空 env + 顶层 model）
        let path2 = dir.join("fresh.json");
        let (_, m2) = write_claude_settings("127.0.0.1", 8787, "claude-relay-model[1m]", &path2).unwrap();
        assert_eq!(m2, "claude-relay-model[1m]");
        let doc2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path2).unwrap()).unwrap();
        assert_eq!(doc2["env"]["ANTHROPIC_MODEL"], "claude-relay-model[1m]");
        assert_eq!(doc2["model"], "claude-relay-model[1m]");

        // 非法 JSON → Err
        let path3 = dir.join("bad.json");
        std::fs::write(&path3, "not json{").unwrap();
        assert!(write_claude_settings("127.0.0.1", 8787, "m", &path3).is_err());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
        let _ = std::fs::remove_file(&path3);
        let _ = std::fs::remove_dir(&dir);
    }
}
