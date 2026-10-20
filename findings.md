# Findings — 调研与关键结论

> 本文件仅存放外部调研数据与中间结论，不存放执行指令。

## 1. 环境现状 (2026-08-03)
- 工作目录 `D:\Tools\ClaudeModelSwitch` 为空，全新项目。
- 平台：Windows 10 Pro x64。
- Rust 工具链**已安装**：rustup 1.29 + stable **1.97.1**。
  - **MSVC 链接器缺失**（VS 2022 目录为空壳，无 VC Tools/vswhere/link.exe）→ **改用 `x86_64-pc-windows-gnu` 工具链**（D1 偏离，见下）。
  - 系统有 **TDM-GCC-64 10.3**（含 gcc/ld/ar，PATH 里 `/c/TDM-GCC-64/bin`）。
- **TDM-GCC 链接坑**：缺 `libgcc_eh.a` → rustc 链接报 `cannot find -lgcc_eh`。
  - **workaround（已实测通过）**：项目本地 `.link/libgcc_eh.a`（`ar rcs` 空归档）+ `.cargo/config.toml` 里
    `rustflags = ["-C", "link-arg=-L<绝对路径或相对 .link>"]`。空归档对纯 Rust + panic=abort 无害。
  - 伴随一个无害链接警告 `corrupt .drectve at end of def file`（空归档无 .drectve 段）。
  - 备选（未采用）：装 VS Build Tools 2-3GB 需管理员；`-Z unstable-options` gnu-lld 需 nightly。
- 目标产物 `x86_64-pc-windows-gnu`（GNU 工具链产物与 MSVC 体积差异 <10%，满足要求）。

## 2. Claude Code 中转接入机制（调研结论，2026-08-03）
> 来源：调研代理基于训练知识 + claude-code-router / LiteLLM / claude-code-proxy 等社区事实标准 + DeepSeek/GLM 官方 Anthropic 兼容文档。部分项版本相关，以首次真机联调为准。

### 2.1 模型名
| 项 | 结论 | 可信度 |
|---|---|---|
| `ANTHROPIC_MODEL=<任意字符串>` | **接受并原样透传**，不在客户端校验。社区标准做法。 | 确定 |
| `claude --model <任意名>` | 接受任意字符串。 | 确定 |
| 交互式 `/model` 输入未知名 | **2026-08-03 真机实测（用户环境）**：接受自定义任意名，三次切换（v4-flash→ds-deepseek-chat→ds-deepseek-v4-pro）全 200，零重启。**版本相关**，本机确认可用；旧版仍以 env var / `--model` 兜底。 | 实测 |
| `ANTHROPIC_DEFAULT_OPUS/SONNET/HAIKU_MODEL` | 把 CC 内部按任务选的 opus/sonnet/haiku 槽位映射到自定义模型名（如 `ds:deepseek-chat`）。比 `/model` 可靠。旧名 `ANTHROPIC_SMALL_FAST_MODEL` 为早期 haiku 槽位。 | 文档推测 |

> ⚠️ 设计影响：用户想要的「在 CC 里 `/model` 切换」能否任意输入自定义名需在 Phase 1 联调时验证。**中继本身不改**（只读 body.model 路由），但需备好规避路径：`--model` 参数、`ANTHROPIC_MODEL`、按任务槽位 env var。中继加 `--debug` 请求日志辅助验证。

### 2.2 端点
| 端点 | 是否被 CC 调用 | 是否必需 | 实现 |
|---|---|---|---|
| `POST {base}/v1/messages` | 主调用 | **必需** | 翻译/透传+流式 |
| `POST {base}/v1/messages/count_tokens` | 会调用 | 非必需，但建议实现 | Anthropic 透传 / openai 启发式 `输入字符数/4` |
| `GET {base}/v1/models` | 不确定(新版可能拉取) | 成本极低，实现作保险 | 聚合返回 `{"data":[{"id":...}]}` |
| `POST /v1/complete`、`/v1/messages/batches` | 不用 | 不需要 | — |
- 路径拼接：CC 把 `ANTHROPIC_BASE_URL` 作前缀直接拼 `/v1/messages`。
- 请求头：恒带 `anthropic-version: 2023-06-01` + `x-stainless-*`；常带一个或多个 `anthropic-beta` 头 → **转发给 OpenAI 上游时剥离**（避免被上游拒绝）。

### 2.3 认证头
| env | 发送头 |
|---|---|
| `ANTHROPIC_API_KEY` | `x-api-key` |
| `ANTHROPIC_AUTH_TOKEN` | `Authorization: Bearer` |
| 两者同设 | 两个头都带 → 中继两个都读（先 Authorization 再 x-api-key），但上游鉴权一律用配置的 provider key |
| `ANTHROPIC_CUSTOM_HEADERS` | JSON 字符串对象，合并进每个请求；可用于传自定义鉴权信息 |

### 2.4 流式（核心）
- CC **交互场景始终 `stream:true`**。期望 Anthropic 事件序列：`message_start` → `content_block_start` → `content_block_delta`(text_delta/input_json_delta/thinking_delta) → `content_block_stop` → `message_delta`(stop_reason+usage) → `message_stop`，另有 `ping` 保活。
- **v2 设计（用户决策：全服务商 Anthropic 协议）→ 中继对 SSE 字节级透传，不做任何转换**。服务商返回的
  就是 Anthropic 事件序列，CC 原生解析。✓ 方案 D7。
- 若将来接入纯 OpenAI 协议服务商，才需要把 OpenAI chunk 转成 Anthropic 事件序列（届时再实现 translate 模块）。

### 2.6b SSE model 回写的格式兼容（Phase 1 实测发现）
- Anthropic 官方/多数服务商用**紧凑 JSON** `"model":"x"`（无空格）→ 字节级替换即可，零开销。
- Python json.dumps 等会带空格 `"model": "x"`，且 SSE 里 model 常**嵌套**在 `message` 对象内层。
- 最终实现双轨：① 紧凑字节快路径；② 慢路径把 `data:` 载荷解析成 JSON、**递归改写任意层** model 字段后重序列化。
- 注：慢路径重序列化会把 key 按字典序排（serde_json 默认 BTreeMap），CC 按 JSON 语义解析，顺序无关紧要。
- 教训：mock 上游测试比真实服务商更能暴露格式健壮性问题——验收时两种格式都测。

### 2.7 模型名解析规则（`-` 分隔，v2 新增）
- 命名 `{服务商id}-{模型}`，例：`ds-deepseek-chat`、`kb-moonshot-v1-32k`。
- ⚠️ 模型名本身常含 `-`（deepseek-chat、gpt-4o），**不能按第一个 `-` 简单切分**。
  正确做法：对**已配置的服务商 id 集合**做最长前缀匹配（如 `ds` 命中 `ds-deepseek-chat` → 真实模型 `deepseek-chat`）。
- 建议服务商 id 互相避免"一个 id 是另一个 id 的前缀"（如 `ds` 与 `dsx`），降低歧义；最长匹配兜底。
- 别名全集可从配置推导并暴露在 `GET /v1/models`。

### 2.8 Claude Code 环境变量优先级（2026-08-03，调研确认）
- **settings.json 的 `env` 块覆盖父 shell 继承的环境变量**（对同名变量做覆盖写，非兜底）。
  官方：`code.claude.com/docs/en/settings`（env 分层合并）、`model-config`（模型优先级）。
- ⚠️ 设计影响：claude.cmd 里 `set ANTHROPIC_BASE_URL=...` 会被 settings.json env 里的同名项覆盖。
  要进中转必须先消除冲突：settings.json env 指向 relay（`http://127.0.0.1:8787`）或删掉 ANTHROPIC_*。
- 无 `--base-url` CLI 参数；`claude --model` 优先级高于 env（但 settings env 的 MODEL 仍会赢）；
  `claude --settings user,project` 可跳过 user 层（版本相关，勿盲信）。
- **`claude --settings <file>` 实测（2026-08-03，D14）**：传 settings 文件路径时，该文件 env **优先于** user settings.json env。
  验证方法：`claude --settings <副本> --print "你好"` → relay 日志确认请求穿过。据此做 **settings 隔离**：
  副本（claude-settings.json）env 指向 relay，主 settings.json 保持日常直连不动。

### 2.9 Windows 批处理中文编码（2026-08-03，Phase 3 实测踩坑）
- cmd.exe 用系统 ANSI 代码页（中文系统=GBK/936）解析批处理，**UTF-8 批处理文件必然乱码断行**。
- GBK 文件在 Git Bash（console 65001）里仍乱；UTF-8+`chcp 65001` 从 Git Bash 调起仍乱（cmd 缓冲解析时序）。
- **最终：批处理一律纯 ASCII**（本项目 claude.cmd 全英文输出），零代码页敏感性，双击/cmd/PowerShell/Git Bash 全兼容。
- 另两个 cmd 陷阱：`if ()` 块内 echo 文案里**不能出现 `(` `)`**（会破坏块解析 → "此时不应有"）；
  `for /f` 反引号命令**首 token 若是带引号的 exe 路径**会报错 → 前置 `""` 转义（`in (\`""%RELAY%" ...\`)`）。
