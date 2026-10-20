# ClaudeRelay 二次开发文档

> 面向要在本项目上做二次开发的开发者。读完本文 + `CLAUDE.md`（项目主说明）+ `findings.md`（踩坑记录），可以较完整地理解这个项目的设计、代码结构、扩展方式与坑。

---

## 1. 项目定位与核心思想

**ClaudeRelay** 是一个 Rust 编写的本地中转服务，部署在 **Claude Code（CC）** 与多个 LLM 服务商之间。CC 把 `ANTHROPIC_BASE_URL` 指向它，用户在 CC 里用 `/model {服务商前缀}-{模型}` 动态切换服务商和模型，**零重启**。

核心思想是**全 Anthropic 协议纯透传**：

```
改 model → 换 Key → 原样转发 → SSE 流式透传回 CC
```

- 不做协议翻译（D3）：假设每个服务商都提供 Anthropic 兼容端点
- 不做缓冲转换：SSE 字节级透传
- 一个 exe、一个端口、零依赖外部服务

**三个硬性约束**（做二开时必须守住）：

| 约束 | 现状 |
|------|------|
| exe 体积 ≤ 6MB | release 约 **2.32MB** |
| 空闲内存极小 | 单线程 tokio `current_thread`，个位数 MB |
| 空闲 CPU ≈ 0 | 纯事件驱动，空闲无轮询 |

---

## 2. 技术栈

| 层 | 选型 | 说明 |
|----|------|------|
| 语言 | Rust（`x86_64-pc-windows-gnu` 工具链） | 目标平台 Windows 10+；GNU 而非 MSVC（本机无 MSVC 链接器） |
| 运行时 | tokio 1，**`current_thread` 单线程** | 空闲内存小的关键；不用多线程 |
| Web 框架 | axum 0.8（仅 `http1`） | 裁剪掉 http2/ws/form |
| HTTP 客户端 | reqwest 0.12（`rustls-tls`） | 不依赖 OpenSSL |
| 序列化 | serde + serde_json | 配置读写、请求改写 |
| 流式 | futures-util（std 子集） | SSE 流的 unfold |
| Windows 集成 | tray-icon 0.20 + windows-sys 0.59 | 托盘、消息泵、注册表、控制台 API |
| UI | 原生 JS 单文件 + Tailwind v4（编译内嵌） | 无前端构建框架 |

完整依赖见 `Cargo.toml`。release profile 用 `opt-level="z"` + `lto` + `codegen-units=1` + `panic="abort"` + `strip` 压体积，**不要随便调低**。

---

## 3. 目录结构

```
ClaudeModelSwitch/
├── Cargo.toml / Cargo.lock     # 依赖（见上表）
├── build.rs                    # 空（历史 workaround 已移除）
├── src/
│   ├── main.rs                 # 入口：CLI 解析、双击隐藏黑框、服务循环
│   ├── config.rs               # 配置结构体、JSON 读写、校验、AppState
│   ├── router.rs               # 模型名最长前缀匹配 → (服务商, 真实模型)
│   ├── proxy.rs                # /v1/messages 转发核心 + SSE 透传
│   ├── api.rs                  # axum Router 构建 + 辅助端点
│   ├── webui.rs                # Web 配置页后端（/api/* 读写）
│   ├── cookies.rs              # 浏览器 Cookie 库读取（DPAPI+AES-GCM+SQLite，Phase 28）
│   ├── tray.rs                 # Windows 系统托盘（图标/菜单/消息泵）
│   └── autostart.rs            # 开机自启注册表（HKCU Run 键）
├── webui/
│   ├── index.html              # 纯原生 JS 单文件配置页（内嵌进 exe）
│   ├── build_ui.py             # Tailwind 一键编译 + 注入
│   └── tailwind/               # Tailwind v4 构建环境（in.css/out.css/package.json）
├── test/
│   ├── e2e.py                  # 端到端验收（25 断言）
│   ├── mock_upstream.py        # mock Anthropic 上游
│   ├── config.mock.json        # mock 配置（假 Key）
│   └── measure.ps1             # 内存/体积测量
├── start-claude.cmd            # 启动 CC（隔离 settings，指向 relay）
├── config.json                 # ⚠️ 真实配置（含 Key，已被 .gitignore 忽略）
├── claude-settings.json        # ⚠️ settings 副本（含 Key，被忽略）
├── CLAUDE.md                   # 项目主说明（CI/agent 用）
└── DEVELOPMENT.md              # 本文档
```

---

## 4. 快速上手

```bash
# 开发构建（debug）
cargo build

# 检查编译（不生成二进制）
cargo check

# 运行（使用项目根 config.json，默认 127.0.0.1:8787）
cargo run -- [--debug] [--host 127.0.0.1] [--port 8787] [--config config.json]

# 测试
cargo test            # 43 个单元测试
python test/e2e.py    # 端到端验收（25 断言，mock 上游 + relay 子进程）

# 发布构建（两步：先 UI 后 Rust）
python webui/build_ui.py   # 1) 编译 Tailwind 并注入 index.html（仅改 UI 后需要）
cargo build --release      # 2) 打包，产物 target/release/claude-relay.exe (~2.32MB)
```

