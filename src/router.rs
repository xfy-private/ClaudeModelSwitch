//! 模型名解析：`{服务商id}{分隔符}{模型}` → (服务商索引, 真实模型名)。
//!
//! 模型名本身常含 `-`（如 `deepseek-chat`、`gpt-4o`），不能按第一个分隔符切分。
//! 因此按**已配置服务商 id 做最长前缀匹配**（决策 D12）：
//!   - `ds-deepseek-chat` → id `ds` 命中，真实模型 `deepseek-chat`
//!   - `dsx-deepseek-r1` → `ds` 也命中但短，`dsx` 更长 → 取 `dsx`（若已配置）

use crate::config::{Config, ProviderConfig};

/// Phase 22：CC 发来 `claude-relay-model[上下文]` 哨兵时，用配置里「当前使用模型」替换。
pub const RELAY_MODEL: &str = "claude-relay-model";

/// 剥离 CC 上下文后缀 `[1m]`/`[200k]`/`[128k]`：返回 (基础名, 后缀内容)。
/// `ds-v4-flash[1m]` → (`ds-v4-flash`, Some("1m"))；无后缀 → (原串, None)。
pub fn split_ctx_suffix(model: &str) -> (&str, Option<&str>) {
    if let Some(open) = model.rfind('[') {
        if open > 0 && model.ends_with(']') {
            return (&model[..open], Some(&model[open + 1..model.len() - 1]));
        }
    }
    (model, None)
}

#[derive(Debug)]
pub struct Resolved {
    /// 命中服务商在 providers 里的下标。
    pub provider: usize,
    /// 去掉前缀后的真实模型名，发给上游。
    pub real_model: String,
    /// 带前缀的别名（原样）。
    pub alias: String,
    /// Phase 60：命中模型行的上游协议覆盖（None = 跟随服务商级 protocol）。
    pub protocol: Option<String>,
}

pub fn resolve(providers: &[ProviderConfig], model: &str, sep: &str) -> Result<Resolved, String> {
    let sep = if sep.is_empty() { "-" } else { sep };
    // Phase 22：先剥 CC 上下文后缀（`[1m]`/`[200k]`…），再做前缀匹配。
    // `Resolved.alias` 保留原始完整模型串（含后缀），SSE 回写时原样回显。
    let (base, _ctx) = split_ctx_suffix(model);

    let mut best: Option<(usize, &ProviderConfig)> = None;
    for (i, p) in providers.iter().enumerate() {
        let prefix = format!("{}{}", p.id, sep);
        if let Some(rest) = base.strip_prefix(&prefix) {
            if rest.is_empty() {
                continue; // 形如 `ds-` 无效
            }
            let better = best
                .as_ref()
                .map_or(true, |(_, bp): &(usize, &ProviderConfig)| p.id.len() > bp.id.len());
            if better {
                best = Some((i, p));
            }
        }
    }

    match best {
        Some((i, p)) => {
            let prefix = format!("{}{}", p.id, sep);
            let suffix = &base[prefix.len()..];
            // 后缀先查该服务商配置的别名 → 命中则发真实模型名；否则后缀直接当真实模型名（向后兼容）。
            let entry = p.models.iter().find(|m| m.alias == suffix);
            let real_model = entry
                .map(|m| m.real.clone())
                .unwrap_or_else(|| suffix.to_string());
            // Phase 60：命中别名行时带出行级协议覆盖（未命中 = 跟随服务商级）
            let protocol = entry.and_then(|m| m.protocol.clone());
            Ok(Resolved {
                provider: i,
                real_model,
                alias: model.to_string(),
                protocol,
            })
        }
        None => Err(format!(
            "unknown model '{model}': no provider prefix matches. Available: {}",
            available_aliases(providers, sep).join(", ")
        )),
    }
}