- **命名陷阱（Phase 3 真机抓到）**：批处理**不能命名为与内部调用的命令同名**——`claude.cmd` 内部执行 `claude` 时，
  cmd **当前目录优先**解析到脚本自身 → 无限自我调用 → `已经达到最大的 setlocal 递归层` 爆栈。
  改名 `start-claude.cmd` 即解（`where claude` 恢复指向真实 `claude.exe`）。

### 2.5 请求字段与转换陷阱（社区验证；v2 纯透传下多数不适用，仅备将来接 OpenAI 协议服务商用）
| 陷阱 | 处理 |
|---|---|
| `max_tokens` **恒发送且偏大**(32000~64000) | **必须按上游上限 clamp**（DeepSeek 等上限 8192）。服务商配置加可选 `max_tokens` 上限 |
| `system` 新版是**数组** `[{type:text,text,cache_control}]` | 拍平成字符串并**剥离 cache_control**（很多 OpenAI 上游不接受数组/cache_control） |
| `tools` 含大量工具定义+`cache_control` | 完整映射 `input_schema`→`parameters`，**剥离 cache_control** |
| `thinking` 参数(开/关) | 转发给 OpenAI 上游前**剥离** |
| `temperature`/`tool_choice` | **不假设存在**，缺省容错 |
| `stop_reason` 映射 | `end_turn↔stop`、`max_tokens↔length`、`tool_use↔tool_calls`、`stop_sequence↔stop` |
| OpenAI `delta.tool_calls` 分片到达 | 按 index 缓冲 id/name/arguments，name 到齐发 `content_block_start(tool_use)`，arguments 以 `input_json_delta` 发出 |
| 空闲长连接 | 发 `ping` 保活 |

### 2.6 接入路径（v2：全 Anthropic 协议）
- **用户决策：所有服务商都提供 Anthropic 协议的 base url** → 中继**纯透传**，零翻译。
- 已验证案例：**DeepSeek** 官方 Anthropic 兼容端点 `https://api.deepseek.com/anthropic`；**GLM/Zhipu** 社区证实有。
- 每个服务商配置 `base_url` = 其 Anthropic 兼容端点根（中继拼接 `/v1/messages` 转发）。
- 注意：若某服务商实际没有 Anthropic 端点，联调时会 404/报错 → 届时再考虑加 OpenAI 翻译模块（v2 不内置）。

### 2.10b 开发期进程管理（Phase 5，2026-08-03）
- **`taskkill //IM <image>` 会杀掉所有同名进程** —— 开发期 relay 在跑、又要重启时，误杀用户正在用的实例会瞬间断链。
  中继无状态所以重启即恢复（CC 无需重启），但仍是事故。**改用 `taskkill //F //PID <pid>` 定向杀，或先 `netstat -ano | grep :8787` 取 PID。**
- 验证 Web UI 内嵌页的套路：`cargo check`（include_str 路径）→ 抽取 `<script>` 存成 `.js` 后 `node --check`（node 不认 `.tmp` 扩展名）→ 临时端口实例 `curl /` 检查新标记 → 回归 e2e.py。

### 2.11 Web UI 工程（Phase 6，2026-08-03，Tailwind 重构）
- **`hidden` 属性陷阱（Bug 根因）**：`[hidden]` 的 UA 样式 `display:none` 特异性 (0,1,0)，一旦元素自己的类也设 `display`（如 `.overlay{display:flex}`，同为 0,1,0），作者样式压过 UA → hidden 完全失效（加载即显、关闭无效）。**修复：`@layer base{ [hidden]{display:none!important} }`**。Tailwind v4 preflight 自带 `[hidden]:where(:not([hidden="until-found"])){display:none!important}`，双保险。
- **Tailwind 编译内嵌工作流（用户选定：自包含/离线）**：源码 `webui/index.html` 含 `<style id="tw">/*__TAILWIND_CSS__*/</style>` 占位；`webui/tailwind/in.css` 用 `@import "tailwindcss"` + `@source "../index.html"` 显式限定扫描范围（避免把已注入的编译产物再扫一遍造成反馈环）；`webui/build_ui.py` 每次先**重置占位块**再编译，保证扫描干净源码 → **幂等**（重复运行产物一致）。编译产物 ~15KB，exe 2.14→2.16MB。
- **构建命令**（改 UI 后三连）：`python webui/build_ui.py` → `cargo build --release` → PID 定向重启 relay。
- **保留 `.chip` 标记类**：Tailwind 重构时 JS 有 `querySelectorAll('.chip')` 选择器，故动态生成的 chip span 同时带 Tailwind 类 + `.chip`（仅作选择器标记，无样式），避免改 JS 选择器。
- 环境：node v22.7 / npm 10.8（Tailwind v4.3.3 已装于 webui/tailwind）；`npx` 在 Python subprocess 里须用 `npx.cmd`（Windows）。

### 2.12 Web UI 保存语义 + 自重启（Phase 7，2026-08-03）
- **「局部保存不落盘」陷阱（删除模型刷新后恢复的根因）**：弹窗「保存服务商」只更新内存数组，真正落盘要靠底部「保存配置」。用户点完弹窗保存就以为持久化了 → 刷新读回磁盘旧配置。**修复：局部保存按钮改为立即 `PUT /api/config` 持久化（成功才关弹窗）**。设计原则：对用户来说「保存」就该落盘，别搞「先内存后磁盘」两步。
- **Rust 进程自重启实现**：`Command::new(current_exe()).args(env::args().skip(1)).spawn()` 拉起同参数新进程（子进程继承父进程控制台与 stdout/stderr 句柄 → 日志文件继续沿用、无新窗口）→ 返回响应 → **600ms 后 `std::process::exit(0)`** 让出端口（响应先发出去）。新进程靠 main.rs **`bind_with_retry`（20×150ms）** 扛过旧进程未退出瞬间。Windows 实测：POST /api/restart 返回 `{ok,restarting}`，PID 变化、端口复用、healthz 恢复。
- **坑（编译期）**：给函数加 `await` 必须把签名改为 `async function`；node --check 会精确报 `await is only valid in async functions`。

### 2.13 模型别名 + 默认模型 + 更新 claude settings（Phase 8，2026-08-03）
- **模型别名格式（向后兼容）**：`models` 元素既可以是字符串 `"deepseek-chat"`（别名==真实名），也可以是 `{ "alias": "flash", "real": "deepseek-v4-flash" }`。序列化时别名==真实名写字符串，否则写对象 → 旧配置零改动，e2e 的 `["mock-chat"]` 直接兼容。`resolve()` 后缀先查别名 → 命中发真实名；未命中别名则后缀直接当真实名（`ds-flash` 与 `ds-deepseek-v4-flash` 都能用）。`/v1/models` 列出 `{id}-{别名}`。
- **默认模型**：`server.default_model` 存完整别名；`--print-default-model`/copyEnv/更新 settings 优先用它，空则回退第一个服务商第一个模型别名。UI 从 `/api/status` 的 models 填充下拉。
- **更新 ~/.claude/settings.json 的稳妥做法**：尊重 `CLAUDE_CONFIG_DIR`（CC 会用它覆盖默认 `~/.claude`）；用 serde_json::Value 读文件 → 只对 `env` 对象 `insert` 3 个 ANTHROPIC_* 键 → `to_string_pretty` 写回 → 其余键（theme/statusLine/插件/model）全部保留。**不要整体覆盖文件**（会丢掉用户其它设置）。
- **坑（编译期）**：PathBuf 不实现 Display，`format!("{path}...")` 报 E0277，必须 `path.display()`。
- **坑（UI）**：配置里 models 可能是字符串或对象混合 → JS 统一 `modelAliasOf/modelRealOf/fmtModel` 归一化；chip 用 `data-alias/data-real` 携带，modalSave 按对象提取。

### 2.14 上下文大小 + 三列模型列表 + 移除 max_tokens（Phase 9，2026-08-03）
- **ModelEntry 加 context（元数据，仅 UI 展示）**：默认 200000。serde 三态：字符串 `"x"` → alias=real、context=默认；对象 `{alias,real,context?}` → context 省略则默认；序列化 alias==real && context==默认 → 字符串（旧配置零改动、`["mock-chat"]` 仍写回字符串），否则对象且 context 非默认才写出。UI 用 `modelCtxOf(m)`（字符串→200000）归一化。
- **移除 max_tokens**：Phase 9 删除 `ProviderConfig.max_tokens` + proxy.rs clamp 块。serde 对未知字段默认忽略 → 旧配置残留 `max_tokens` 字段也能正常解析（不会报错，只是不生效）。e2e 断言从「clamp 32000→1024」改为「max_tokens 原样透传 32000」，25 项数量不变。
- **/api/status 新增 `model_details`**：`[{provider, alias, real, context, full}]`，`full`=`{id}{sep}{alias}`。前端默认模型下拉与已注册模型区都消费它；`models`（平铺别名列表）保留向后兼容。
- **Tailwind v4 任意值类检查陷阱**：编译产物里任意值类名带转义（`.grid-cols-\[minmax\(0\,1fr\)_...\]`），用子串直查会误报 MISS，须按转义形式比对（或直接看 CSS 里是否含 `grid-template-columns:minmax(0,1fr)` 规则体）。`self-start` 未在 HTML 使用故未编译，属正常。
- **JS 坑（数组去尾）**：`(p.models||[]).length ? p.models : [null]` 用 `[null]` 占位让空模型服务商也渲染一行空行（`m===null` → `modelRow('','',200000)`），否则无 + 按钮提示用户先加行。
- **可搜索自定义下拉的关闭策略**：`document.click` 监听 `e.target.closest('#dm')` 判断外点关闭；容器内点击（按钮 toggle、搜索输入、选项）都不误关。Escape 单独监听。