**注意**：本机 cargo 不在 PATH，需 `export PATH="$HOME/.cargo/bin:$PATH"`（Git Bash）。

---

## 5. 架构总览

```
Claude Code ──(Anthropic Messages)──▶ ClaudeRelay (127.0.0.1:8787)
                                        │
                                        ├─ /v1/messages              ← proxy.rs（POST，SSE 透传）
                                        ├─ /v1/messages/count_tokens ← proxy.rs（转发 + 启发式回退）
                                        ├─ /v1/models                ← api.rs（聚合别名）
                                        ├─ /healthz                  ← api.rs
                                        ├─ /                         ← webui.rs（配置页 HTML）
                                        └─ /api/config, /api/status, /api/restart, /api/update-settings,
                                           /api/balance, /api/coding-plan, /api/import-cookie ← webui.rs
                                              │
                              ┌───────────────┼───────────────┐
                              ▼               ▼               ▼
                        DeepSeek          Kimi          Anthropic
                        (各自 Anthropic 兼容端点)
```

**进程模型**：单进程、单 tokio `current_thread` runtime，配一个独立 **std 线程**跑 Windows 托盘消息泵。托盘线程与 tokio 服务循环通过 `tokio::sync::watch` 通道通信（停止/启动）。

### 一次 `/v1/messages` 的完整数据流

```
CC 请求 → proxy::messages()
  → 读 body（上限 64MB）→ 解析 JSON → 取 model 字段
  → router::resolve_for_request(&cfg, model) → (provider_idx, real_model)
     （D22：`claude-relay-model[上下文]` 哨兵 → 用配置「当前使用模型」替换；任何模型先剥 `[ctx]` 后缀，
       再按服务商 id 最长前缀匹配；后缀先查模型别名 → 命中发 real，否则后缀即真实模型名）
  → json["model"] = real_model
  → 白名单透传请求头（FORWARD_HEADERS）
  → 替换认证头（Bearer 或 x-api-key，取决于 ProviderConfig.auth）
  → reqwest POST {base_url}/v1/messages
  → 响应：
      SSE → sse_rewrite_stream() 逐行把真实 model 改回别名（ModelRewriter 双轨）
      非 SSE → 字节流透传
  → 响应头白名单透传（anthropic-*, x-*, content-type, retry-after）
  → 返回 CC
```

---

## 6. 模块详解

### 6.1 `src/main.rs` — 入口与服务循环

| 项 | 说明 |
|----|------|
| `struct Args` | CLI 参数：`config_path`/`host`/`port`/`debug`/`print_default_model`/`autostart`/`restart` |
| `hide_console_if_double_clicked()` | **双击隐藏黑框**（D19）：`GetConsoleProcessList` 返回 1（独占控制台 = 双击/Explorer 启动）→ `ShowWindow(SW_HIDE)`；返回是否「双击场景」 |
| `resolve_config_path()` | `--config` 未指定时 CWD/config.json，不存在回退 **exe 同目录**（开机自启 CWD=System32 也能加载） |
| `bind_with_retry()` | 20×150ms 绑定重试（扛 `/api/restart` 新旧交接）；返回 `Result`；首次失败 exit(1)，托盘重启路径 1s 重试 |
| `port_is_busy()` | 500ms TCP 探测端口是否被占用；**`--restart` 时跳过**（防自重启误判，见 §10 坑） |
| `run_server()` | **服务循环**（D18）：`watch::Receiver<bool>` 监听托盘停止/启动；`paused=false` → bind+serve（`tokio::select!` 同时监听停止信号）；`paused=true` → 等启动信号重新 bind。**停止 = drop serve future → 端口释放**；`open_browser` 参数=双击首次就绪后开配置页 |
| `async_main()` | 组装 AppState、banner、托盘 spawn、进入 `run_server` |

### 6.2 `src/config.rs` — 配置与共享状态

**配置结构**（对应 config.json）：

```rust
struct ServerConfig { host, port, separator, default_model, auto_start: bool }  // auto_start 默认 false，skip_serializing_if
struct ProviderConfig { id, name, base_url, api_key, auth, models, strip_beta, rewrite_model, protocol, console_cookie, console_usage_url }  // protocol 默认 "anthropic"（skip_serializing_if），"openai" 走 translate.rs（D36），"gemini" 走 gemini.rs（D56）
struct ModelEntry { alias, real, context: u64 }   // context 默认 DEFAULT_CONTEXT=200000
struct Config { server, providers }
```

