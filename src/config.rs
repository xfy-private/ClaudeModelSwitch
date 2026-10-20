//! 配置结构、JSON 读写、校验与共享状态。
//!
//! 配置源为 `config.json`，运行期由 `Arc<RwLock<Config>>` 共享，Web UI 或外部改文件后
//! 热重载（下一请求读到新配置即生效，无需重启）。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_SEP: &str = "-";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// 服务商 id 与模型名之间的分隔符，默认 `-`，如 `ds-deepseek-chat`。
    pub separator: String,
    /// **当前使用模型**（完整别名，如 `ds-v4-flash`；Phase 22 语义由「默认」改为「当前使用」）。
    /// 空则回退第一个服务商第一个模型别名。CC 发 `claude-relay-model[上下文]` 哨兵时中继用它替换；
    /// 另供 `--print-default-model`、copyEnv、更新 claude 配置/环境变量生成哨兵值。
    pub default_model: String,
    /// 开机自启（Windows，Phase 19）：注册到 HKCU Run 键，登录后自动运行（无窗口）。
    /// 保存即生效（webui config_put 写注册表）；启动时按配置同步一次。
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_start: bool,
    /// Phase 54：用量时间统计保留窗口——小时桶保留 `usage_hours` 小时、天桶保留 `usage_days` 天
    ///（默认 48 / 14，可配置），超期自动剪除（内存/磁盘有界）。
    #[serde(default = "usage_hours_default", skip_serializing_if = "is_usage_hours_default")]
    pub usage_hours: usize,
    #[serde(default = "usage_days_default", skip_serializing_if = "is_usage_days_default")]
    pub usage_days: usize,
    /// 桌面卡片数据刷新间隔（秒，1..=60，默认 3）：数据任务每轮现读，改配置即生效。
    #[serde(default = "card_refresh_secs_default", skip_serializing_if = "is_card_refresh_secs_default")]
    pub card_refresh_secs: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT,
            separator: DEFAULT_SEP.into(),
            default_model: String::new(),
            auto_start: false,
            usage_hours: usage_hours_default(),
            usage_days: usage_days_default(),
            card_refresh_secs: card_refresh_secs_default(),
        }
    }
}

/// 模型条目：`alias` 是 CC 里输入的前缀简写（如 `flash`，完整输入 `ds-flash`），
/// `real` 是服务商真实模型名（如 `deepseek-v4-flash`），`context` 是上下文窗口大小
/// （tokens，元数据，仅 UI 展示；默认 200k）。
///
/// 向后兼容的 JSON 表示：字符串 `"mock-chat"` 等价于
/// `{ "alias": "mock-chat", "real": "mock-chat", "context": 200000 }`；
/// 序列化时 alias==real 且 context==默认且无协议覆盖写成字符串，否则写成对象
/// （context 非默认 / protocol 非空才写出）。
pub const DEFAULT_CONTEXT: u64 = 200_000;

/// 上下文窗口 tokens → CC 模型后缀（`[1m]`/`[200k]`/`[128k]`…，Phase 22）。
/// 1e6 整倍 → `[Nm]`；千整倍 → `[Nk]`；否则四舍五入到千。0 → 空串。
fn fmt_ctx_suffix(ctx: u64) -> String {
    if ctx == 0 {
        return String::new();
    }
    if ctx % 1_000_000 == 0 {
        return format!("[{}m]", ctx / 1_000_000);
    }
    if ctx % 1_000 == 0 {
        return format!("[{}k]", ctx / 1_000);
    }
    format!("[{}k]", (ctx as f64 / 1000.0).round() as u64)
}

#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub alias: String,
    pub real: String,
    pub context: u64,
    /// Phase 60：模型行级上游协议覆盖——`None`/空 = 跟随服务商级 `ProviderConfig.protocol`。
    /// 让同一服务商下不同模型走不同 API 族（URL 仍按对应协议模板由 base_url 拼出）。
    pub protocol: Option<String>,
}

impl ModelEntry {
    pub fn named(name: impl Into<String>) -> Self {
        let n = name.into();
        ModelEntry { alias: n.clone(), real: n, context: DEFAULT_CONTEXT, protocol: None }
    }
}

fn default_context() -> u64 {
    DEFAULT_CONTEXT
}

impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Obj {
                alias: String,
                real: String,
                #[serde(default = "default_context")]
                context: u64,
                #[serde(default)]
                protocol: Option<String>,
            },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Name(n) => ModelEntry::named(n),
            Raw::Obj { alias, real, context, protocol } => {
                if alias.trim().is_empty() {
                    return Err(serde::de::Error::custom("model alias must not be empty"));
                }
                if real.trim().is_empty() {
                    return Err(serde::de::Error::custom("model real must not be empty"));
                }
                // Phase 60：空串协议视为未指定（跟随服务商级）
                let protocol = protocol.filter(|s| !s.trim().is_empty());
                ModelEntry { alias, real, context, protocol }
            }
        })
    }
}