### 2.15 按钮组件化 + 确认弹窗重设计（Phase 10，2026-08-03）
- **Tailwind 之外的手写组件 CSS 可行**：build_ui.py 的 `reset_marker` 只重置 `<style id="tw">` 占位块，页面里**另外的 `<style id="ui">` 块原样保留** → 可在 Tailwind 之上叠加手写组件层（`.btn`、`.ic` 等），免去在 HTML 里重复写一长串 Tailwind 类。每次改按钮样式只改一个 CSS 块。
- **按钮视觉公式**：`linear-gradient(180deg, 亮, 暗)` + `inset 0 1px 0 rgba(255,255,255,.22)` 上缘高光 + `0 1px 2px` 底阴影 + hover `filter:brightness(1.06)` + `box-shadow` 加深 + active `transform:scale(.96)` + `:focus-visible` 双层 ring（白底+暗环，避免 currentColor 白叠白）。`filter` 而非 hover 换色 → 渐变不用写 hover 变体。
- **SVG 图标体积可控**：Feather 线性图标每个 ~150B，5 个顶部按钮 + 若干动态按钮共 ~1.5KB，release exe 仅 +8KB（2.314MB）。动态按钮用 `svg(S.trash)` 注入 innerHTML。
- **删除类确认弹窗的默认感知**：确认弹窗加彩色图标圆（red/orange=⚠warn、blue/indigo=ⓘinfo、green=✓check），危险操作默认红色圆 + `.btn-red` 确认钮，用户第一眼即知是破坏性操作。
- **删除不持久化是隐形 bug**：服务商列表「删除」曾用原生 `confirm()` + 仅内存 splice（不 PUT）→ 刷新后恢复。凡改动 providers 必须落盘（PUT /api/config）。Phase 10 已修：confirmAction 确认 → splice → 若默认模型失效则清空 → PUT。另拦截「删除最后一个服务商」（validate 不允许空 providers）。
- **顺手教训**：删旧按钮 Tailwind 色类后，Tailwind 编译体积从 16119B 降到 14708B——**类从 HTML 移除后不再生成，属预期**，不是丢样式。

### 2.16 顶部工具栏单行 + 列表单行截断（Phase 12，2026-08-03）
- **工具栏 5 按钮单行的宽度预算**：容器 `max-w-3xl`(768px) − 2×px-4 − 2×px-3.5 ≈ 708px。按钮 12px 字号（中文 ≈12px/字）下 5 按钮文字+图标(13px)+gap(6px)+padding(2×0.7rem) 合计 ≈637px，加 4×8px gap ≈669px < 708px → **12px + padding .45rem .7rem 的 `.btn-bar` 紧凑变体能单行放下**。容器仍加 `flex-nowrap overflow-x-auto` 兜底，窄窗口横向滚动而非换行。
- **中文 ellipsis 截断**：Tailwind `truncate`（overflow:hidden+nowrap+text-overflow:ellipsis）对长文本摘要可直接用，配合 `title` 属性悬停看全文；不要用 `break-all`（会换行堆叠）。注意 truncate 要求元素是 block 且有确定宽度——父级 `flex-1 min-w-[180px]` 已满足。
- **按钮一致性套路**：凡「新增/添加」类按钮统一 `btn btn-solid btn-green`（添加服务商、添加模型），破坏类统一 `btn-danger-ghost`/`.btn-red` 确认，次要类 `btn-ghost`——用户扫一眼颜色即知动作属性。

### 2.17 隐藏元素的两套机制不能混用（Phase 14 bug，2026-08-03）
- **`.hidden` class vs `hidden` 属性是两套独立机制**：Tailwind `.hidden{display:none}` 绑定 class；`[hidden]{display:none!important}` 绑定属性。JS 若切属性（`el.hidden=false`）但元素 class 里残留 `hidden` → class 仍强制 display:none → **永远显示不出来**。
- **坑例**：`#dm-drop` 曾写成 `class="hidden absolute ..."`，openDm 只清属性 → 下拉永远不可见，用户报「默认模型不能选择」。修复：改成 `class="absolute ..."` + `hidden` 属性。
- **本项目约定**：所有 JS 切换显隐的元素统一用 **`hidden` 属性**（三个 overlay + dm-drop 都是）；`.hidden` class 只在纯 CSS 场景用。排查「控件点了没反应」先看是不是 class 里混了 `hidden`。

### 2.18 tray-icon 托盘两大致命前提（Phase 16，2026-08-03）
- **必须有图标**：tray-icon 0.20 无内置默认图标，`TrayIconBuilder::new().build()` 不传 `.with_icon()` 直接 Err → 托盘完全不出现。无外部 .ico 时用 `Icon::from_rgba(vec![r,g,b,a; N], w, h)` 程序化生成（需 w*h*4==len，正方形）。本项目 32×32 画绿圆（`dx*dx+dy*dy <= r*r`）+ 白色闪电多边形（射线法 `in_poly`）。
- **必须有 Windows 消息泵**：托盘隐藏窗口靠 GetMessage/DispatchMessage 处理 WM_TRAYICON（图标事件）/WM_COMMAND（菜单事件）。只 `MenuEvent::receiver().recv()` 阻塞、从不泵消息 → 图标不显示、右键无菜单。标准写法：托盘线程 `GetMessageW(&mut msg, null, 0, 0)` 阻塞循环 + `DispatchMessageW` 后 `try_recv` 消费 MenuEvent/TrayIconEvent 通道。依赖 `windows-sys`（Win32_UI_WindowsAndMessaging），链接自动，体积增量可忽略。
- **API 坑**：tray-icon 0.20 的 `TrayIconEvent` 是 **enum**（`Click{..}`/`DoubleClick{..}`/`Enter`/`Move`/`Leave`），不是带 `.event` 字段的 struct，也没有 `TrayIconEventKind` 导出 → `matches!(ev, TrayIconEvent::DoubleClick{..})`。菜单项改文字用 muda `MenuItem::set_text`。
- **暂停/停止语义**：托盘「停止服务」不应退出进程（否则无法再运行），用共享 `Arc<AtomicBool>` paused，proxy `forward` 顶部检查返回 503；Web UI / healthz 保持可用。`/api/status` 加 `paused` 供 UI 显示。
- **`#[tokio::test]` 依赖 macros feature**：本项目 tokio 只开 rt/net/io-util/sync/time，测试用 `tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(...)` 手写；AppState.cfg 是 tokio RwLock，别用 std::sync::RwLock（E0308）。

### 2.19 停止服务的语义：整站 503 守卫（Phase 17，2026-08-03）
- **用户对「停止」的心理模型 = 整站不可访问**，不只是转发暂停。用 axum 中间件 `middleware::from_fn_with_state(state, paused_guard)` 挂在 `Router::build()` 最外层：paused 时所有路由（`/`、`/healthz`、`/api/*`、`/v1/*`）统一 503。比在每个 handler 加检查干净得多。
- **恢复路径必须在 HTTP 之外**：整站 503 后浏览器无法再操作，唯一恢复点是托盘「启动」——托盘是独立 std 线程 + 共享 `Arc<AtomicBool>`，天然不受 HTTP 影响，这是「停止后还能恢复」的关键前提。proxy 内保留 503 早返回是为了单测直接覆盖 `forward`，与中间件并存（防御纵深）。
- **测试技巧**：`Router::oneshot` 来自 `tower::util::ServiceExt`，需 dev-dependency `tower=0.5 features=["util"]`（只加 dev-dep，不污染 release 体积）。`Router` 可 Clone，一次 build 多次 oneshot 逐路径断言 503。
- **菜单文案**：用户要「只显示停止、启动两个字」——菜单项短文案（2 字）比「停止服务」更贴合托盘习惯；`set_text` 切换保持简洁。
- ⚠️ **Phase 18 已被取代**：用户要求「停止 = 浏览器显示服务不存在」，503 方案改为**释放端口真停服务**（见 §2.20）。

### 2.20 停止的真语义：释放端口停掉整个服务（Phase 18，2026-08-03）
- **用户心理模型**：停止后浏览器访问**必须显示服务不存在**（连接被拒），不是 503 页面。503 方案（§2.19）端口仍开着，只是返回错误——不够「停」。
- **核心机制**：main.rs 抽出 `run_server` 无限循环，托盘 watch 信号直接驱动：
  - `paused=false` → `bind_with_retry` + `axum::serve`，`tokio::select!` 同时监听停止信号；
  - `paused=true` → 等「启动」信号后重新 bind。
  - **停止 = select 的 serve future 被 drop → Serve 拥有 TcpListener → 监听器释放、端口关闭**。这是关键：`axum::serve(listener, app)` 返回的 Serve future 拥有 listener，drop 即释放端口。