- `ModelEntry` 自定义 serde：字符串或 `{alias,real,context?}` 双形；`alias==real && context==默认` 序列化为字符串
- `Config::load/save/validate/masked/default_model`（默认模型=**当前使用模型**，D22）
- **哨兵辅助（D22）**：`Config::sentinel_model()`（`claude-relay-model[上下文]`，写入 claude 用）、`current_ctx_suffix()` / `ctx_suffix_of(full_alias)`（上下文后缀 `[1m]`/`[200k]`…，`fmt_ctx_suffix` 内部：1e6 整倍→`[Nm]`、千整倍→`[Nk]`、否则四舍五入 k）
- **校验（D21）**：`validate()` 允许空服务商（`providers.is_empty()` 不再致命——首次运行/全部删除后去配置页添加即可，空配置下 `/v1/messages` 由 `router::resolve` 返回 400 不 panic）；`Config.providers` 带 `skip_serializing_if="Vec::is_empty"` → 无配置首跑生成的 `config.json` 恰为 `{"server":{host,port,separator,default_model}}`
- `AppState`（运行期共享）：`cfg: Arc<RwLock<Config>>`、`client`、`debug`、`config_path`、`started`、`usage`（D26）、`usage_dirty`（D27）。**无停止标志**（停止状态在 main 的 watch 通道）
- **`AppState::set_default_model_blocking(full)`（D25）**：托盘设「当前使用模型」——`router::resolve` 校验完整别名可路由 → `Config::save` → `blocking_write` 热重载（零重启）。**阻塞版**（供 std 托盘线程调用；tokio RwLock 的 `blocking_write` 在异步上下文调用会 panic，故勿在 runtime 内调用）
- **用量统计（D26）+ 持久化（D27）**：`UsageStats { providers: HashMap<id, ProviderUsage> }`、`ProviderUsage { requests, input_tokens, output_tokens, by_model }`、`ModelUsage`——请求数+输入/输出 tokens 按服务商+模型累加（模型键=完整别名剥 `[ctx]` 后缀），`std::sync::Mutex` 短锁（relay 单线程 + 托盘只读）；**serde 持久化**——`UsageStats::save(path)` 原子写（`.tmp`+rename）+ `load(path)`（缺失/损坏→空），main 5s 周期落盘（`usage_dirty` 脏标记，空闲零 IO），重启不清零
- **Coding Plan 用量（D28/D30）**：`ProviderConfig` 加 `console_cookie`（控制台 Cookie——火山方舟 `console.volcengine.com` / 讯飞星辰 `maas.xfyun.cn`，`mask_cookie` 掩码保留尾 8 字符 + PUT 复用，同 D13）+ `console_usage_url`（可选，默认按服务商推导：火山方舟 `https://console.volcengine.com/api/top/ark/{region}/2024-01-01/GetCodingPlanUsage`、讯飞 `https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`）；`Config::mask_cookie()`

### 6.3 `src/router.rs` — 模型名解析

```rust
struct Resolved { provider: usize, real_model: String, alias: String }
fn resolve(providers: &[ProviderConfig], model: &str, sep: &str) -> Result<Resolved, String>
fn resolve_for_request(cfg: &Config, model: &str) -> Result<Resolved, String>  // D22
fn split_ctx_suffix(model: &str) -> (&str, Option<&str>)                        // D22
fn available_aliases(providers: &[ProviderConfig], sep: &str) -> Vec<String>
const RELAY_MODEL: &str = "claude-relay-model"                                  // D22 哨兵
```

**最长前缀匹配**（D12）：模型名本身含 `-`（如 `deepseek-chat`），不能按第一个分隔符切。遍历所有服务商，`{id}{sep}` 前缀命中且 id 最长者胜。后缀先查该服务商配置的 `models[].alias`，命中 → 发 `real`；否则后缀直接当真实模型名（向后兼容）。

**哨兵路由（D22）**：`resolve_for_request` 先 `split_ctx_suffix` 剥 CC 上下文后缀（`claude-relay-model[1m]`→`claude-relay-model`，`ds-v4-flash[1m]`→`ds-v4-flash`）；若 base==`claude-relay-model` → 用 `cfg.default_model()`（当前使用模型）替换后 resolve，`Resolved.alias` 回写**原始哨兵串**（SSE 原样回显）；否则正常 resolve。`resolve` 内部也先剥后缀，`Resolved.alias` 保留原始完整模型串（含后缀）。**上游发干净模型名**（`[ctx]` 不转发）。

### 6.4 `src/proxy.rs` — 转发核心

| 项 | 说明 |
|----|------|
| `messages()` / `count_tokens()` | 两个端点共用 `forward()` |
| `forward()` | 解析→匹配→改写 model→替换认证→白名单头→上游请求→透传响应 |
| `FORWARD_HEADERS` | 请求头白名单（content-type/accept/anthropic-*/x-stainless-* 等） |
| `ModelRewriter` | SSE 流内 `"model":"<真实>"` → `"<别名>"` 改写；**双轨**：紧凑 JSON 字节快路径 + 空格 JSON 递归慢路径；**D26：所有 SSE 统一走该缓冲流并提取用量**——含 `usage` 子串的行必须慢路径解析（跳过字节快路径防漏计），`message_start` 累加 `message.usage.input_tokens`、`message_delta` 累加 `usage.output_tokens`；不含 usage 且 `do_rewrite=false` 时零解析（纯透传） |
| `UsageSink` / `accumulate_usage()` | D26：用量收集目标（共享 `UsageStats` + 本流 provider/model 键）与累加器；count_tokens 响应解析 `input_tokens`、非 SSE `/v1/messages` 读体解析顶层 `usage`；**D27：每处累计置 `usage_dirty`** |
| `heuristic_tokens()` | count_tokens 上游失败时回退 `字符数/4`（D8）；回退路径不计真实用量（仅请求数） |