impl Serialize for ModelEntry {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.alias == self.real && self.context == DEFAULT_CONTEXT && self.protocol.is_none() {
            s.serialize_str(&self.alias)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("ModelEntry", 4)?;
            st.serialize_field("alias", &self.alias)?;
            st.serialize_field("real", &self.real)?;
            if self.context != DEFAULT_CONTEXT {
                st.serialize_field("context", &self.context)?;
            }
            if let Some(p) = self.protocol.as_deref().filter(|s| !s.trim().is_empty()) {
                st.serialize_field("protocol", p)?;
            }
            st.end()
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProviderConfig {
    /// 服务商简写前缀，全局唯一，如 `ds`、`kb`、`anthropic`。
    pub id: String,
    /// 显示名（可选）。
    #[serde(default)]
    pub name: String,
    /// Anthropic 兼容端点根地址，中继拼 `/v1/messages` 转发。如 `https://api.deepseek.com/anthropic`。
    pub base_url: String,
    pub api_key: String,
    /// 上游认证头方式：`bearer`（Authorization: Bearer，默认）或 `x-api-key`。
    #[serde(default = "default_auth_mode")]
    pub auth: String,
    /// 模型列表（别名→真实模型+上下文），仅用于 UI/别名展示与 `GET /v1/models`。
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// 是否剥离 CC 的 `anthropic-beta` 头（默认透传，D10）。
    #[serde(default)]
    pub strip_beta: bool,
    /// 是否把响应流里 `message.model` 改回带前缀的别名（D7，默认开）。
    #[serde(default = "default_true")]
    pub rewrite_model: bool,
    /// Phase 36：上游协议——`anthropic`（默认，纯透传）或 `openai`（请求/响应翻译为
    /// OpenAI Chat Completions，解锁 OpenRouter/Qwen 等仅 OpenAI 协议服务商）。
    #[serde(default, skip_serializing_if = "is_anthropic")]
    pub protocol: String,
    /// Phase 28/30：Coding Plan 用量查询的控制台 Cookie——火山方舟 `console.volcengine.com` /
    /// 讯飞星辰 `maas.xfyun.cn`。手动粘贴或「导入 Cookie」按钮写入（查询时也兜底自动读浏览器对应域）。
    /// 掩码展示，PUT 复用（同 D13）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_cookie: String,
    /// Phase 28/30：可选用量查询 URL（默认按服务商推导 console 端点——火山方舟按区域 / 讯飞星辰官方列表）。
    /// 测试/自建时可直接指定（如 e2e 指向本地 mock）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_usage_url: String,
    /// Phase 57：隐藏该服务商的余额/用量展示（Web UI 使用情况卡片/余额/Coding Plan/
    /// 模型详情大屏/桌面卡片均不显示；统计照常记录，开关随时可逆）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_usage: bool,
}

impl ProviderConfig {
    /// Phase 58：是否 **OpenCode Go 网关**服务商（转发时需携带 `x-opencode-session` 会话头 +
    /// 专属 User-Agent，否则上游 400 `MissingSessionID`）。
    ///
    /// 判定口径与 webui::console_kind 的 Oc 分支一致：`base_url` 含 `opencode`
    /// （真实网关如 `https://opencode.ai/zen/go/v1`），或 `console_usage_url` 含
    /// `/workspace/`/`opencode`（OpenCode /go 页地址，供 e2e 用本地 mock 触发）。
    pub fn is_opencode(&self) -> bool {
        let base = self.base_url.to_ascii_lowercase();
        if base.contains("opencode") {
            return true;
        }
        let u = self.console_usage_url.to_ascii_lowercase();
        u.contains("opencode") || u.contains("/workspace/")
    }
}
fn default_auth_mode() -> String {
    "bearer".into()
}
fn default_true() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_anthropic(p: &str) -> bool {
    p == "anthropic"
}
fn usage_hours_default() -> usize {
    48
}
fn usage_days_default() -> usize {
    14
}
fn is_usage_hours_default(v: &usize) -> bool {
    *v == usage_hours_default()
}
fn card_refresh_secs_default() -> u64 {
    3
}
fn is_card_refresh_secs_default(v: &u64) -> bool {
    *v == card_refresh_secs_default()
}
fn is_usage_days_default(v: &usize) -> bool {
    *v == usage_days_default()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Phase 21：`skip_serializing_if` 让空服务商不写出——首次运行生成的默认配置
    /// 恰为 `{"server":{host,port,separator,default_model}}`（用户可去配置页添加服务商）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderConfig>,
}

/// Phase 26：调用量/用量统计（自启动以来，按服务商+模型累加）。
/// 纯透传也能统计——从**自己的转发流**里取 SSE usage / count_tokens。
/// `std::sync::Mutex` 短锁即可（relay 单线程 runtime + 托盘只读，锁内无 await）。
///
/// Phase 27：**持久化**——`save()` 原子写 `*-usage.json`（同配置目录，重启不清零），
/// `load()` 缺失/损坏 → 空统计（不崩）。
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    /// key = 服务商 id。
    pub providers: HashMap<String, ProviderUsage>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// key = 模型（完整别名剥 `[ctx]` 后缀，如 `xf-glm-5.1`）。
    pub by_model: HashMap<String, ModelUsage>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct ModelUsage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Phase 54：时间桶统计（每小时/每日，有界，`prune` 剪除）。
    #[serde(default)]
    pub time: TimeUsage,
}

/// Phase 54：时间桶——每小时/每日各一条；`ts` = 桶起始 unix 秒
///（小时 = UTC 整点；天 = 本地日零点，用 Windows 时区偏置换算）。有界，`prune` 超期剪除。
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TimeBucket {
    pub ts: i64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TimeUsage {
    pub hours: Vec<TimeBucket>,
    pub days: Vec<TimeBucket>,
}

impl UsageStats {
    /// 原子落盘：先写 `<path>.tmp` 再 rename 覆盖。原子性防「/api/restart 新旧进程
    /// 交接瞬间两进程并发写坏文件」（最后一个写者胜，文件始终完整）。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize usage: {e}"))?;
        write_file_atomic(path, &s).map_err(|e| format!("write usage: {e}"))
    }

    /// 读取持久化用量；文件缺失/损坏 → 空统计（启动不崩，仅告警）。
    pub fn load(path: &Path) -> UsageStats {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("[warn] 用量文件损坏，重新统计: {e}");
                    UsageStats::default()
                }
            },
            Err(_) => UsageStats::default(),
        }
    }

    /// Phase 54：记录一次用量——累加累计值（请求数/输入/输出）+ upsert 小时/天时间桶。
    /// 小时桶 ts = `now - now % 3600`（UTC 整点）；天桶 ts = 本地日零点（Windows 时区偏置）。
    /// 桶内 `find` 命中则叠加、否则 push（桶数 ≤ usage_hours/usage_days，线性扫描足够）。
    pub fn record(
        &mut self,
        provider: &str,
        model: &str,
        requests: u64,
        input: u64,
        output: u64,
        now: i64,
    ) {
        let pu = self.providers.entry(provider.to_string()).or_default();
        pu.requests += requests;
        pu.input_tokens += input;
        pu.output_tokens += output;
        let mu = pu.by_model.entry(model.to_string()).or_default();
        mu.requests += requests;
        mu.input_tokens += input;
        mu.output_tokens += output;
        mu.time.add(now, requests, input, output);
    }

    /// Phase 54：剪除超期时间桶（保留窗口可配；`unix_now()` 作基准，内存/磁盘有界）。
    pub fn prune(&mut self, hours: usize, days: usize) {
        let now = unix_now();
        let h_cut = now - (hours as i64) * 3600;
        let d_cut = now - (days as i64) * 86400;
        for pu in self.providers.values_mut() {
            for mu in pu.by_model.values_mut() {
                mu.time.hours.retain(|b| b.ts >= h_cut);
                mu.time.days.retain(|b| b.ts >= d_cut);
            }
        }
    }
}