/// Phase 22：按请求模型解析——`claude-relay-model[上下文]` 哨兵 → 用配置里
/// 「当前使用模型」（`server.default_model`，空则回退第一个别名）替换后解析；
/// 其它模型先剥 `[ctx]` 后缀再正常前缀匹配（具体模型优先）。
pub fn resolve_for_request(cfg: &Config, model: &str) -> Result<Resolved, String> {
    let (base, _ctx) = split_ctx_suffix(model);
    if base == RELAY_MODEL {
        let cur = cfg.default_model();
        if cur.is_empty() {
            return Err(format!(
                "model '{model}': {RELAY_MODEL} 哨兵但未配置「当前使用模型」（无服务商/模型）"
            ));
        }
        let mut r = resolve(&cfg.providers, &cur, &cfg.server.separator)?;
        // Phase 22：SSE 回写「当前使用模型」完整别名 + 上下文后缀（如 xf-glm-5.1[200k]），
        // 让 CC 看到实际用的模型而非哨兵占位名 claude-relay-model。
        r.alias = format!("{}{}", cur, cfg.ctx_suffix_of(&cur));
        return Ok(r);
    }
    resolve(&cfg.providers, model, &cfg.server.separator)
}

/// 从配置推导全部可选别名（供 `GET /v1/models` 与错误提示使用）。
pub fn available_aliases(providers: &[ProviderConfig], sep: &str) -> Vec<String> {
    let sep = if sep.is_empty() { "-" } else { sep };
    let mut v: Vec<String> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(|m| format!("{}{}{}", p.id, sep, m.alias)))
        .collect();
    if v.is_empty() {
        v = providers
            .iter()
            .map(|p| format!("{}{}<model>", p.id, sep))
            .collect();
    }
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn p(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
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

    #[test]
    fn resolves_prefixed() {
        let ps = vec![p("ds"), p("kb")];
        let r = resolve(&ps, "ds-deepseek-chat", "-").unwrap();
        assert_eq!(r.provider, 0);
        assert_eq!(r.real_model, "deepseek-chat");
        assert_eq!(r.alias, "ds-deepseek-chat");
    }

    #[test]
    fn resolve_carries_model_protocol() {
        // Phase 60：命中别名行 → 带出行级 protocol 覆盖；未命中/直接真实名 → None
        let mut pds = p("ds");
        pds.models = vec![
            crate::config::ModelEntry {
                alias: "flash".into(),
                real: "deepseek-chat".into(),
                context: 128000,
                protocol: Some("openai".into()),
            },
            crate::config::ModelEntry::named("direct-model"),
        ];
        let ps = vec![pds];
        let r = resolve(&ps, "ds-flash", "-").unwrap();
        assert_eq!(r.real_model, "deepseek-chat");
        assert_eq!(r.protocol.as_deref(), Some("openai"));
        let r2 = resolve(&ps, "ds-direct-model", "-").unwrap();
        assert_eq!(r2.real_model, "direct-model");
        assert_eq!(r2.protocol, None);
        // 未配置后缀直接当真实模型名（向后兼容）→ 无协议覆盖
        let r3 = resolve(&ps, "ds-unknown-thing", "-").unwrap();
        assert_eq!(r3.real_model, "unknown-thing");
        assert_eq!(r3.protocol, None);
    }

    #[test]
    fn longest_prefix_wins() {
        let ps = vec![p("ds"), p("dsx")];
        let r = resolve(&ps, "dsx-deepseek-r1", "-").unwrap();
        assert_eq!(r.provider, 1);
        assert_eq!(r.real_model, "deepseek-r1");
    }

    #[test]
    fn no_match_errors() {
        let ps = vec![p("ds")];
        let e = resolve(&ps, "unknown-x", "-").unwrap_err();
        assert!(e.contains("no provider prefix matches"));
    }

    #[test]
    fn bare_prefix_rejected() {
        let ps = vec![p("ds")];
        assert!(resolve(&ps, "ds-", "-").is_err());
    }

    #[test]
    fn colon_sep() {
        let ps = vec![p("ds")];
        let r = resolve(&ps, "ds:deepseek-chat", ":").unwrap();
        assert_eq!(r.real_model, "deepseek-chat");
    }

    #[test]
    fn alias_maps_to_real() {
        use crate::config::ModelEntry;
        let mut ps = vec![p("ds")];
        ps[0].models = vec![
            ModelEntry::named("deepseek-chat"),
            ModelEntry { alias: "flash".into(), real: "deepseek-v4-flash".into(), context: crate::config::DEFAULT_CONTEXT, protocol: None },
        ];
        // 简写别名 → 真实模型
        let r = resolve(&ps, "ds-flash", "-").unwrap();
        assert_eq!(r.real_model, "deepseek-v4-flash");
        assert_eq!(r.alias, "ds-flash");
        // 真实模型名也仍可直接用
        let r = resolve(&ps, "ds-deepseek-chat", "-").unwrap();
        assert_eq!(r.real_model, "deepseek-chat");
    }

    #[test]
    fn aliases_listed_with_alias_names() {
        use crate::config::ModelEntry;
        let mut ps = vec![p("ds")];
        ps[0].models = vec![
            ModelEntry { alias: "flash".into(), real: "deepseek-v4-flash".into(), context: crate::config::DEFAULT_CONTEXT, protocol: None },
            ModelEntry::named("deepseek-chat"),
        ];
        let al = available_aliases(&ps, "-");
        assert!(al.contains(&"ds-flash".to_string()));
        assert!(al.contains(&"ds-deepseek-chat".to_string()));
        assert!(!al.contains(&"ds-deepseek-v4-flash".to_string()));
    }

    // ---- Phase 22：上下文后缀剥离 + 哨兵路由 ----
    use crate::config::{Config, ModelEntry};

    fn cfg(dm: &str) -> Config {
        let mut c = Config::default();
        c.server.default_model = dm.into();
        c.providers = vec![ProviderConfig {
            id: "ds".into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![
                ModelEntry {
                    alias: "v4-flash".into(),
                    real: "deepseek-v4-flash".into(),
                    context: crate::config::DEFAULT_CONTEXT,
                    protocol: None,
                },
                ModelEntry::named("deepseek-chat"),
            ],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }];
        c
    }

    #[test]
    fn split_ctx_suffix_parses() {
        assert_eq!(split_ctx_suffix("ds-v4-flash[1m]"), ("ds-v4-flash", Some("1m")));
        assert_eq!(
            split_ctx_suffix("claude-relay-model[200k]"),
            ("claude-relay-model", Some("200k"))
        );
        assert_eq!(split_ctx_suffix("ds-v4-flash"), ("ds-v4-flash", None));
        assert_eq!(split_ctx_suffix("a[1m"), ("a[1m", None)); // 缺 `]` 不剥
        assert_eq!(split_ctx_suffix(""), ("", None));
    }

    #[test]
    fn resolve_strips_ctx_suffix() {
        let c = cfg("ds-v4-flash");
        let r = resolve_for_request(&c, "ds-v4-flash[1m]").unwrap();
        assert_eq!(r.real_model, "deepseek-v4-flash");
        assert_eq!(r.alias, "ds-v4-flash[1m]"); // 原始串（含后缀）回显
    }

    #[test]
    fn sentinel_resolves_to_current_model() {
        let c = cfg("ds-v4-flash");
        let r = resolve_for_request(&c, "claude-relay-model[1m]").unwrap();
        assert_eq!(r.real_model, "deepseek-v4-flash");
        assert_eq!(r.alias, "ds-v4-flash[200k]"); // 回写当前使用模型完整别名 + 上下文后缀
        assert_eq!(r.provider, 0);
    }

    #[test]
    fn bare_sentinel_uses_fallback_current() {
        // 未配置 default_model → 回退第一个服务商第一个模型别名
        let c = cfg("");
        let r = resolve_for_request(&c, "claude-relay-model").unwrap();
        assert_eq!(r.real_model, "deepseek-v4-flash");
        assert_eq!(r.alias, "ds-v4-flash[200k]");
    }

    #[test]
    fn sentinel_with_no_providers_errors() {
        let c = Config::default(); // 空服务商
        let e = resolve_for_request(&c, "claude-relay-model").unwrap_err();
        assert!(e.contains("哨兵"), "got: {e}");
    }

    #[test]
    fn specific_model_beats_sentinel() {
        let c = cfg("ds-v4-flash");
        let r = resolve_for_request(&c, "ds-deepseek-chat").unwrap();
        assert_eq!(r.real_model, "deepseek-chat"); // 指定模型优先，不走当前模型
    }
}