### 6.5 `src/api.rs` — 路由构建

`build(state) -> Router` 挂全部路由（§5 表）。含 `VERSION`（`env!("CARGO_PKG_VERSION")`）。辅助端点 `healthz`/`root`/`models`/`config_get`。

### 6.5c `src/translate.rs` — OpenAI 协议翻译（D36）

`ProviderConfig.protocol == "openai"` 时启用——让仅 OpenAI Chat Completions 协议的服务商（OpenRouter/Qwen 百炼等）可用。

| 项 | 说明 |
|----|------|
| `anthropic_to_openai(body, model)` | 请求体翻译：`system`（字符串/数组）→ 首位 system 消息；messages 里 `tool_use`→OpenAI `tool_calls`、`tool_result`→`role:"tool"` 消息、image→`image_url` data URL、`thinking` 丢弃；`tools`→function tools；`tool_choice` 映射（any→required / tool→指定函数 / 默认 auto）；`stop_sequences`→`stop`；stream 时加 `stream_options.include_usage` |
| `openai_message_to_anthropic(v, alias)` | 非流式响应：`choices[0].message` → Anthropic message（text + tool_use 内容块），`stop_reason` 映射（tool_calls→tool_use / length→max_tokens），usage=prompt/completion_tokens，`model` 回写别名 |
| `OpenAiStreamTranslator` | 流式：消费 OpenAI SSE（`data: {chunk}`…`[DONE]`）→ 产出 Anthropic 事件序列。文本增量开 text 块发 `text_delta`；`delta.tool_calls` 按 OpenAI tool index 缓冲为 tool_use 块发 `input_json_delta` 分片；收尾 `message_delta`（stop_reason 映射）+ `message_stop`，并把 usage chunk 的 prompt/completion_tokens 提取到共享 UsageStats（无 usage 用字符/4 启发式）。`emit` 是模块级自由函数（避免 `iter_mut` 循环里 `&self` 借冲突） |
| 接线 | proxy.rs `is_openai` → URL=`{base}/chat/completions`、剥 `anthropic-version/beta`、请求体翻译、响应走翻译器；count_tokens 直接 `heuristic_tokens` 回退 |

### 6.5d `src/gemini.rs` - Gemini 原生协议翻译（D56）

`ProviderConfig.protocol == "gemini"` 时启用--让仅 Gemini generateContent 协议的服务商（Google AI Studio 官方 `generativelanguage.googleapis.com`）可用。

| 项 | 说明 |
|----|------|
| `anthropic_to_gemini(body, model)` | 请求体翻译：`system`->`systemInstruction`；role `assistant`->`model` 且**合并连续同角色**（Gemini 要求 user/model 严格交替）；`tool_use`->`functionCall`（args 直接是对象）、`tool_result`->`functionResponse`（**须扫历史 assistant 块建 tool_use_id->name 映射**，Anthropic 只给 id）、image->`inlineData`（base64 原样）、thinking 块->`thought:true` part（`signature`<->`thoughtSignature` 往返，Gemini 函数调用要求签名回传）；`tools`->`functionDeclarations`、`tool_choice`->`toolConfig`；max_tokens/temperature/top_p/stop_sequences/thinking(budget_tokens)->`generationConfig`。**stream 不进 body**（流式由 URL 端点选择） |
| `gemini_message_to_anthropic(v, alias)` | 非流式：`candidates[0].content.parts`->thinking/text/tool_use 块；`finishReason` 映射（STOP->end_turn / MAX_TOKENS->max_tokens / SAFETY 等->refusal）；usageMetadata：input=promptTokenCount、output=candidates+**thoughts**TokenCount |
| `GeminiStreamTranslator` | 流式：`?alt=sse` 产出 `data:{json}` 行、**无 [DONE]**（流自然结束->`finish()` 收尾）；functionCall 整体一个 `input_json_delta`（Gemini 不分片）；每个 chunk 随时更新 usageMetadata；收尾 accumulate_usage（缺 promptTokenCount 用 input_fallback 启发式） |
| 接线 | proxy.rs `is_gemini` -> URL=`{base}/v1beta/models/{real}:generateContent`（流式 `:streamGenerateContent?alt=sse`，**冒号分隔**）；认证 `auth="x-goog-api-key"` -> `x-goog-api-key` 头；剥 `anthropic-version/beta`；count_tokens 直接 `heuristic_tokens` 回退；上游错误体 `error.status`（无 `error.type`）回退提取进 Anthropic type:error |

### 6.5b `src/cookies.rs` — 浏览器 Cookie 库读取（D28）