impl TimeUsage {
    /// 记录到时间桶：upsert 小时桶与天桶（存在则叠加，否则追加）。
    fn add(&mut self, now: i64, requests: u64, input: u64, output: u64) {
        upsert_bucket(&mut self.hours, now - now.rem_euclid(3600), requests, input, output);
        upsert_bucket(&mut self.days, local_day_start_of(now), requests, input, output);
    }
}

fn upsert_bucket(
    buckets: &mut Vec<TimeBucket>,
    ts: i64,
    requests: u64,
    input: u64,
    output: u64,
) {
    match buckets.iter_mut().find(|b| b.ts == ts) {
        Some(b) => {
            b.requests += requests;
            b.input_tokens += input;
            b.output_tokens += output;
        }
        None => buckets.push(TimeBucket {
            ts,
            requests,
            input_tokens: input,
            output_tokens: output,
        }),
    }
}

/// Phase 54：当前 unix 秒（`record`/`prune` 时间基准）。
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 本地日零点 unix 秒——用 Windows 时区偏置换算（OnceLock 缓存一次）。非 Windows 回退 0（UTC）。
fn local_day_start_of(now: i64) -> i64 {
    day_start_with_offset(now, local_utc_offset_secs())
}

/// 纯函数：给定时区偏置（秒）算本地日零点 unix 秒（Phase 54 天桶；可单测）。
/// pub(crate) 供 widget.rs 桌面卡片算「今日」用量。
pub(crate) fn day_start_with_offset(now: i64, offset: i64) -> i64 {
    now - (now + offset).rem_euclid(86400)
}

/// 本地时区相对 UTC 的偏置（秒）。Windows 用 `GetTimeZoneInformation` 的 `Bias`（分钟）：
/// UTC = local + Bias ⇒ local − UTC = −Bias 分钟。OnceLock 缓存一次；非 Windows 回退 0。
/// pub(crate) 供 widget.rs 桌面卡片算「今日」用量。
pub(crate) fn local_utc_offset_secs() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        #[cfg(windows)]
        {
            let mut tzi: windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION =
                unsafe { std::mem::zeroed() };
            let r = unsafe { windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut tzi) };
            // TIME_ZONE_ID_INVALID = 0xFFFFFFFF → 查询失败回退 0（UTC）
            if r != windows_sys::Win32::System::Time::TIME_ZONE_ID_INVALID {
                return -(tzi.Bias as i64) * 60;
            }
        }
        0
    })
}

/// 运行期共享状态：配置 + 复用的 HTTP 客户端 + 调试开关 + 配置文件路径/启动时刻。
///
/// Phase 18：不再有 `paused` 停止标志——托盘「停止」由 main 的 run_server 循环
/// 直接释放监听端口（停掉整个服务），状态经 `tokio::sync::watch` 承载，不在此结构里。

pub struct AppState {
    pub cfg: Arc<RwLock<Config>>,
    pub client: reqwest::Client,
    pub debug: bool,
    /// 配置文件路径（Web UI 保存/热重载用）。
    pub config_path: PathBuf,
    /// 启动时刻（/api/status 上报 uptime）。
    pub started: std::time::Instant,
    /// Phase 26：调用量/用量统计（每服务商请求数 + 输入/输出 tokens，自启动以来）。
    pub usage: Arc<std::sync::Mutex<UsageStats>>,
    /// Phase 27：用量脏标记——累计后置位，周期落盘任务读到才写盘（空闲零 IO）。
    pub usage_dirty: Arc<std::sync::atomic::AtomicBool>,
}