- **状态承载用 `tokio::sync::watch::Sender<bool>` 而非 Arc<AtomicBool>`**：托盘线程（非 tokio 上下文）`send()` 合法（watch 的 send/borrow 无需 runtime，只动原子量+waker）；服务循环 `borrow()/changed()`。一个通道同时承载「当前状态」+「变化通知」，比原子量+Notify 双结构干净。
- **watch 陈旧唤醒陷阱**：`notify_one` 型信号存在「快速 stop→start 连点时旧版本被新注册的 changed() 误消费 → 误停一次」。解法：**每轮先 `borrow_and_update()` 丢弃陈旧版本再读状态**，changed() 之后只会看到真正的新唤醒。
- **通道保活**：托盘线程若初始化失败会 return、Sender drop；watch 全 Sender drop 后 `Receiver::borrow()` 会 **panic**。解法：async_main 作用域内保留一个 Sender clone（`_keepalive`），通道永不关闭——托盘失败仍不致命（与历史行为一致）。changed() 的 Err 分支仍加 exit(0) 兜底。
- **绑定失败双策略**：首次启动失败 `exit(1)`（立即暴露）；托盘重启路径失败 1s 重试（菜单「停止」=准确，用户可点停止放弃），不 force-stop 避免菜单文案失同步。
- **测试**：`main::tests::tray_stop_releases_port_and_start_rebinds` 在真实临时端口上 spawn `run_server`（与托盘路径同一循环），`send(true)` 后轮询确认连接被拒、`send(false)` 后恢复。比 mock 强得多。current_thread 下 `tokio::spawn` + `wait_until`（100×20ms 轮询 async 闭包）让后台任务与断言交错。

### 2.21 开机自启 + 双击隐藏黑框 + 打开配置页（Phase 19，2026-08-03）
- **开机自启注册表**：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 值 `ClaudeRelay` = `"<exe绝对路径>" --autostart`（HKCU 免管理员）。注册值带 `--autostart`（后台语义）。删除方向走 UI 保存（config_put `apply(false)`）；**启动只注册不删除**——否则 e2e/mock 配置启动（auto_start 缺省 false）会误删用户已启用的注册表项。
- **windows-sys 坑**：`RegCreateKeyExW`/`RegSetValueExW` 等注册表函数被 **`Win32_Security` feature 门控**（lpsecurityattributes 是 `*const SECURITY_ATTRIBUTES`），只开 `Win32_System_Registry` 会 E0432。`GetConsoleWindow`/`GetConsoleProcessList` 在 `Win32::System::Console`；`ShowWindow`/`SW_HIDE` 在 `Win32::UI::WindowsAndMessaging`。HWND 是 `*mut c_void`，判空用 `.is_null()` 不是 `==0`。
- **双击 vs 终端判定（隐藏黑框）**：`GetConsoleProcessList` 返回 n==1 = 独占该控制台（Explorer 双击、`start` 新建控制台）→ `ShowWindow(SW_HIDE)`；n>=2 = 共享父终端（cmd/PowerShell）→ 保留日志。**只 SW_HIDE 不 FreeConsole**——FreeConsole 后 stdout/stderr 句柄失效，后续 println!/eprintln! 会 panic；隐藏控制台仍接受写入，无害。e2e 用 `CREATE_NO_WINDOW` 启动 → GetConsoleWindow()==0 → 跳过隐藏，输出照常进管道。
- **双击打开配置页的三种情境**：`gui_launch && !autostart`（双击）→ 首次 bind 成功后 open_browser；`port_is_busy`（500ms TCP connect 探测）已运行 → 双击开配置页后 `return` 退出、`--autostart` 静默退出。start-claude.cmd 的 `start /min` 新建控制台（n==1）→ 控制台被隐藏 + 因加了 `--autostart` 不弹浏览器。
- **config 路径回退**：`--config` 未指定时 CWD/config.json，不存在回退 exe 同目录——开机自启 CWD=System32 也能加载配置（否则会在 System32 生成空默认配置 → validate 失败 exit）。
- **serde 省写**：`#[serde(default)]` 只影响反序列化；要「默认 false 不写盘」需 `#[serde(default, skip_serializing_if="is_false")]`。

### 2.22 保存并重启杀程序的回归根因 + toast 布局（Phase 20，2026-08-03）
- **回归根因（忙端口误判杀服务）**：/api/restart 时序 = 旧进程 spawn 新进程（同参数）→ 返回响应 → 600ms 后旧进程 exit。Phase 19 加的 `port_is_busy`（启动时探测端口）在重启场景**必中**：新进程启动瞬间旧进程还占着端口 → 误判"已有 relay 在运行"→ 新进程退出；旧进程随后也退出 → **服务整体被杀**。任何"启动时检测端口占用"的逻辑都必须避开自重启用——`webui::restart` spawn 追加 **`--restart`** 标志，main 里 `!restart && port_is_busy` 才检查，restart 时直接交给 `bind_with_retry`（20×150ms）扛过新旧交接。
- **顺带**：parse_args 参数太多（6→7 元组）改为 **`Args` 结构体**——元组返回极易漏传参数（E0061），结构体字段名自文档化。
- **toast 挤压**：反馈元素若在按钮 flex 行内（`ml-auto`），出现时会挤压按钮。改为卡片内按钮行**下方独立行**，`min-h-[18px]` 保底（空时不塌陷、出现时不跳动）。

### 2.23 CC 上下文后缀 + 哨兵模型（Phase 22，2026-08-04）
- **`[1m]`/`[200k]` 上下文后缀会原样到达中继**：CC 把模型串（含后缀）原样发给 relay。证据：claude-code-guide 子代理试图用 `claude-haiku-4-5-20251001` 时，relay 日志/响应 400 "no provider prefix matches" —— 说明 CC 透传任意模型串。`[ctx]` 后缀是中继需自行剥离的信号，不是上游模型名的一部分。
- **settings.json 顶层 `model` 键优先于 `env.ANTHROPIC_MODEL`**：实测会话运行模型 = 顶层 `model: deepseek-v4-flash[1m]`，而 env 里是 `ds-deepseek-v4-flash`。→ **改 CC 实际用模型必须写顶层 `model` 键**（Phase 22 的 update_settings 两个都写，用户确认）。
- **哨兵设计（Phase 22）**：CC 配 `model: claude-relay-model[上下文]` → 中继 `resolve_for_request()` 剥 `[ctx]` → base==`claude-relay-model` → 用配置「当前使用模型」（`server.default_model`，空回退第一个别名）替换 → 正常前缀路由。CC 设具体模型（如 `/model ds-v4-flash`）则剥后缀后直接路由。配置页切「当前使用模型」即时生效，无需改 CC。
- **剥后缀后上游发干净模型名是安全的**：真机 DeepSeek 实测——中继把 `deepseek-v4-flash[1m]` 剥成 `deepseek-v4-flash` 转发，count_tokens 200。`[1m]` 只是 CC 侧上下文窗口提示，DeepSeek 端点忽略它。真需要带后缀的变体时，在 `real` 里写（如 `{alias:"v4-flash-1m", real:"deepseek-v4-flash[1m]"}`）。
- **上下文后缀格式**（CC 约定，与后端 `fmt_ctx_suffix` 一致）：1e6 整倍→`[1m]`，千整倍→`[200k]`，否则四舍五入 k。200k 默认 context → `claude-relay-model[200k]`。
- **环境现状（2026-08-04）**：8787 无 relay 进程；live relay 的配置（alias `ds-v4-flash`/`hs-*`/`xf-*`，3 服务商）**不在本项目**（项目 config.json 只有 ds，models 为 `deepseek-v4-flash` 字符串）。用户实际部署的 relay 在项目外，重启需用户自行操作。