| 项 | 说明 |
|----|------|
| `read_console_cookie(domain_like, display)` | **域参数化统一入口（D30）**：遍历 Chrome/Edge 各 profile 的 `(Local State, Network/Cookies)`，解出目标控制台域 Cookie 头；全部失败返回 Err（浏览器锁库则报明确提示，调用方回退手动粘贴）。包装：`read_volcengine_console_cookie()`（`%volcengine.com`，D28）/ `read_xf_console_cookie()`（`%xfyun.cn`，D30） |
| `load_os_crypt_key()` | 读 `Local State` 的 `os_crypt.encrypted_key`（base64 `DPAPI` 前缀）→ `CryptUnprotectData` 解密 → 32B 主密钥 + `app_bound_encrypted_key`（v11 尽力） |
| `query_cookies()` | **锁库对策**：主库+`-wal`+`-shm` 复制到临时目录，读-写打开副本让 SQLite 回放 WAL；`SELECT name,value,encrypted_value ... WHERE host_key LIKE ?1`（域参数）→ v10/v11 解密 → 拼 `name=value; ...` |
| 关键 API | windows-sys 0.59 用 **`CRYPT_INTEGER_BLOB`**（非 `DATA_BLOB`）；`LocalFree` 在 `Win32::Foundation` |

`build(state) -> Router` 挂全部路由（§5 表）。含 `VERSION`（`env!("CARGO_PKG_VERSION")`）。辅助端点 `healthz`/`root`/`models`/`config_get`。

### 6.6 `src/webui.rs` — 配置页后端

| 端点 | 行为 |
|------|------|
| `GET /` | 返回内嵌 `webui/index.html`（`include_str!`） |
| `PUT /api/config` | 掩码 Key 复用（D13）→ validate → 写盘 → 热重载 → **`autostart::apply(auto_start)`**（失败进 warnings） |
| `GET /api/status` | `{version, running, host, port, separator, providers, models, model_details, usage, uptime_secs}`（`usage`=D26 用量：每服务商 请求数/输入/输出 tokens + 按模型明细） |
| `GET /api/balance` | D26/D29：余额——`balance_root()` 识别有简单接口的服务商（当前仅 `api.deepseek.com`，剥 `/anthropic` 取主机根 → `GET {root}/user/balance`，同 key Bearer/x-api-key，8s 超时不缓存）；**无接口的服务商不在响应中**（`continue`，待有 API 支持再加） |
| `GET /api/coding-plan` | **D28/D30：Coding Plan 用量**——`console_kind()` 分派：火山方舟（Ark，base_url 含 volces/volcengine）查 `console_usage_url`（默认按区域推导）解析 `Result.QuotaUsage[]` → `[{level,label,percent,reset_ts}]`（session→5小时/weekly→一周/monthly→一月）；讯飞星辰（Xf，base_url 含 xfyun.cn/xf-yun.com 或 URL 含 coding-plan/list）查 `xf_usage_url`（默认官方端点）解析 `data.rows[]`，**按 `codingPlanAppCredentialDTO.apiKey == p.api_key` 过滤** → `plans[]`（套餐名/包量%/剩余/5小时/一周/到期；多个配置服务商/多匹配行→多套餐）。cookie 源 = `console_cookie`（存档）或按 kind 自动读浏览器对应域；带 Cookie/Referer/UA，8s 超时 |
| `POST /api/import-cookie` | **D28/D30**：按目标服务商 kind 自动读浏览器对应控制台域 Cookie（Ark→volcengine / Xf→xfyun）→ 存 `provider.console_cookie` → 写盘热重载 → 返回掩码（`mask_cookie`） |
| `POST /api/restart` | spawn 同参数新进程 + **追加 `--restart`**（Phase 20）→ 600ms 后 exit(0) |
| `POST /api/update-settings` | 写 BASE_URL/KEY + **哨兵 MODEL**（`claude-relay-model[上下文]`，D22）：**顶层 `model` 键 + env 都写**，env 含 `ANTHROPIC_MODEL` + **三个任务槽** `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`（均=哨兵值，D24：CC 子代理/compact 走独立槽位，裸名会 400；实测顶层 model 键优先于 env，只写 env 不生效；尊重 `CLAUDE_CONFIG_DIR`，其余保留） |

### 6.7 `src/tray.rs` — Windows 托盘

- 程序化生成 32×32 图标（绿圆+白闪电，`Icon::from_rgba`，无需 .ico 文件）
- Windows 消息泵（`GetMessageW`/`DispatchMessageW`）处理 WM_TRAYICON/WM_COMMAND
- 菜单：**设置 / 模型（D25 子菜单：模型→服务商→模型，`CheckMenuItem` 勾选当前模型）/ 停止\|启动（两字，随状态切换）/ 退出**；双击打开配置页
- **模型子菜单（D25）**：`spawn` 收 `state: Arc<AppState>` + `model_menu: Vec<(服务商显示名, [(完整别名, 别名)])>` + `current` 快照（main 从 cfg 构建，启动时一次）；`MenuEvent` 命中模型 id → `state.set_default_model_blocking` → 遍历 `set_checked` 刷新勾选。选中不改 CC 自身模型，**仅当 CC 发 `claude-relay-model[ctx]` 哨兵时生效**（D22），CC 指定具体模型则继续走指定。空服务商占位「（未配置服务商）」；Web UI 改配置后需重启托盘刷新菜单
- 持有 `watch::Sender<bool>`：点停止 `send(true)` → run_server 释放端口；点启动 `send(false)` → 恢复
- `pub fn open_browser(url)`：main 双击场景复用
- 非 Windows：no-op