/// Phase 59：健壮的原子写盘（所有落盘点统一入口）——先写 `<path>.tmp` 再 rename 覆盖
/// （写一半崩溃/断电不会损坏原文件；此前 config.json 是直写，一并修复），并对
/// 跨机拷贝/新环境常见的两类失败自愈：
///   ① 目标/tmp 带**只读属性**（U盘/网盘/压缩包拷出常见，表现为 os error 5）→ 自动解除后重试；
///   ② 杀毒/同步盘瞬时锁定 → 短退避重试（4 次，最多约 600ms）。
/// 父目录不存在则顺带创建（`~/.claude/settings.json` 首次写入、新装机器）。最终仍失败
/// （如目录 ACL 无写权限）→ os error 5 的错误信息附排查指引，UI 原样展示给用户。
pub(crate) fn write_file_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(dir); // 已存在则 no-op；失败由后续写报错体现
        }
    }
    let tmp = path.with_extension("tmp");
    let mut last: Option<std::io::Error> = None;
    let mut healed = false;
    for attempt in 0..4u32 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * attempt as u64));
        }
        healed |= clear_readonly(&tmp);
        healed |= clear_readonly(path);
        match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path)) {
            Ok(()) => {
                if healed {
                    eprintln!("[info] {} 带只读属性，已自动解除后写回", path.display());
                }
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    let e = last.expect("至少尝试一次写盘");
    let mut msg = format!("{e}");
    if e.raw_os_error() == Some(5) {
        msg.push_str(
            "；排查：目标目录需有写权限（勿放 Program Files 等受保护目录，可移到用户目录），或文件被杀毒软件/同步盘锁定",
        );
    }
    Err(msg)
}

/// 尽力解除文件只读属性；有解除动作返回 true。std `Permissions` 在 Windows 上映射
/// FILE_ATTRIBUTE_READONLY、Unix 上映射写权限位——无需 windows-sys，跨平台一致。
/// 不存在/查询失败一律返回 false（由后续写报错体现）。
fn clear_readonly(path: &Path) -> bool {
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    let mut perms = md.permissions();
    if !perms.readonly() {
        return false;
    }
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).is_ok()
}

impl Config {
    /// 读取配置；文件不存在则写入默认配置并返回。
    pub fn load(path: &Path) -> Result<Config, String> {
        if !path.exists() {
            let cfg = Config::default();
            Self::save(path, &cfg)?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read config: {e}"))?;
        let cfg: Config =
            serde_json::from_str(&raw).map_err(|e| format!("parse config: {e}"))?;
        Ok(cfg)
    }

    pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
        let s = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize: {e}"))?;
        write_file_atomic(path, &s).map_err(|e| format!("write config: {e}"))
    }

    /// 校验配置。Err = 致命问题（拒绝启动/保存），返回的 Vec = 非致命告警。
    pub fn validate(&self) -> Result<Vec<String>, String> {
        if !(1..=65535).contains(&self.server.port) {
            return Err(format!("invalid server.port {}", self.server.port));
        }
        if self.server.separator.is_empty() {
            return Err("server.separator must not be empty".into());
        }
        if !(1..=60).contains(&self.server.card_refresh_secs) {
            return Err(format!(
                "server.card_refresh_secs must be 1..=60, got {}",
                self.server.card_refresh_secs
            ));
        }
        // Phase 21：服务商允许为空（首次运行/全部删除后合法，去配置页添加即可）；
        // 空服务商时 /v1/messages 由 router::resolve 返回 400，不阻断启动。
        let mut seen = HashSet::new();
        for p in &self.providers {
            if p.id.is_empty() {
                return Err("provider id must not be empty".into());
            }
            if !seen.insert(&p.id) {
                return Err(format!("duplicate provider id '{}'", p.id));
            }
            let ok_url = p.base_url.starts_with("http://") || p.base_url.starts_with("https://");
            if p.base_url.is_empty() || !ok_url {
                return Err(format!(
                    "provider '{}': base_url must start with http:// or https://",
                    p.id
                ));
            }
            if p.api_key.is_empty() {
                return Err(format!("provider '{}': api_key is empty", p.id));
            }
        }
        // 前缀冲突告警（一个 id 是另一个的前缀会加重 `-` 分隔歧义，D12）
        let mut warnings = Vec::new();
        for a in &self.providers {
            for b in &self.providers {
                if a.id != b.id && b.id.starts_with(&a.id) {
                    warnings.push(format!(
                        "provider '{}' id is a prefix of '{}' — 建议避免（前缀匹配可能歧义）",
                        a.id, b.id
                    ));
                }
            }
        }
        Ok(warnings)
    }

    /// 返回隐藏 api_key 后的副本（用于 API 展示）。
    /// Phase 53c：`console_cookie` **明文**展示（用户要求便于调试/核对，PUT 直接存明文），
    /// api_key 仍按 D13 掩码。
    pub fn masked(&self) -> Config {
        let mut c = self.clone();
        for p in &mut c.providers {
            if p.api_key.len() > 4 {
                let stars = "*".repeat(p.api_key.len() - 4);
                p.api_key = format!("{stars}{}", &p.api_key[p.api_key.len() - 4..]);
            } else {
                p.api_key = "*".repeat(p.api_key.len().max(1));
            }
            // console_cookie 明文保留（Phase 53c）
        }
        c
    }