### 2.24 哨兵功能的两个现场坑（Phase 23，2026-08-04）
- **下拉选中≠生效，必须持久化才热重载**：`selectDefault` 曾只改内存 `defaultModel` 变量，不 PUT——用户改了「当前使用模型」没点「保存配置」→ relay 仍用旧 `server.default_model`（本次 8787 生产 relay default_model 实测为 `xf-glm-5.1`）。**修复：选中即 PUT /api/config**（与 D16「弹窗保存即持久化」同一原则：对用户来说「选中/保存」就该落盘生效，别搞「先内存后磁盘」）。教训：凡 UI 上会改变行为的选择控件，默认都应「变更即持久化」，除非明确是草稿态。
- **哨兵 SSE 回写占位名会误导用户**：哨兵路径若把响应 `message.model` 回写成 `claude-relay-model[1m]`（CC 发的哨兵串），CC 显示的是占位名，用户看不到实际用的模型 → 误以为「没按选择的模型转发」。**修复：回写「当前使用模型」完整别名+上下文后缀**（`xf-glm-5.1[200k]`），让 CC 显示真实模型。原则：中转给 CC 的 `message.model` 应反映**实际执行模型**，除非别名语义要求回显请求名。
- **部署/诊断要点**：生产 relay 在 `C:\Users\dcbxfy\AppData\Local\Programs\claude-relay\`（非项目目录）；改 exe 需先 taskkill 再覆盖（Windows 锁定运行中的 exe）；重启用 PowerShell `Start-Process -WindowStyle Hidden`。判别 relay 是否新构建：POST `claude-relay-model[1m]` count_tokens 200 = 支持哨兵（旧构建会 400 no provider prefix matches）。count_tokens 上游失败回退启发式恒 200，**不能**用来验证路由目标——需 --debug 日志或故意设坏 default_model。

### 2.25 CC 内部任务槽位用裸模型名 → 中继 400（Phase 24，2026-08-04）
- **现象**：`/model claude-relay-model[1m]` 后主对话正常走页面模型，但**子代理/后台任务先报错**：
  `Agent "Explore codebase structure" failed: API Error: 400 unknown model 'claude-opus-5': no provider prefix matches`；
  `/compact` 也报 `400 unknown model 'deepseek-v4-flash'`（裸名、无 `[1m]` 后缀）。两处 400 后主流程仍继续，故用户看到「先报错、后正常」。
- **根因**：Claude Code 是**多槽位模型架构**——`/model` 只设「主对话模型」；子代理（Explore/Plan/code review 等）、`/compact`、`/loop`、plan 模式等后台任务走独立的 **opus/sonnet/haiku 任务槽**。自定义 base URL 下这些槽位默认用**内置 Anthropic 名**（`claude-opus-5`/`claude-sonnet-5`/`claude-haiku-4-5-20251001`）或 settings 里残留的裸名 → 中继只认 `{服务商id}-{别名}` 和哨兵 `claude-relay-model[ctx]` → 裸名无前缀可匹配 → 400。
- **修复方向**（CC 官方文档 findings §2.1 已列）：设三个槽位 env var 把任务模型映射到中继可路由名——
  `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_SONNET_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL`。
  值可以是**具体别名**（`ds-v4-flash`，按任务分档省钱）或**哨兵**（`claude-relay-model[1m]` → 跟随页面「当前使用模型」，与主对话一致）。
  `ANTHROPIC_SMALL_FAST_MODEL` 是旧 haiku 槽位名，已废弃。
- **主对话本身没坏**：哨兵 `claude-relay-model[1m]` → `resolve_for_request` 剥 `[1m]` → 命中哨兵 → 换 `server.default_model`（页面选的 xf-glm-5.1）→ 正常路由。问题只在**主对话槽位之外**的任务槽。
- **live 验证（8787）**：sentinel count_tokens 200（新构建）；`/api/status` model_details 确认 xf-glm-5.1 real=`xopglm51` context 200k。

### 2.26 托盘「模型」子菜单 + 托盘线程写配置（Phase 25，2026-08-04）
- **muda 0.16.1 菜单 API 确认**（tray-icon 0.20 内嵌）：`Submenu::with_id(id,text,enabled)->Self`（非 Result）、`Submenu::append(&dyn IsMenuItem)`、`CheckMenuItem::with_id(id,text,enabled,checked,accelerator)` + `set_checked(bool)`（返回 `()` 不返回 Result）、`Menu::append` 收 `&Submenu`/`&CheckMenuItem`（IsMenuItem trait object 隐式 unsize 强转）。`MenuId::new(S: AsRef<str>)`，事件比较 `ev.id() == item.id()`（&MenuId 对 MenuId）。
- **`tray-icon::menu` 与 `tray_icon::menu`**：`use tray_icon::menu::{...}` 即 muda 全量 re-export（`pub use muda::*`），不必直接依赖 muda。
- **tokio `RwLock::blocking_write` 供 std 托盘线程**：tokio 1.53 RwLock 有 `blocking_read/blocking_write`，**在异步上下文调用会 panic**；托盘是独立 std 线程（非 runtime 上下文）故安全。托盘设模型 = `blocking_write` 克隆 cfg → 改 default_model → `Config::save` → `*guard = cfg`（与 webui config_put 同「先校验/写盘、再替换内存」顺序）。
- **托盘菜单是启动时快照**：`spawn` 收 `(服务商显示名, [(完整别名, 别名)])` + 当前模型，菜单构建一次；Web UI 改配置后**需重启托盘**才刷新（已知限制，已文档化）。
- **选中即热重载 ≠ 改 CC 模型**：托盘选中仅设 `server.default_model`；生效语义=D22 哨兵路由——CC 发 `claude-relay-model[ctx]` 时用它，CC 指定具体模型则继续走指定（与用户需求「前提是 CC 用哨兵，否则 CC 走指定」完全一致）。
- **潜在小竞态**：托盘 `set_default_model_blocking` 是「读锁→克隆→改→写盘→回写」，若同时 Web UI 保存了新服务商，托盘写盘会覆盖掉 UI 那次变更（丢失更新）。低频、可接受；真要做需全量 PUT 或文件锁，未做。

### 2.27 服务商使用情况可行性（Phase 26，2026-08-04）
- **余额没有统一接口**：中继是纯透传，余额是各家 OpenAPI 的私有能力，格式/鉴权/路径都不同，不能通用实现。
- **DeepSeek 余额实测可用**：`GET https://api.deepseek.com/user/balance` + `Authorization: Bearer <同款模型 key>` → `{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"5.39","granted_balance":"0.00","topped_up_balance":"5.39"}]}`（真机 ds 账号，只读免费）。注意 balance 接口在**主机根**（base_url 是 `.../anthropic`，要剥掉 `/anthropic` 前缀）。
- **火山引擎/讯飞星火余额不可简单查**：余额走各自控制台 OpenAPI，需 AccessKey/SecretKey（HMAC 签名），模型 key（maas-coding API key）查不了；`ark.cn-beijing.volces.com/api/coding`、`maas-coding-api.cn-huabei-1.xf-yun.com/anthropic` 都无简单 GET 余额接口。
- **调用量/用量统计可通用实现**（纯透传也能做）：中继每请求都能从**自己的转发流**统计——SSE 的 `message_start.message.usage.input_tokens`（输入）、`message_delta.usage.output_tokens`（输出）、count_tokens 响应 `input_tokens`、请求次数，按服务商（+模型）累加。无需任何服务商适配，所有服务商通用。空闲 CPU≈0 目标下用 std Mutex 短锁即可（relay 单线程）。
- **结论**：Phase 26 推荐「用量统计（全服务商，可靠）+ DeepSeek 余额（自动识别，其余显示无简单接口）」；如需火山/讯飞余额，用户得提供能直接 GET 的余额地址（通用 `balance_url` 配置项）。

### 2.28 用量统计实现要点 + 余额路径实测（Phase 26，2026-08-04）
- **用量提取与 model 回写合并进同一条 SSE 缓冲流**：所有 SSE 统一走 ModelRewriter（`do_rewrite=false` 时纯透传）。**含 `usage` 子串的行必须走慢路径解析**——否则 `message_start`（含 model+usage）会被字节快路径先替换并 return，漏计输入 tokens。无 usage 且不改写的行直接 return（零解析），保持纯透传零开销。
- **token 来源**：`message_start.message.usage.input_tokens`（输入，output 通常 0）、`message_delta.usage.output_tokens`（输出，Anthropic 规范=响应累计输出，只加一次不重复）；count_tokens 响应 `input_tokens`；非 SSE `/v1/messages` 读体解析 `message.usage`/顶层 `usage`。启发式回退（count_tokens 上游失败）不计真实用量，只计请求数。
- **模型键 = 完整别名剥 `[ctx]` 后缀**（`split_ctx_suffix(alias).0`）：哨兵路径 alias=`xf-glm-5.1[200k]` → 键 `xf-glm-5.1`，与页面别名一致。
- **真机实测（8892 临时实例 + 部署配置）**：`/api/status.usage` 对哨兵 count_tokens 计数 xf requests=1、model=xf-glm-5.1 ✓；`/api/balance` 的 **ds 返回真实余额 `¥5.10`（available=true, balance_infos 含 granted/topped_up）**，hs（火山方舟）/xf（讯飞星辰）正确返回「无简单余额接口」✓。余额两次查询 ¥5.39→5.10 说明实时。
- **reqwest 无 `json` feature**：`Response::json()` 不可用（Cargo.toml 只开 rustls-tls+stream）→ `r.bytes().await` + `serde_json::from_slice`。
- **e2e 断言**：`/api/status` 含 mock 用量、`/api/balance` 200 且 mock（非 deepseek）返回无简单接口文案。