### 6.8 `src/autostart.rs` — 开机自启

- `apply(true)` 写 `HKCU\...\CurrentVersion\Run` 值 `ClaudeRelay = "<exe>" --autostart`
- `apply(false)` 删除该值（幂等）
- 启动时**只注册不删除**（防 mock 配置启动误删用户已启用的注册表项）；关闭方向走 UI 保存（config_put）
- 非 Windows：no-op

---

## 7. CLI 参数

| 参数 | 含义 |
|------|------|
| `--config <path>` | 配置文件（默认 `config.json`，缺失回退 exe 同目录） |
| `--host <ip>` / `--port <port>` | 监听地址/端口（默认 127.0.0.1:8787） |
| `--debug` | 请求/转发日志 |
| `--print-default-model` | 打印哨兵模型值 `claude-relay-model[上下文]` 后退出（D22，供 start-claude.cmd 提示） |
| `--autostart` | 后台启动（开机自启/脚本）：不弹配置页；端口被占时静默退出 |
| `--restart` | **内部**：/api/restart 自重启用——跳过端口占用检查、不弹配置页 |
| `-h` / `--help` | 帮助 |

---

## 8. 配置文件格式（config.json）

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 8787,
    "separator": "-",
    "default_model": "ds-flash",
    "auto_start": false
  },
  "providers": [
    {
      "id": "ds",
      "name": "DeepSeek",
      "base_url": "https://api.deepseek.com/anthropic",
      "api_key": "sk-...",
      "auth": "bearer",
      "models": [
        "deepseek-chat",
        { "alias": "flash", "real": "deepseek-v4-flash", "context": 128000 }
      ],
      "strip_beta": false,
      "rewrite_model": true
    }
  ]
}
```

字段语义：
- `id`：服务商前缀，全局唯一 → CC 里输入 `{id}-{alias}`（如 `ds-flash`）
- `base_url`：**Anthropic 兼容端点**根地址，中继拼 `/v1/messages`
- `auth`：`bearer`（Authorization: Bearer，默认）或 `x-api-key`
- `models`：字符串（别名==真实名）或 `{alias, real, context?}`（context 默认 200000，仅 UI 展示元数据）
- `server.default_model`：**当前使用模型**完整别名（D22）；空则回退第一个服务商第一个模型别名。CC 发 `claude-relay-model[上下文]` 哨兵时用它替换；另生成哨兵值写 claude 配置
- 配置**热重载**：改文件或 Web UI 保存后，下一请求即生效（`Arc<RwLock>` 原地替换）

⚠️ `config.json` / `claude-settings.json` 含真实 API Key，**已被 `.gitignore` 忽略，切勿提交**。

---

## 9. Web UI 架构（webui/index.html）

纯原生 JS 单文件，无前端框架。**样式分两层**：

- `<style id="tw">`：Tailwind v4 **编译产物**（`python webui/build_ui.py` 每次重置并重编译注入）
- `<style id="ui">`：**手写组件层**（`.btn`/`.btn-solid`+颜色变体/`.btn-ghost`/`.btn-danger-ghost`/`.btn-sm`/`.btn-bar`/`.btn-icon`/`.ic`；**Phase 39 起含全局 `body{letter-spacing:.012em;...}` 字距与 `.leading-loose-inline` 行高**），build_ui.py **不覆盖**，二开按钮/全局样式加这里

页面结构：
- **顶部操作栏**：5 个分色按钮（保存配置/保存并重启/更新 claude 配置/更新环境变量/复制 CC 环境变量命令）+ **按钮下方独立 toast 反馈行**（`#toast`，`min-h-[18px]` 防跳动）
- **服务设置**：监听地址/端口/分隔符/默认模型下拉（富选项：徽章+别名+真实名+上下文徽标）/开机自启开关
- **模型订阅列表（Phase 38 术语：配置条目叫「模型订阅」，上游厂商叫「服务商」）**：每行 **服务商名称徽标**（`provName`，不再显示 ID 简写）+ 名称 + 模型/URL 摘要（单行截断 + title）+ 编辑/删除；**可全部删除**（D21，删空后列表显示"暂无模型订阅"引导添加）
- **已注册模型**：别名 chip + 上下文徽标
- **添加/编辑模型订阅弹窗**：**「服务商」下拉必填**（`#m-preset`：6 预设 + 自定义；选预设自动填 ID 简写/名称/Base URL/认证，ID 可改；编辑时 `presetOfProvider` 按 id/base_url 推断）；**控制台 Cookie 输入框仅服务商选讯飞/火山时显示**（`toggleCookieField` 切 `#m-cookie-wrap` hidden）；三列模型行（别名/真实名/上下文下拉默认 200K），保存即 `PUT /api/config` 持久化；**字段校验显示在弹窗里**（D21）——`#m-error` 红字 + `fieldError()` 高亮对应输入框（服务商/ID 空/重复、Base URL 空、Key 掩码/空、后端错误 `r.error` 都进弹窗，主界面 toast 被遮罩盖住看不到）
- **通用确认弹窗**：彩色图标圆 + 按钮配色，删除模型订阅走红色确认