    /// 默认模型完整别名：优先 `server.default_model`，否则回退第一个服务商第一个模型别名。
    pub fn default_model(&self) -> String {
        if !self.server.default_model.trim().is_empty() {
            return self.server.default_model.trim().to_string();
        }
        let sep = if self.server.separator.is_empty() {
            "-"
        } else {
            &self.server.separator
        };
        self.providers
            .iter()
            .flat_map(|p| p.models.iter().map(move |m| format!("{}{}{}", p.id, sep, m.alias)))
            .next()
            .unwrap_or_default()
    }

    /// Phase 22：按完整别名（`{id}{sep}{alias}`）查该模型的上下文后缀（`[1m]`/`[200k]`…）；
    /// 找不到或 context 为 0 返回空串。
    pub fn ctx_suffix_of(&self, full_alias: &str) -> String {
        let sep = if self.server.separator.is_empty() {
            "-"
        } else {
            &self.server.separator
        };
        for p in &self.providers {
            let prefix = format!("{}{}", p.id, sep);
            if let Some(alias) = full_alias.strip_prefix(&prefix) {
                if let Some(m) = p.models.iter().find(|m| m.alias == alias) {
                    return fmt_ctx_suffix(m.context);
                }
            }
        }
        String::new()
    }

    /// 当前使用模型（`server.default_model`，空则回退第一个别名）的上下文后缀。
    pub fn current_ctx_suffix(&self) -> String {
        self.ctx_suffix_of(&self.default_model())
    }

    /// 写入 claude 的哨兵模型值：`claude-relay-model{上下文后缀}`（如 `claude-relay-model[1m]`）。
    /// CC 收到后发回哨兵，中继用「当前使用模型」替换（Phase 22）。
    pub fn sentinel_model(&self) -> String {
        format!("{}{}", crate::router::RELAY_MODEL, self.current_ctx_suffix())
    }
}