### 2.29 火山方舟 Coding Plan 用量查询 + 浏览器 Cookie 读取（Phase 28，2026-08-04）
- **console 内部 API**：`https://console.volcengine.com/api/top/ark/{region}/2024-01-01/GetCodingPlanUsage` 是控制台页面调用的内部接口，鉴权靠 `console.volcengine.com` 域名的 Cookie（用户从 DevTools 拿到的响应即契约，无需外部文档）。
- **响应格式（用户提供样例）**：`{"ResponseMetadata":{...},"Result":{"Status":"Running","UpdateTimestamp":<ts>,"QuotaUsage":[{"Level":"session","Percent":3.49,"ResetTimestamp":<ts>},{"Level":"weekly",...},{"Level":"monthly",...}]}}`。`Level` 语义：`session`≈5小时窗口、`weekly`=一周、`monthly`=一月；`Percent`=已用百分比、`ResetTimestamp`=重置时间戳。配额按请求数（由 token 消耗折算），Lite/Pro 额度不同。
- **开源工具佐证**（CodexBar docs/doubao、arkcli）：另有官方 `arkcli` CLI（npm `@volcengine/ark-cli`，`arkcli usage plan --json`，SSO/API-key 鉴权），但那是独立 npm 工具，不内置。中继自己查只需 GET + Cookie 头即可（参考控制台 fetch 行为）。
- **浏览器 Cookie 库读取（Windows）**：
  - 路径：Chrome `%LOCALAPPDATA%\Google\Chrome\User Data\Default\Network\Cookies`（新版在 `Network/` 下）；Edge `%LOCALAPPDATA%\Microsoft\Edge\User Data\...\Network\Cookies`；`os_crypt.encrypted_key` 在对应目录 `Local State`（JSON，base64，前缀 `DPAPI`）。
  - 解密链：`CryptUnprotectData`（windows-sys `Win32_Security_Cryptography`，当前进程=同一 Windows 用户即可）→ 32 字节 key；Cookie `encrypted_value` 前缀 `v10` = 3 字节 + 12 字节 nonce + AES-128-GCM（key 前 16 字节）密文。新版 Chrome 可能用 `v11`（app-bound，key 在 Local State `app_bound_encrypted_key`，跨进程解密受限）——**v11 尽力而为，失败提示手动粘贴兜底**。
  - **SQLite 读取**：Cookie 库是 SQLite 且运行中处于 WAL 模式（最新 cookie 在 `Cookies-wal`，主库可能滞后）→ 手写最小解析器难处理 WAL，**用 `rusqlite`（bundled）最稳**，代价是 exe +~1MB（首个 C 编译依赖，GNU 工具链需 TDM-GCC；opt-level=z 下总量仍 <6MB）。
  - 会话 Cookie（无过期）Chrome 也写入 DB（`expires_utc=0`），查询 `WHERE host_key LIKE '%volcengine.com'` 取全部拼 Cookie 头即可。
- **e2e 可测性**：`console_usage_url` 做成可配置 → e2e 让 mock 服务商指到本地 mock 的 `/GetCodingPlanUsage`（返回用户样例 JSON），无需真实 Cookie 即可断言解析与 import 落盘。

### 2.30 新版浏览器 Cookie 库运行中锁定 + 读取对策（Phase 28 真机，2026-08-04）
- **Chrome/Edge（127+ 防窃取加固）运行中独占锁定 Cookie 库主文件**：Windows 上浏览器开着时，`std::fs::copy`/Python open 报 **os error 32（共享冲突）** / PermissionError——直接读或复制主库都不行。仅**浏览器关闭**或**非运行 profile**（如 Chrome Profile 5 未开）可读。这是硬约束，自动读取只在浏览器关着时可靠。
- **WAL 直开坑**：浏览器开着但某 profile 能打开时，只读直开 WAL 库会 `unable to open database file`（read-only 需要 `-shm`）。对策：**把主库 + `-wal` + `-shm` 一起复制到临时目录，对副本以读-写打开**（SQLite 自动回放 WAL，快照足够新）。副本打开失败/无目标 Cookie 均跳过，继续下一浏览器。
- **windows-sys 0.59 无 `DATA_BLOB`**：DPAPI 类型是 **`CRYPT_INTEGER_BLOB`**（`cbData: u32, pbData: *mut u8`），`CryptProtectData/UnprotectData` 都在 `Win32::Security::Cryptography`（feature `Win32_Security_Cryptography`）；**`LocalFree` 在 `Win32::Foundation`**（不是 System::Memory）。DPAPI 在当前 Windows 用户下对同用户进程可用（真机 roundtrip 测试通过）。
- **`v10` Cookie 解密链**（已单测全链路验证）：`v10` + 12B nonce + AES-128-GCM 密文，key = `os_crypt.encrypted_key` DPAPI 解出的 32B 前 16B。`v11`（app-bound）用 `Local State.app_bound_encrypted_key`（DPAPI + `APPBOUND` magic）尽力而为，失败跳过该条（`console_cookie` 手动粘贴兜底）。
- **实用结论**：UI「导入 Cookie」按钮在浏览器运行时会报「浏览器运行中锁定 Cookie 库——手动粘贴或关浏览器后重试」；**手动粘贴 Cookie 到服务商配置是可靠路径**（存 config.json，掩码展示 + PUT 复用）。
- **e2e 测试路径**：`console_usage_url` 可配置 → mock 服务商指到本地 mock 的 `/GetCodingPlanUsage`（返回用户样例 JSON），用存档 `console_cookie`（saved 源）即可确定性断言解析 + Cookie 头到达；import-cookie 断言「ok 为 true 或 false 均合法」（环境相关）。
- **用量持久化文件命名**：`{配置文件名去最后扩展}-usage.json`——`config.mock.json` → `config.mock-usage.json`（e2e 踩坑）；生产 `config.json` → `config-usage.json`。原子写 `.tmp`+rename，`/api/restart` 新旧进程交接不坏文件。

### 2.31 讯飞星辰 Coding Plan 用量（Phase 30，2026-08-04）
- **端点**：`https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`（控制台内部 API，Cookie 鉴权——`maas.xfyun.cn` 域名的会话 Cookie，同火山方舟 Phase 28 机制）。
- **响应**：`{"code":0,"data":{"rows":[{appId, codingPlanAppCredentialDTO:{apiKey}, codingPlanUsageDTO:{...}, name, expiresAt}], ...},"succeed":true}`。关键：**每行套餐的凭证 apiKey 在 `codingPlanAppCredentialDTO.apiKey`**——用它匹配中继服务商配置的 `api_key`，决定该行归哪个服务商显示。
- **配额字段（codingPlanUsageDTO，单位=次数，Phase 31 用户确认）**：`packageLimit`=一个月总次数、`packageUsage`=一个月已使用次数、`packageLeft`=一个月剩余次数；`rpwLimit`=一周总次数、`rpwUsage`=一周已使用次数；`rp5hLimit`=5小时总次数、`rp5hUsage`=5小时已使用次数；`dailyUsage/dailyLimit`（可为 null）。`name`=套餐名（如 专业版-包月）、`expiresAt`=到期时间。
- **讯飞域判定坑**：讯飞控制台域是 `maas.xfyun.cn`，而 **Anthropic 兼容端点域是 `*.xf-yun.com`**（如 `maas-coding-api.cn-huabei-1.xf-yun.com/anthropic`）——两个域都属讯飞，`console_kind` 判定时两个子串都匹配（`xfyun.cn` || `xf-yun.com`）。
- **按 apiKey 匹配的语义**：一个 xf 服务商配置一个 api_key；查询返回的 rows 里 apiKey == 该 key 的行才显示。配置多个 xf 服务商（多个 key）→ 各自查到各自匹配的套餐，天然「多个就显示多个」。无匹配行 → 报「未找到 apiKey 匹配的 Coding Plan」提示核对。
- **e2e 可测性**：`console_usage_url` 可配置 → xf 服务商指到本地 mock `/coding-plan/list`；base_url 用 `127.0.0.1`（非讯飞域），`console_kind` 靠 **URL 路径含 `coding-plan/list`** 识别为 Xf——与 ark 的 `/GetCodingPlanUsage` 区分（URL 含该串 → Ark）。真实用户则靠 base_url 域直接识别。
- **cookies.rs 域参数化**：`read_volcengine_console_cookie` 泛化为 `read_console_cookie(domain_like, display)`，`query_cookies` 的 SQL 用 `LIKE ?1` 占位——同一套 DPAPI/AES-GCM/SQLite 复制逻辑对不同控制台域复用；新增 `read_xf_console_cookie()`（`%xfyun.cn`）。
- **讯飞用量 UI 最终形态（Phase 32，用户决策）**：**不用进度条**——像火山方舟一样**单行 pill 组**展示：`[套餐名] 22/1200次（5小时）| 1940.4/9000次（1周）| 4653.4/18000次（月）| 2026-08-14（到期）`。每档一个圆角彩色胶囊（5小时 amber / 1周 emerald / 月 sky，`bg-*-50 text-*-700`），已用/总量 ≥90% 转红底警示；套餐名靛蓝 badge、到期灰字。教训：用户对配额展示偏好「紧凑单行信息密度」胜过「可视化条」——设计前先问，别默认上进度条。