**JS 关键函数**：`load()`（拉 /api/config + /api/status）、`collect()`（组装配置对象）、`save()`/`modalSave()`（PUT 持久化）、`renderProviders()`/`renderModels()`、`openDm()`/`selectDefault()`（默认模型下拉）、`toast()`/`modalError()`/`fieldError()`（主界面与弹窗两类反馈）、**`provName()`（id→服务商名称）、`presetOfProvider()`（编辑推断服务商）、`toggleCookieField()`（Cookie 输入框按服务商显隐）**（Phase 38）。

### 改 UI 必须重跑构建链

```
改 webui/index.html → python webui/build_ui.py → cargo build --release → 部署
```

build_ui.py 幂等可重复。**改过 UI 不重跑 = exe 内嵌的是旧页面**。

---

## 10. Windows 集成细节（二次开发容易踩的坑）

### 10.1 托盘
- tray-icon 0.20 **无内置默认图标**，必须 `.with_icon()`，否则 build 直接 Err
- 托盘线程必须跑 **Windows 消息泵**（GetMessageW/DispatchMessageW），否则图标不显示、菜单无响应
- `TrayIconEvent` 是 **enum**（`DoubleClick{..}` 等），无 `.event` 字段

### 10.2 停止/启动（D18）
- 状态经 `watch::Sender<bool>`：托盘线程 send、run_server borrow/await
- 每轮先 `borrow_and_update()` 丢弃陈旧唤醒，防 stop→start 快速连点误停
- async_main 保留一个 Sender clone（`_keepalive`）保活通道——托盘初始化失败时不 panic
- **停止 = 端口释放**：浏览器访问显示"服务不存在"（连接被拒），恢复只靠托盘「启动」

### 10.3 开机自启
- `RegCreateKeyExW` 等注册表函数被 **`Win32_Security` feature 门控**（SECURITY_ATTRIBUTES 参数），只开 `Win32_System_Registry` 会编译报错

### 10.4 双击隐藏黑框
- `GetConsoleWindow`/`GetConsoleProcessList` 在 `Win32::System::Console` feature
- 只 `ShowWindow(SW_HIDE)` 不 `FreeConsole`——FreeConsole 后 stdout/stderr 句柄失效，`println!` 会 panic
- HWND 判空用 `.is_null()` 而非 `== 0`

### 10.5 自重启（大坑，Phase 20）
任何"启动时检测端口占用"的逻辑都必须避开自重启用：`/api/restart` 拉起的新进程启动瞬间**旧进程还占着端口**（600ms 后才退出）→ 会误判"已有 relay"直接退出 → 服务整体被杀。修复：`webui::restart` 追加 `--restart` 标志，main 里 `!restart && port_is_busy` 才检查。

### 10.6 其他
- `taskkill //IM claude-relay.exe` 会杀掉**所有**同映像实例 → 开发期用 PID 定向 taskkill
- 命令行 `-` 开头的参数在 Git Bash 里要双斜杠（`//config` 之类）
- curl 内联中文 JSON 会被 Git Bash 打乱 → 用 `--data-binary @file`

---

## 11. 测试体系