impl AppState {
    /// Phase 25：托盘「模型」菜单——设置当前使用模型（`server.default_model`）。
    /// 流程：校验完整别名可路由 → 写盘 → 热重载（Arc<RwLock> 原地替换，下一请求即生效，零重启）。
    ///
    /// **阻塞版（供 std 托盘线程调用）**：tokio RwLock 的 `blocking_write` 在异步上下文调用会
    /// panic；托盘是独立 std 线程（非 runtime 上下文），故安全。返回设置的完整别名。
    ///
    /// 生效语义（D22 哨兵路由）：仅当 CC 发 `claude-relay-model[上下文]` 哨兵时用它；
    /// CC 设具体模型则继续走指定模型，切到哨兵后自动走本方法设置的模型。
    pub fn set_default_model_blocking(&self, full: &str) -> Result<String, String> {
        let mut guard = self.cfg.blocking_write();
        let mut cfg = guard.clone();
        cfg.server.default_model = full.to_string();
        // 防御性校验：完整别名必须能路由到已配置服务商/模型（托盘只给配置里的模型）
        crate::router::resolve(&cfg.providers, full, &cfg.server.separator)
            .map_err(|e| format!("model '{full}' 不可路由: {e}"))?;
        Config::save(&self.config_path, &cfg)?;
        *guard = cfg;
        Ok(full.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_entry_string_form_roundtrip() {
        let v: Vec<ModelEntry> = serde_json::from_str(r#"["mock-chat"]"#).unwrap();
        assert_eq!(v[0].alias, "mock-chat");
        assert_eq!(v[0].real, "mock-chat");
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"["mock-chat"]"#);
    }

    #[test]
    fn model_entry_protocol_field() {
        // Phase 60：模型行级协议覆盖——None=跟随服务商；空串归一化为 None；序列化向后兼容
        let s: ModelEntry = serde_json::from_str(r#""mock-chat""#).unwrap();
        assert_eq!(s.protocol, None);
        let o: ModelEntry =
            serde_json::from_str(r#"{"alias":"c","real":"mock-chat","protocol":"openai"}"#)
                .unwrap();
        assert_eq!(o.protocol.as_deref(), Some("openai"));
        let n: ModelEntry = serde_json::from_str(r#"{"alias":"c","real":"mock-chat"}"#).unwrap();
        assert_eq!(n.protocol, None);
        let e: ModelEntry =
            serde_json::from_str(r#"{"alias":"c","real":"mock-chat","protocol":""}"#).unwrap();
        assert_eq!(e.protocol, None);
        // alias==real + 默认上下文 + 无协议 → 字符串形式（磁盘整洁，向后兼容）
        let plain = ModelEntry::named("mock-chat");
        assert!(serde_json::to_value(&plain).unwrap().is_string());
        // 带 protocol → 对象形式且含 protocol；roundtrip
        let mut with = plain.clone();
        with.protocol = Some("openai".into());
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["protocol"], "openai");
        let back: ModelEntry = serde_json::from_value(v).unwrap();
        assert_eq!(back.protocol.as_deref(), Some("openai"));
    }

    #[test]
    fn model_entry_object_form_roundtrip() {
        let v: Vec<ModelEntry> =
            serde_json::from_str(r#"[{"alias":"flash","real":"deepseek-v4-flash"}]"#).unwrap();
        assert_eq!(v[0].alias, "flash");
        assert_eq!(v[0].real, "deepseek-v4-flash");
        assert_eq!(v[0].context, DEFAULT_CONTEXT);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"[{"alias":"flash","real":"deepseek-v4-flash"}]"#
        );
    }

    #[test]
    fn model_entry_context_roundtrip() {
        // context 非默认 → 对象携带 context；默认 → 省略
        let v: Vec<ModelEntry> = serde_json::from_str(
            r#"[{"alias":"gpt-4o","real":"gpt-4o","context":128000},{"alias":"chat","real":"deepseek-chat","context":65536}]"#,
        )
        .unwrap();
        assert_eq!(v[0].context, 128_000);
        assert_eq!(v[1].context, 65_536);
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains(r#""context":128000"#), "got: {s}");
        assert!(s.contains(r#""context":65536"#), "got: {s}");
        assert!(s.contains(r#""alias":"gpt-4o","real":"gpt-4o","context":128000"#), "got: {s}");
    }

    #[test]
    fn model_entry_default_context_string_form() {
        // 字符串形式始终回退默认上下文
        let v: Vec<ModelEntry> = serde_json::from_str(r#"["mock-chat"]"#).unwrap();
        assert_eq!(v[0].context, DEFAULT_CONTEXT);
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"["mock-chat"]"#);
    }

    #[test]
    fn provider_hide_usage_roundtrip() {
        // Phase 57：hide_usage 默认 false 不落盘；true 序列化并 roundtrip
        let s = serde_json::to_string(&ServerConfig::default()).unwrap();
        assert!(!s.contains("hide_usage"));
        let p: ProviderConfig =
            serde_json::from_str(r#"{"id":"ds","base_url":"https://x.test","api_key":"k","hide_usage":true}"#).unwrap();
        assert!(p.hide_usage);
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""hide_usage":true"#), "got: {s}");
        // 缺省字段解析为 false
        let p2: ProviderConfig =
            serde_json::from_str(r#"{"id":"ds","base_url":"https://x.test","api_key":"k"}"#).unwrap();
        assert!(!p2.hide_usage);
    }

    #[test]
    fn server_auto_start_roundtrip() {
        // 默认 false；缺省不写出；true 时序列化保留
        let s = serde_json::to_string(&ServerConfig::default()).unwrap();
        assert!(!s.contains("auto_start"), "默认 auto_start=false 不应写出: {s}");
        let mut c = ServerConfig::default();
        c.auto_start = true;
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"auto_start\":true"), "got: {s}");
        let back: ServerConfig = serde_json::from_str(&s).unwrap();
        assert!(back.auto_start);
        // 缺省字段解析为 false
        let back2: ServerConfig = serde_json::from_str(r#"{"host":"127.0.0.1"}"#).unwrap();
        assert!(!back2.auto_start);
    }

    #[test]
    fn card_refresh_secs_default_and_roundtrip() {
        // 默认 3、缺省不写出、自定义保留；1..=60 校验
        let s = serde_json::to_string(&ServerConfig::default()).unwrap();
        assert!(!s.contains("card_refresh_secs"), "默认 3 不应写出: {s}");
        let mut c = ServerConfig::default();
        c.card_refresh_secs = 5;
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"card_refresh_secs\":5"), "got: {s}");
        let back: ServerConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.card_refresh_secs, 5);
        let back2: ServerConfig = serde_json::from_str(r#"{"host":"127.0.0.1"}"#).unwrap();
        assert_eq!(back2.card_refresh_secs, 3);
        // 越界校验
        let bad: ServerConfig = serde_json::from_str(r#"{"host":"127.0.0.1","card_refresh_secs":0}"#).unwrap();
        let cfg = Config { server: bad, providers: vec![] };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_providers_is_valid_and_serializes_without_key() {
        // Phase 21：首次运行默认配置——空服务商合法（可启动后去配置页添加），
        // 且序列化省略 providers 键 → config.json 恰为 {server:{...}}。
        let cfg = Config::default();
        assert!(cfg.validate().is_ok(), "空服务商应通过校验");
        let s = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(!s.contains("providers"), "空服务商不应写出 providers 键: {s}");
        assert_eq!(cfg.server.host, DEFAULT_HOST);
        assert_eq!(cfg.server.port, DEFAULT_PORT);
        assert_eq!(cfg.server.separator, DEFAULT_SEP);
        // 反序列化缺省 providers 也能解析回空
        let back: Config = serde_json::from_str(&s).unwrap();
        assert!(back.providers.is_empty());
    }

    #[test]
    fn provider_is_opencode_detects() {
        // Phase 58：base_url 含 opencode，或 console_usage_url 含 opencode//workspace/ → true
        fn p(base: &str, usage: &str) -> ProviderConfig {
            serde_json::from_str(&format!(
                r#"{{"id":"oc","base_url":"{base}","api_key":"k","console_usage_url":"{usage}"}}"#
            ))
            .unwrap()
        }
        // 真实 OpenCode Go 网关
        assert!(p("https://opencode.ai/zen/go/v1", "").is_opencode());
        // console_usage_url 为 /go 页地址（含工作区 id）→ opencode
        assert!(p(
            "https://api.deepseek.com/anthropic",
            "https://opencode.ai/workspace/wrk_01KZQ6TA0DCPT2KZ1H43BG23R5/go"
        )
        .is_opencode());
        // 非 opencode 服务商 → false
        assert!(!p("https://api.deepseek.com/anthropic", "").is_opencode());
        assert!(!p(
            "https://ark.cn-beijing.volces.com/api/coding",
            "https://console.volcengine.com/api/.../GetCodingPlanUsage"
        )
        .is_opencode());
    }

    #[test]
    fn default_model_prefers_configured() {
        let mut cfg = Config::default();
        cfg.server.default_model = "ds-flash".into();
        cfg.providers = vec![ProviderConfig {
            id: "ds".into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![ModelEntry::named("deepseek-chat")],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }];
        assert_eq!(cfg.default_model(), "ds-flash");
        // 未配置时回退第一个别名
        cfg.server.default_model = String::new();
        assert_eq!(cfg.default_model(), "ds-deepseek-chat");
    }

    #[test]
    fn sentinel_model_appends_ctx_suffix() {
        let mut cfg = Config::default();
        cfg.server.default_model = "ds-v4-flash".into();
        cfg.providers = vec![ProviderConfig {
            id: "ds".into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![ModelEntry {
                alias: "v4-flash".into(),
                real: "deepseek-v4-flash".into(),
                context: 1_000_000,
                protocol: None,
            }],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }];
        assert_eq!(cfg.ctx_suffix_of("ds-v4-flash"), "[1m]");
        assert_eq!(cfg.sentinel_model(), "claude-relay-model[1m]");
        // 200k
        cfg.providers[0].models[0].context = 200_000;
        assert_eq!(cfg.sentinel_model(), "claude-relay-model[200k]");
        // 未知完整别名 → 无后缀
        assert_eq!(cfg.ctx_suffix_of("nope"), "");
    }

    #[test]
    fn sentinel_model_falls_back_to_first_alias() {
        let mut cfg = Config::default();
        cfg.server.default_model = String::new();
        cfg.providers = vec![ProviderConfig {
            id: "ds".into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![ModelEntry::named("deepseek-chat")], // 默认 context 200k
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }];
        assert_eq!(cfg.sentinel_model(), "claude-relay-model[200k]");
    }

    #[test]
    fn set_default_model_blocking_updates_and_saves() {
        // Phase 25：托盘「模型」菜单核心——设 default_model → 写盘 + 热重载 + 哨兵跟随；
        // 不可路由的模型拒绝且不改状态。纯 std 测试（非 tokio），blocking_write 安全。
        use std::path::PathBuf;
        let tmp = std::env::temp_dir().join(format!("relay_test_dm_{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut cfg = Config::default();
        cfg.providers = vec![ProviderConfig {
            id: "ds".into(),
            name: String::new(),
            base_url: "https://x.test/anthropic".into(),
            api_key: "k".into(),
            auth: "bearer".into(),
            models: vec![ModelEntry {
                alias: "v4-flash".into(),
                real: "deepseek-v4-flash".into(),
                context: 1_000_000,
                protocol: None,
            }],
            strip_beta: false,
            rewrite_model: true,
            protocol: "anthropic".into(),
            console_cookie: String::new(),
            console_usage_url: String::new(),
            hide_usage: false,
        }];
        let state = AppState {
            cfg: Arc::new(RwLock::new(cfg)),
            client: reqwest::Client::new(),
            debug: false,
            config_path: PathBuf::from(&tmp),
            started: std::time::Instant::now(),
            usage: Arc::new(std::sync::Mutex::new(UsageStats::default())),
            usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        // 设置成功：内存热重载 + 落盘 + 哨兵跟随
        assert_eq!(state.set_default_model_blocking("ds-v4-flash").unwrap(), "ds-v4-flash");
        assert_eq!(state.cfg.blocking_read().server.default_model, "ds-v4-flash");
        let ondisk: Config =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(ondisk.server.default_model, "ds-v4-flash");
        assert_eq!(state.cfg.blocking_read().sentinel_model(), "claude-relay-model[1m]");

        // 不可路由 → Err，状态与磁盘都不变
        assert!(state.set_default_model_blocking("nope-x").is_err());
        assert_eq!(state.cfg.blocking_read().server.default_model, "ds-v4-flash");
        let ondisk: Config =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(ondisk.server.default_model, "ds-v4-flash");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn usage_stats_persist_roundtrip() {
        // Phase 27：用量持久化——缺失→空统计、save→load roundtrip、损坏→空统计（不崩）
        let tmp = std::env::temp_dir().join(format!("relay_test_usage_{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        // 文件缺失 → 空统计
        assert!(UsageStats::load(&tmp).providers.is_empty());

        let mut u = UsageStats::default();
        let pu = u.providers.entry("ds".into()).or_default();
        pu.requests = 3;
        pu.input_tokens = 100;
        pu.output_tokens = 50;
        let mu = pu.by_model.entry("ds-v4-flash".into()).or_default();
        mu.requests = 3;
        mu.input_tokens = 100;
        mu.output_tokens = 50;
        u.save(&tmp).unwrap();

        // roundtrip
        let back = UsageStats::load(&tmp);
        let p = back.providers.get("ds").expect("ds provider");
        assert_eq!(p.requests, 3);
        assert_eq!(p.input_tokens, 100);
        assert_eq!(p.by_model.get("ds-v4-flash").unwrap().output_tokens, 50);

        // 损坏 → 空统计不崩
        std::fs::write(&tmp, "not-json").unwrap();
        assert!(UsageStats::load(&tmp).providers.is_empty());

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("tmp"));
    }

    #[test]
    fn record_creates_hour_and_day_buckets() {
        // Phase 54：固定 now 验证小时桶（UTC 整点）与天桶（本地日零点）ts 与计数
        let mut u = UsageStats::default();
        let now = 1_700_000_000i64;
        u.record("ds", "ds-chat", 1, 10, 5, now);
        u.record("ds", "ds-chat", 1, 0, 0, now + 60); // 同一小时桶叠加请求数
        let pu = u.providers.get("ds").unwrap();
        assert_eq!(pu.requests, 2);
        assert_eq!(pu.input_tokens, 10);
        assert_eq!(pu.output_tokens, 5);
        let mu = pu.by_model.get("ds-chat").unwrap();
        assert_eq!(mu.requests, 2);
        assert_eq!(mu.time.hours.len(), 1);
        assert_eq!(mu.time.hours[0].ts, now - now % 3600);
        assert_eq!(mu.time.hours[0].requests, 2);
        assert_eq!(mu.time.hours[0].input_tokens, 10);
        assert_eq!(mu.time.hours[0].output_tokens, 5);
        assert_eq!(mu.time.days.len(), 1);
        assert_eq!(mu.time.days[0].ts, day_start_with_offset(now, local_utc_offset_secs()));
        assert_eq!(mu.time.days[0].requests, 2);
    }

    #[test]
    fn time_buckets_prune_by_retention() {
        // Phase 54：prune 按保留窗口剪除超期小时/天桶
        let mut u = UsageStats::default();
        let now = crate::config::unix_now();
        u.record("ds", "ds-chat", 1, 10, 5, now - 100 * 3600);  // 超 48h 小时桶
        u.record("ds", "ds-chat", 1, 20, 10, now - 10 * 3600);   // 48h 内
        u.record("ds", "ds-chat", 1, 30, 15, now - 20 * 86400);  // 超 14 天天桶
        u.record("ds", "ds-chat", 1, 40, 20, now - 5 * 86400);   // 14 天内
        u.prune(48, 14);
        let mu = u.providers.get("ds").unwrap().by_model.get("ds-chat").unwrap();
        assert_eq!(mu.time.hours.len(), 1, "只剩 48h 内的小时桶");
        assert_eq!(mu.time.hours[0].input_tokens, 20);
        // 天桶：100h 前（≈4 天）、10h 前（今天）、5 天前各 1 条，均在 14 天窗口内；20 天前那条被剪除
        assert_eq!(mu.time.days.len(), 3, "14 天窗口内应保留 3 个天桶");
        assert!(!mu.time.days.iter().any(|b| b.input_tokens == 30), "20 天前的天桶应被剪除");
        assert!(mu.time.days.iter().any(|b| b.input_tokens == 40), "5 天前的天桶应保留");
    }

    #[test]
    fn day_start_with_offset_pure() {
        // Phase 54：本地日零点纯函数——固定 offset 可测（UTC+8）
        let off = 8 * 3600;
        let now = 1_700_000_000i64;
        let day_start = day_start_with_offset(now, off);
        // 本地日零点对 86400 取模应为整日边界
        assert_eq!((day_start + off) % 86400, 0);
        assert!(day_start <= now && day_start > now - 86400);
        // 无偏置 = UTC 日零点
        assert_eq!(day_start_with_offset(now, 0) % 86400, 0);
    }

    #[test]
    fn usage_retention_defaults_apply() {
        // Phase 54：usage_hours/usage_days 默认 48/14；缺省 config 解析回落默认
        let s = ServerConfig::default();
        assert_eq!(s.usage_hours, 48);
        assert_eq!(s.usage_days, 14);
        // 默认值不写出
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("usage_hours"), "默认保留窗口不应写出: {j}");
        // 旧 config（无此键）→ 默认
        let back: ServerConfig = serde_json::from_str(r#"{"host":"127.0.0.1"}"#).unwrap();
        assert_eq!(back.usage_hours, 48);
        assert_eq!(back.usage_days, 14);
    }

    #[test]
    fn usage_time_buckets_roundtrip() {
        // Phase 54：时间桶随 UsageStats 一起持久化（config-usage.json 含 time）
        let tmp = std::env::temp_dir().join(format!("relay_test_time_{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut u = UsageStats::default();
        u.record("ds", "ds-chat", 2, 30, 15, crate::config::unix_now());
        u.save(&tmp).unwrap();
        let back = UsageStats::load(&tmp);
        let mu = back.providers.get("ds").unwrap().by_model.get("ds-chat").unwrap();
        assert_eq!(mu.time.hours.len(), 1);
        assert_eq!(mu.time.hours[0].requests, 2);
        assert_eq!(mu.time.hours[0].input_tokens, 30);
        assert_eq!(mu.time.days.len(), 1);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("tmp"));
    }

    #[test]
    fn write_file_atomic_roundtrip_and_heal() {
        // Phase 59：原子写 roundtrip + 父目录缺失自动创建 + tmp 无残留 + 只读目标自愈
        let dir = std::env::temp_dir().join(format!("relay_wfa_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("cfg.json"); // 父目录 sub 不存在 → 自动创建
        write_file_atomic(&path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        assert!(!path.with_extension("tmp").exists(), "写完不应残留 tmp");

        // 覆盖写（rename 替换既有文件）
        write_file_atomic(&path, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");

        // 只读目标自愈：设置只读属性后再写 → 自动解除并写回成功（跨机拷贝常见失败场景）
        {
            let md = std::fs::metadata(&path).unwrap();
            let mut perms = md.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        write_file_atomic(&path, "{\"a\":3}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":3}");
        assert!(
            !std::fs::metadata(&path).unwrap().permissions().readonly(),
            "只读属性应被自动解除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