### 2.32 OpenAI 协议翻译实现要点（Phase 36，2026-08-04）
- **动机**：OpenRouter/Qwen 百炼等大量服务商只有 OpenAI Chat Completions 协议；Claude Code 只讲 Anthropic Messages。`ProviderConfig.protocol="openai"` 时中继做请求/响应翻译（不推翻纯透传——只对标记了 openai 的服务商生效）。
- **请求翻译关键映射**：Anthropic `system`（字符串或 `[{type:text,text}]`）→ 首位 OpenAI `role:system` 消息；assistant 消息里 `tool_use` 块 → `tool_calls[]`（`arguments` = input JSON 字符串），`content` 与 `tool_calls` 分离（text 保留 content、tool_use 进 tool_calls）；user 消息里 `tool_result` → 独立 `role:tool` + `tool_call_id` 消息；image 块 → `{type:image_url,image_url:{url:"data:{media};base64,{data}"}}`；`thinking` 块直接丢弃。`tools` → `{type:function,function:{name,description,parameters}}`。`tool_choice`：`{type:any}`→`"required"`、`{type:tool,name}`→`{type:function,function:{name}}`、默认 `"auto"`。`stop_sequences`→`stop`。
- **流式响应翻译（难点）**：OpenAI chunk 与 Anthropic 事件结构差异大。做法=维护状态机（message_start 惰性发出 / text 块与 tool 块按需开关 / `delta.tool_calls[].index` 缓冲为 tool_use 块发 `input_json_delta` 分片 / finish_reason + usage chunk + `[DONE]` 收尾发 message_delta+message_stop）。**usage 提取**：请求带 `stream_options.include_usage` → 上游末尾返回 usage chunk（prompt/completion_tokens）→ 收尾累加；无 usage chunk 用 `字符/4` 启发式。
- **serde_json Map 键序**：默认（无 preserve_order feature）是 BTreeMap → JSON 输出**按键排序**（如 `{"text":..,"type":"text_delta"}`）。流式输出的子串断言必须**顺序无关**（分开查 key/value，别查 `"type":"x","k":v` 连串）。
- **Rust 字符串坑**：原始字节串 `br#"..."#` **不处理 `\n` 转义**（`\n` 是字面反斜杠+n），且**不能含非 ASCII**；原始串 `r#"..."#` 同样不处理转义。测试里要 `\n` 换行 + 中文 → 用**普通字符串字面量** `"..."`（两者都支持）。`emit` 做成模块级自由函数（在 `self.tools.iter_mut()` 循环里调 `self.emit()` 会 E0502 借冲突）。
- **proxy 接线点**：`is_openai` 判定 → 上游 URL `{base}/chat/completions`、剥 `anthropic-version`/`anthropic-beta` 头（OpenAI 不认识）、请求体 `anthropic_to_openai`、响应流 `openai_sse_stream`（复用 `UsageSink`/`accumulate_usage`，提为 `pub(crate)`）、非流式 `openai_message_to_anthropic`；count_tokens 直接 `heuristic_tokens` 回退（OpenAI 无对应端点）。
- **e2e**：mock 加 `POST /chat/completions` 返回 OpenAI 流式 chunk（含 usage + [DONE]）；断言 relay 侧收到 `/chat/completions`、body 已翻译（system 消息/model）、无 anthropic-version 头、认证替换、CC 侧收到 Anthropic SSE（message_start/text_delta/message_stop/end_turn）、用量提取、count_tokens 启发式。

### 2.33 术语调整：服务商 → 模型订阅（Phase 38，2026-08-04）
- **动机**：用户视角里「服务商」容易和「模型订阅」混——一个配置条目（id+key+base_url+models）实际是一个**订阅**；「服务商」应特指上游厂商（DeepSeek/讯飞/火山/Kimi/GLM/Anthropic）。全 UI 统一改名，并借机把「服务商」从可空下拉升级为**必填**、把仅讯飞/火山才用的 Cookie 字段按服务商**条件显示**、把 ID 简写改成可读名称。
- **改名覆盖点**（纯前端）：区块标题「模型订阅」；添加/编辑/保存按钮与弹窗标题；空态「暂无模型订阅」；删除确认「删除模型订阅」；`save()`/`modalSave()` toast；清零按钮 title；各处注释。**保留「服务商」的场景**：上游厂商（预设下拉选项、`服务商真实模型名`、`默认按服务商推导`、预设注释）——这些指厂商/公司，不算配置条目。
- **`#m-preset` 必填**：`<option value="">选择服务商…</option>` 占位 + 6 预设 + `value="custom"` 自定义；`openModal` 新增=置空（不选则 `modalSave` fieldError「请选择服务商（必填）」），编辑=**按 id 先匹配预设、再按 base_url 关键词回退**（`presetOfProvider`：deepseek/xf-yun.com|xfyun.cn/volces|volcengine/moonshot/bigmodel/anthropic → 对应预设，否则 custom）。选预设自动填 ID 简写（`m-id`，**可改**）/名称/Base URL/认证；`custom` 不动字段（避免编辑态误清用户手填数据）。
- **Cookie 条件显示**：`#m-cookie-wrap`（cookie 的 label 加了 id）+ `toggleCookieField()`——`m-preset∈{xf,hs}` 才显示，openModal 与 preset change 都调；导入失败自动打开弹窗聚焦 Cookie 的流程仍可用（编辑 xf/hs 时 preset 推断为 xf/hs → 字段可见）。
- **`provName(id)`**：`providers.find(x=>x.id===id)?.name || id || '?'`，替换使用情况（renderUsage/loadBalance/loadCodingPlan）与列表（renderProviders 徽标）/默认模型下拉（dmItem badge）/已注册模型 title 的 ID 简写显示。后端 `/api/status` usage 的 `provider` 字段仍是 id（键），仅展示层映射。

### 2.34 页面字体紧凑/重叠排查（Phase 39，2026-08-04）
- **现象**：部分区域字与字过于紧凑甚至重叠。**三个真实根因**：① flex 里的名称徽标（现在放完整服务商名）没 `shrink-0` + `whitespace-nowrap`——长名被挤压或换行贴邻；② `flex flex-wrap` 容器用 `gap-0` 时**换行行距为 0**，上下两行元素贴住（讯飞 pill 组踩中）；③ `text-[10px]`/`text-[11px]` + `py-px`（1px 垂直内边距）小字号字形贴边裁切感。
- **通用修法（本项目可复用）**：① 徽标/胶囊一律 `shrink-0 whitespace-nowrap` + 合理 `min-w`，长名横向扩展而非换行；② 任何 `flex-wrap` 容器必须带 `gap-y-*`（至少 1）给换行留行距；③ 最小字号提到 11-12px，垂直内边距 ≥ `py-0.5`；④ 全局 `body{letter-spacing:.012em;-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}` 对中文观感提升明显；⑤ 手写类（如 `.leading-loose-inline{line-height:1.7}`）放 `<style id="ui">` 块——build_ui.py 不覆盖该块，长段说明文字用它放宽行高。
- **坑**：`text-xs` 在 Tailwind v4 的 line-height 是 1.333（`calc(1/.75)`），对长中文段落仍偏紧，需叠加 `leading-*`；小字号胶囊若要紧凑又防裁切，用 `leading-none`（字形不被行盒放大）。

### 2.35 单行 pill 组宽度预算（Phase 40，2026-08-04）
- **现象**：讯飞用量 pill 组在 768px 容器下换行。**宽度预算**（`text-xs`=12px，CJK≈12px/字、数字≈7px/字）：`22/1200次（5小时）`≈128px、`1940.4/9000次（1周）`≈132px、`4653.4/18000次（月）`≈144px、3 个 `|` 分隔≈39px、`2026-08-14（到期）`≈114px → 整组约 **557px**。而行内可用 = 容器 - px-4(32) - 名称徽标(~70) - gap(12) - 导入/粘贴按钮(~130)。
- **结论**：要让 pill 组单行放下，容器 ≥ 768px（`max-w-3xl`）不够——info 栏只余 ~620px；提到 **896px（`max-w-4xl`）** 后 info 栏约 644px ≥ 557px ✓。配合 pill `px-1.5` + 分隔符 `mx-0.5` 再省 ~40px 余量更足。**通用教训**：横向一排 nowrap 元素做宽度预算时，先估元素总宽再反推容器 min-width；`flex-wrap` + `gap-y-*` 是窄窗口优雅降级的兜底。

### 2.36 讯飞输入 tokens 为 0 的根因 + 启发式兜底（Phase 41，2026-08-04）
- **现象（真机）**：`xf requests=6 in=0 out=772`，ds/hs 输入正常。**根因是双重漏记**：
  1. 讯飞星辰 Anthropic 兼容端点的 `message_start` 事件**不返回 `usage.input_tokens`**（返回空）；中继按 Anthropic 标准只从 `message_start.message.usage.input_tokens` 提取输入 → 输入恒 0。
  2. `rewrite_line` 的快路径优化：只有行含 `"usage"` 子串才进慢路径调 `accumulate`——讯飞 message_start 行恰好无 usage → 走快路径（或纯透传 return），**连累计函数都不调用**。
- **修复**：`UsageSink` 加 `input_fallback`（=请求体字符/4，`heuristic_input()`，与 count_tokens 兜底 D8 同规则）；`rewrite_line` 对 `message_start` 行强制慢路径累计（`is_message_start` 字节判定，每流仅 1 行，解析开销可忽略）；`accumulate` 的 message_start 分支无 input 时用 fallback；非流式 Anthropic 响应同样兜底；translate.rs `OpenAiStreamTranslator.finalize` 的 `in_tokens==0` 时用 fallback（OpenAI 不返回 usage chunk 也兜底）。
- **语义边界**：兜底是**估算**（字符/4），**非上游计费值**；仅当上游不返回输入用量时启用；上游恢复返回则自动用真实值（`usage_uses_real_input_when_present` 单测锁定不覆盖）。历史存量数据不回溯，新对话后开始累计。
- **性能注意**：message_start 进慢路径意味着**每个流多解析 1 行 JSON**——相对纯透传是恒定的每请求一次小开销，可接受（D7 快路径收益仍是主要的）。