| 层 | 命令 | 内容 |
|----|------|------|
| 单元测试 | `cargo test` | **43 个**：config 10（含 `server_auto_start_roundtrip`、`set_default_model_blocking_updates_and_saves`、`usage_stats_persist_roundtrip`）、router 8（哨兵/后缀）、proxy 8（ModelRewriter 双轨/启发式/**用量累加**）、webui 5（`balance_root_detects_deepseek_only`、`parse_coding_plan_normalizes/missing`、`console_usage_url_derivation`、`mask_cookie_keeps_tail`）、cookies 4（`v10_decrypt_roundtrip`、`v11_without_app_key_errors`、`query_cookies_reads_volcengine_rows`、`dpapi_roundtrip`）、main 1（`tray_stop_releases_port_and_start_rebinds`） |
| 端到端 | `python test/e2e.py` | **25 断言**：mock 上游 + relay 子进程（`CREATE_NO_WINDOW`），走通配置→启动→切换→对话→工具全链路；含 SSE 事件序列、认证替换、model 回写、断线重连 |

**测试技巧**：
- `#[tokio::test]` 需要 macros feature（本项目没开）→ 用手写 `Builder::new_current_thread().build().unwrap().block_on(...)`
- AppState.cfg 是 `tokio::sync::RwLock`，测试里别用 `std::sync::RwLock`
- current_thread 下 spawn 后台任务后要 `await`（sleep/轮询）让任务推进，用 `wait_until`（100×20ms 轮询 async 闭包）

---

## 12. 二次开发扩展点（怎么做）

### 12.1 新增一个服务商
**不需要改代码**——只要该服务商提供 Anthropic 兼容端点（`/v1/messages` 接受 Anthropic 格式），在 Web UI 添加即可。这是 D3 纯透传决策的直接红利。

### 12.2 新增配置字段（如给服务商加个自定义参数）
1. `config.rs` 对应结构体加字段 + `#[serde(default)]`（如需默认值不落盘：`skip_serializing_if`）
2. `webui/index.html` 的弹窗加输入项 + `collect()`/`openModal()` 读写
3. `python webui/build_ui.py` 重编译 → `cargo build --release`

### 12.3 新增 HTTP 端点
1. `api.rs` 的 `build()` 加 `.route(...)`
2. handler 放 `webui.rs`（配置/管理类）或 `api.rs`（纯查询类）
3. 取状态用 `State(state)`；读写配置用 `state.cfg.read()/write().await`

### 12.4 修改请求/响应改写逻辑
在 `proxy.rs::forward()`：改 model、改 max_tokens、加/去 header 都在这里。SSE 流内改写扩展点在 `ModelRewriter`（注意双轨：快路径字节替换 + 慢路径 JSON 递归）。

### 12.5 接入非 Anthropic 协议的服务商
已有两个模板：`translate.rs`（D36，OpenAI Chat Completions）与 `gemini.rs`（D56，Gemini generateContent）。新协议照抄三件套结构：
1. `xxx.rs`：请求体翻译 + 非流式响应翻译 + 流式 Translator（`push`/`finish` 接口，复用 `proxy::UsageSink`/`accumulate_usage` 提取用量）
2. `proxy.rs::forward()` 加 `is_xxx` 分支：URL 拼接 / 认证头 / 剥头 / body 翻译 / SSE 包装 / 错误翻译
3. `webui/index.html` 的 `m-protocol` select 加选项（可选：PRESETS 加预设自动填）
注意与 D3 纯透传哲学的取舍：翻译层只在上游确实无 Anthropic 兼容端点时才加。

### 12.6 新增 CLI 参数
`main.rs::Args` 加字段 + `parse_args()` 匹配 → `async_main` 使用。

### 12.7 改托盘行为
`tray.rs`：菜单项、图标、双击动作都在这里。停止/启动语义由 watch 通道驱动 `run_server`。

### 12.8 改 UI 样式
- 按钮/通用组件 → `<style id="ui">`（不会被 build_ui.py 覆盖）
- 布局/间距 → 直接改 index.html 的 Tailwind 类（重编译后生效）
- 新图标 → `S` 映射表 + `svg()` 帮助函数（Feather 风格）

---

## 13. 已知问题与未竟事项

- **CC ~1MB 大请求谜题**：CC 大请求偶发静默 400/挂起。proxy.rs 已埋诊断日志（body parse 失败/无 model/model 解析失败三处），排除 relay 大 body 问题（1.2MB 合成请求正常），待复现定位
- **可选后续**：给 ds 配别名体验 `/model ds-flash`、接入 Kimi/GLM/Anthropic 官方服务商
- **当前限制**：仅 Windows 完整支持（托盘/自启/隐藏黑框）；非 Windows 编译可通过但托盘/自启为 no-op

---

## 14. 给二开者的建议

1. **先读 `findings.md`**：记录了每个踩过的坑（serde 省写、watch 陈旧唤醒、tray 前置条件、注册表 feature 门控、自重启误判等），能省大量调试时间
2. **守住三个硬约束**（§1）：加依赖前掂量体积（如加 winreg 不如用已有的 windows-sys）
3. **测试先跑一遍**：`cargo test` + `python test/e2e.py` 建立基线，改完再跑
4. **改 UI 记得跑 build_ui.py**：不然部署后页面是旧的
5. **config.json / claude-settings.json 含真实 Key**：改代码、写文档、跑测试都不要把它们提交进 git

## OpenCode Go 新控制台用量与显示（2026-10-20）
- console_usage_url 填 https://opencode.ai/console/{工作区ID}/go，配置中保留页面地址。新版页面为 SPA，后端请求同域 /console/api/go/status，附 x-org-id、页面 Referer 和存档 Cookie；查询与 Cookie 续期共用请求头逻辑。
- access.meters.fiveHour/week/month 中 usedMicroCents 与 limitMicroCents 计算百分比，限制最高 100%，四舍五入至两位小数；resetsAt 为 UTC ISO 时间或 null。旧 HTML 页面解析兼容。
- 页面用 toFixed(2)，桌面卡片用 .2 格式固定显示两位小数（如 0.10%）。
- OpenAI 流忽略空 content 占位，防止关闭尚在接收参数的工具块；thinking 块补齐空 signature 字段。
- test/probe_tool_stream.py 可读取指定配置做最小工具探测，仅输出分片元数据，不输出凭据或参数内容。
- 本机旧 TDM GCC 链接器需使用 Rust 自带 rust-lld：复制为 target/build-support/ld.exe，COMPILER_PATH/LIBRARY_PATH 指向该目录，放置空 libgcc_eh.a；gcc -dumpspecs 去掉 --exclude-libs=libpthread.a 后存 gcc.specs；cargo rustc 最终目标带 -C link-arg=-specs=该绝对路径。