### 2.37 托盘 Win32 信息气泡：GUID 无窗口方案在 Win10 返回 E_FAIL，经典 hWnd+uID 可靠（Phase 42，2026-08-05）
- **需求**：托盘菜单加「更新 claude 配置 / 更新环境变量」→ 服务隐藏运行（--autostart 黑框隐藏）时 stderr 不可见，需要**气泡**作唯一可见反馈。tray-icon 0.20 / muda **无气泡 API**，只能裸调 `Shell_NotifyIconW`。
- **踩坑（真机实测）**：先用 `NIF_INFO | NIF_GUID` + 稳定 GUID + `uTimeoutOrVersion=NOTIFYICON_VERSION_4`、**hWnd=null**（Windows 7+ GUID 模式 hWnd 被忽略）→ `Shell_NotifyIconW(NIM_ADD)` 返回 **0（失败）**，last_os_error = `-2147467259`（= `0x80004005` E_FAIL）。GUID 方案在**未先 NIM_ADD + NIM_SETVERSION(NOTIFYICON_VERSION_4) 注册**时，靠 ADD 时 uTimeoutOrVersion=4 不生效 → 无法用 GUID 标识通知 → E_FAIL。
- **修复（可靠路径）**：回到**经典 hWnd+uID 模式**——`RegisterClassW`（`WNDCLASSW.lpfnWndProc = Some(DefWindowProcW)`，hInstance=`GetModuleHandleW(NULL)`）→ `CreateWindowExW` 创建**消息专用窗口**（`HWND_MESSAGE`，不占任务栏）→ `NOTIFYICONDATAW{ cbSize, hWnd=该窗口, uID=1, uFlags=NIF_INFO, dwInfoFlags=NIIF_INFO, szInfo/szInfoTitle }` → `Shell_NotifyIconW(NIM_ADD)`。**只设 NIF_INFO（无 NIF_ICON/NIF_MESSAGE/NIF_GUID）= 临时图标，气泡消失自动移除**。真机验证返回非零（成功），气泡正常显示。
- **消息泵适配**：托盘线程已有 `GetMessageW/DispatchMessageW` 循环会调度本线程所有窗口消息，气泡窗口无需任何处理逻辑（WndProc 直接用 DefWindowProcW）。
- **API 位置（windows-sys 0.59）**：`Shell_NotifyIconW`/`NOTIFYICONDATAW`/`NIF_*`/`NIIF_*`/`NIM_*` 在 `Win32::UI::Shell`（需 feature `Win32_UI_Shell`）；`NOTIFYICONDATAW` 的 `Anonymous` union 里 `uTimeout`/`uVersion` 是**第一成员**（x86_64 直接赋值即可）；`GetModuleHandleW` 在 `Win32::System::LibraryLoader`（feature `Win32_System_LibraryLoader`）；`GUID` 在 `windows_sys::core`。
- **标准 x64 `NOTIFYICONDATAW` 尺寸**：cbSize = sizeof = **976 字节**（windows-sys x86_64 结构自然对齐，与 Win32 一致）。

### 2.38 Claude Code 自定义斜杠命令：给 CC 加 `/models` 查看模型列表（Phase 44，2026-08-05）
- **需求**：在 CC 里输 `/models` 查看中继当前模型列表。**无需改 Rust/重启 relay**——用 CC **自定义斜杠命令**。
- **机制**：命令文件放 `~/.claude/commands/<名字>.md`（用户全局，所有项目可用；项目级在 `<cwd>/.claude/commands/`）。格式 = YAML frontmatter + 正文：frontmatter 必填 `description`，可用 `allowed-tools: "Bash"`（**引号包空格分隔**——本机 planning-with-files 插件 `commands/*.md` 的真实样例就是 `"Read Bash"` 这种，非逗号）。输 `/` 时正文作为**用户提示注入**，由模型执行（本质是「让模型去 curl + 格式化」的提示词，非直接跑脚本）。
- **数据源选型**：`GET /api/config` 返回 `cfg.masked()`——含 `server.default_model` + `providers[].models[{alias,real,context}]`，比 `/v1/models`（只回 `{id}` 列表）信息全；Key/console_cookie 已掩码，正文明确禁止展示。
- **⚠️ 加载时机**：CC 自定义命令在**会话启动时**扫描加载——新增命令文件后，已开着的会话要**重启（或新开会话）**才能看到；旧会话 `/` 菜单里没有。
- **端口回退顺序**（正文指引模型）：默认 8787 → `echo $ANTHROPIC_BASE_URL`（start-claude.cmd 在进程环境里设置了会话 base URL，最可靠）→ settings.json `env.ANTHROPIC_BASE_URL`。
- **扩展点**：同类「给 CC 加本地工具型命令」（查余额、清用量等）都可复用此模式——命令正文指向 relay 的 `/api/*` 即可。

### 2.39 双击自动安装 CC 自定义命令：命令文件内嵌进 exe（Phase 45，2026-08-05）
- **需求**：把 `claude-relay.exe` 分发给用户后，跑一次（双击）即自动生成 `/models` 自定义命令，不用手工建文件。
- **实现**：命令文件本体 `assets/commands/models.md` 入库，`include_str!` 内嵌进 exe（与 `webui.rs` 的 `include_str!("../webui/index.html")` 同模式，+~2KB）；新模块 `src/commands.rs::ensure_models_command()` 在 main 启动早期调用——**已存在则不动**（幂等，不覆盖用户自定义内容）、缺失则 `create_dir_all` + 写入。失败仅 `[warn]` 不阻断服务。
- **命令目录解析（与 `webui::claude_settings_path` 同规则）**：`CLAUDE_CONFIG_DIR` 优先（`{dir}/commands`）；否则 `{USERPROFILE|HOME}/.claude/commands`。
- **放置点**：main() 的 `--print-default-model` 分支**之后**（该分支保持纯查询、零副作用）；正常启动（双击/autostart/restart）都会确保命令存在。
- **测试技巧**：单测用临时 `CLAUDE_CONFIG_DIR` 隔离 `set_var`——不污染真实 `~/.claude`；并行测试时其他测试不读该变量，风险可忽略。

### 2.10 端到端验证结论（Phase 4，2026-08-03，test/e2e.py 25 断言全过）
- **工具调用全链路**：中继对 `tools` 数组零处理原样透传（纯透传），tool_use SSE 块（content_block_start/delta/stop）字节级透传无损。真机 CC 会话（本工程经 relay 运行）中 Bash/Read 等工具调用持续正常。
- **断线重连**：客户端读一半断开 SSE，relay 不崩（上游写抛 BrokenPipe，中继透传流终止），下一请求立即正常——中继天然无状态。
- **认证替换精确行为**：剥离 CC 的 `x-api-key`/`Authorization`，替换为 `Bearer <服务商Key>`；`anthropic-version` 白名单透传；`anthropic-beta` 默认透传（可配剥离）。
- **测试细节**：relay 转发请求头时保留原始大小写名（CC 发小写）；Windows 客户端中途断开会抛 `ConnectionAbortedError`(10053) 而非 BrokenPipeError；urllib 对非 2xx 抛 HTTPError。

## 3. 体积/资源优化手段
- `[profile.release]`: `opt-level="z"`、`lto=true`、`codegen-units=1`、`panic="abort"`、`strip=true`
- axum 裁剪特性：`default-features=false, features=["http1","json","tokio"]`（砍 http2/ws）
- reqwest `default-features=false, features=["rustls-tls"]`（避开 OpenSSL 静态依赖）
- 单线程 `tokio` current_thread runtime → 空闲内存个位数 MB
- 可选 UPX 压缩进一步缩小（注意杀软误报风险）
- 预估产物：stripped 5~8MB，空闲内存 ~5-10MB，空闲 CPU ≈0

## 4. 需要实现的中转端点
| 端点 | 作用 | 实现方式 |
|------|------|----------|
| `POST /v1/messages` | 主对话 | 翻译/透传 + 流式 |
| `POST /v1/messages/count_tokens` | token 统计 | Anthropic 透传 / openai 启发式估算 |
| `GET /v1/models` | 模型列表(可选) | 聚合所有服务商模型 |
| `GET /healthz` | 存活 | 静态 |

## 5. 待验证风险点（列入 Phase 1/4 验收）
- Claude Code 在自定义 base URL 下是否允许任意模型名（`前缀:模型`）→ **已实测允许**（§2.1），三档兜底无需启用。
- count_tokens 不精确实现是否仅影响显示不影响主流程 → **已实现**：优先转发服务商端点，失败回退 `字符/4`（D8）；透传 42 实测通过。
- 工具调用全链路翻译的正确性 → **已验证**（§2.10）：tools 原样透传 + tool_use SSE 无损，真机 CC 工具调用持续正常。
