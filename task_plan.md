# 任务计划：ClaudeRelay — Claude Code 多服务商多模型动态切换中转工具

## Goal
做一个**本地中转服务**（Rust 单 exe）：Claude Code 通过 `ANTHROPIC_BASE_URL` 指向它，
用户在 Claude Code 内用 `/model {服务商前缀}-{模型名}` 动态切换到任意服务商的任意模型，**无需重启**。
附带可视化 Web 配置页。硬性要求：**exe 尽量小、运行内存极低、CPU 占用低**。

## 架构概览（v2：全 Anthropic 协议，纯透传）

> 用户决策：**所有服务商都提供 Anthropic 协议的 base url** → 中继无需协议翻译，
> 核心是「改 model → 换 Key → 原样转发 → 流式透传返回」，零翻译、零缓冲转换。

```
┌──────────────┐   ANTHROPIC_BASE_URL=http://127.0.0.1:8787
│  Claude Code │─────────────────────────▶┌────────────────────────────┐
│  (客户端)     │  Anthropic Messages 格式  │  ClaudeRelay (Rust)         │
│  /model      │◀─────────────────────────│  · /v1/messages 纯透传       │
│  ds-deepseek │   SSE 原样流式回传        │  · model 改写 + Key 替换      │
└──────────────┘                          │  · 前缀-模型 → 服务商路由    │
                                          │  · / (配置 Web UI)           │
                                          └───────────┬────────────────┘
                                   每个服务商都接 Anthropic 兼容端点
                                   ┌───────────────┬───────────────┐
                                   ▼               ▼               ▼
                     DeepSeek /anthropic   Kimi /anthropic   Anthropic 官方
                     (base_url + /v1/messages)   —— 全部原样转发/透传
```

## 关键决策 Decisions

| # | 决策 | 理由 |
|---|------|------|
| D1 | Rust + `tokio`(current_thread 单线程) + `axum`(仅http1) + `reqwest`(rustls) | exe 小(~4-6MB strip后)、空闲内存几MB、CPU 近0；**目标实际用 `x86_64-pc-windows-gnu`**（本机无 MSVC 链接器，GNU 产物体积差异 <10%） |
| D2 | 模型名格式 `{服务商id}-{模型}`，如 `ds-deepseek-chat`、`kb-moonshot-v1-8k`；分隔符可配 | 全局唯一、一眼识别服务商 |
| D3 | **全 Anthropic 协议纯透传**（服务商配置 `base_url` 即其 Anthropic 兼容端点）| 用户决策；零翻译→体积/CPU 最小化 |
| D4 | 配置源为 `config.json`，Web UI 编辑后写回并**热重载**(Arc<RwLock>) | 可视化配置、免重启 |
| D5 | **单端口**(默认 8787，仅绑 127.0.0.1)：`/v1/*` 走中转，`/` 和 `/api/*` 走配置页 | 一个端口最简单 |
| D6 | 编译优化：`opt-level="z"` + `lto` + `codegen-units=1` + `panic="abort"` + `strip`；可选 UPX | 极致减小体积 |
| D7 | **SSE 字节级透传**（不解析不转换）；可选逐行改写 `model` 字段回显前缀名 | 流式零开销；UX 好 |
| D8 | `/v1/messages/count_tokens` 优先转发服务商端点，失败回退启发式 `字符/4` | 兼容 CC 的 token 显示 |
| D9 | 服务商可选 `max_tokens` 上限，转发前改写 clamp（CC 恒发 32000+） | 防某些上游拒绝 |
| D10 | 替换认证头：剥掉 CC 的 `x-api-key`/`Authorization`，改用服务商配置的 Key（`Authorization: Bearer` 默认，可配 `x-api-key`）；保留 `anthropic-version`；`anthropic-beta` 默认透传、可按服务商配置剥离 | 认证替换 + 兼容开关 |
| D11 | `--debug` 请求日志（method/path/header/body 摘要）+ 最小 `GET /v1/models` 作保险 | 首次联调验证 CC 行为 |
| D12 | 模型名解析：按**已配置服务商 id 做最长前缀匹配**（因 `-` 也出现在模型名中，如 `ds-deepseek-chat`），无匹配则报错 | 消除 `-` 分隔的歧义 |
| D13 | Web UI 掩码 Key 往返：GET 只回掩码（`****abcd`）；PUT 遇含 `*` 的 Key 按 id 复用当前真实值，新服务商掩码 Key 则 400 拒绝 | UI 不落盘/不泄露真实 Key，又支持「未改不填」 |
| D14 | **settings 隔离**：`claude --settings <副本>` 加载独立 settings 文件（env 指向 relay），主 `~/.claude/settings.json` 不动 | 中转配置与日常使用彻底隔离；实测 --settings 文件 env 优先于 user settings.json |

## 模型切换与使用方式

1. 用户配置好服务商（Web UI 或手写 config.json）
2. 用启动脚本设置环境变量再进入 Claude Code：
   ```bat
   set ANTHROPIC_BASE_URL=http://127.0.0.1:8787
   set ANTHROPIC_API_KEY=relay-dummy            :: 中继会替换成服务商真实 Key
   set ANTHROPIC_MODEL=ds-deepseek-chat         :: 默认模型
   claude
   ```
3. 在 Claude Code 内 `/model kb-moonshot-v1-32k` → 中继下一请求读到 model 字段，
   `kb` 前缀命中 Kimi → 改写为 `moonshot-v1-32k` 转发，流式原样回传。**切换零重启**。

> ⚠️ **模型切换可行性（调研结论，需联调验证）**：`ANTHROPIC_MODEL` / `--model` 接受任意模型名（确定）。
> 交互式 `/model` 输入自定义名**版本相关**。三档切换手段，Phase 1 用 `--debug` 真机验证：
> 1) `/model 前缀-模型`（首选，若 CC 接受任意名）
> 2) `claude --model 前缀-模型` 或 `ANTHROPIC_MODEL` env（可靠兜底，重启生效）
> 3) `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` 把 CC 内部任务槽位映射到指定模型

## 核心机制：纯透传映射（极简）

| 输入(来自 CC) | 处理 | 输出(发给服务商) |
|---|---|---|
| body.model = `ds-deepseek-chat` | 前缀匹配→改写 | `deepseek-chat` |
| 认证头 `x-api-key`/`Authorization` | **剥离替换** | 服务商 Key（Bearer 或 x-api-key） |
| `anthropic-version` 等 | 原样保留 | 同左 |
| `anthropic-beta` | 默认透传(可配剥离) | 同左 |
| `max_tokens: 32000` | 若配置上限则 clamp | 例 `8192` |
| 响应 SSE | 原样透传；(可选)逐行把 `message.model` 改回 `ds-deepseek-chat` | → 回传 CC |

## 目录结构（v2，去掉翻译相关模块）
```
D:\Tools\ClaudeModelSwitch\
├── Cargo.toml
├── config.json            # 默认配置(含示例服务商)
├── src/
│   ├── main.rs            # CLI、配置加载、单线程 runtime、--debug
│   ├── config.rs          # 配置结构/校验/读写/热重载
│   ├── router.rs          # `前缀-模型`→(服务商,真实模型) 最长前缀匹配
│   ├── proxy.rs           # 请求转发 + SSE 透传 + 头部/body 改写
│   ├── api.rs             # /v1/messages /count_tokens /v1/models /healthz
│   └── webui.rs           # include_str! 内嵌配置页 + /api/* 读写
├── webui/index.html       # 纯原生 JS 配置页
├── claude.cmd             # 一键设置环境变量并进入 CC
└── README.md
```

## 阶段 Phases

### Phase 1 — 骨架 + 配置 + 纯透传中转 【complete】
- [x] Cargo 项目初始化，profile 编译优化配置（release exe **2.0MB**，优于 6MB 目标）
- [x] config.rs：providers/server 结构、JSON 读写、校验、热重载（Arc<RwLock>）
- [x] router.rs：`前缀-模型` 最长前缀匹配解析（5 单测）
- [x] proxy.rs：/v1/messages 转发（model 改写、Key 替换、max_tokens clamp、SSE 透传、model 回写）
- [x] api.rs：count_tokens 转发+启发式回退、/v1/models 聚合、/healthz
- [x] main.rs：CLI(默认 `--config config.json`)、`--debug` 请求日志、单线程 runtime
- [x] 验收A：mock Anthropic 上游 + curl 模拟 CC **8 项全过**（详见 progress 会话 3）
- [x] 验收B：真实 Claude Code 接中继跑通，--debug 日志锁定 `/model` 任意名是否被接受；
  结果：**交互式 `/model` 接受自定义模型名**（实测 v4-flash → deepseek-chat → v4-pro 三次切换全 200），三档兜底无需启用

### Phase 2 — 可视化配置 Web UI 【complete】
- [x] webui/index.html：服务商增删改查、models 标签（回车/逗号+×删除）、Key（掩码显示）、可选 max_tokens/认证头方式、strip_beta/rewrite_model 开关
- [x] /api/config GET(masked)+PUT(保存+热重载) + /api/status(运行状态/uptime/已注册模型列表)
- [x] 保存→写 config.json→热重载（下一请求即生效，免重启）；一键复制 CC 环境变量命令（cmd 格式，含 http:// 前缀）
- [x] 掩码 Key 复用（D13）：GET 返回 `****abcd`，未改动的 Key PUT 时按 id 自动用回真实值；新服务商误填掩码 Key 直接 400 拒绝
- [x] 验收A：curl 验证 GET /(配置页) /api/status /api/config masked；PUT 改名+加模型→热重载即时生效（/v1/models 同步更新）；掩码 Key 复用→磁盘真实 Key 不变；400 错误分支（新服务商掩码 Key/空服务商/垃圾 JSON）；回归 mock 全链路透传（前缀剥离+Key 替换+max_tokens clamp+SSE 别名回写）
- [x] exe 2.02MB（+20KB 内嵌 HTML），空闲内存 4.6MB
- 验收B（浏览器）：**进行中** — 已启动 `http://127.0.0.1:8787` 待用户浏览器确认

### Phase 3 — 打包发布 + 启动体验 【complete】
- [x] release 构建瘦身核对：exe **2.14MB**（含托盘 +120KB）；**跳过 UPX**（已远优于 ≤6MB，UPX 有杀软误报风险，见 findings §3）
- [x] `--print-default-model` CLI：按配置顺序输出第一个服务商第一个模型别名（如 `ds-deepseek-chat`），供 claude.cmd 取默认模型
- [x] **start-claude.cmd** 包装脚本（原 claude.cmd，因命名冲突改名，见 §2.9）：检测 relay → 未运行则最小化启动并等就绪 → 取默认模型 → **`claude --settings claude-settings.json` 进 CC**。**纯 ASCII 编码**（批处理中文需 GBK/65001，ASCII 全兼容零坑）
- [x] **settings 隔离方案（用户提出）**：复制 `~/.claude/settings.json` → 项目 `claude-settings.json`，env 指向 relay（BASE_URL=http://127.0.0.1:8787 / KEY=relay-dummy / MODEL=ds-deepseek-v4-flash）。start-claude.cmd 用 `--settings` 加载 → **主 settings.json 保持 DeepSeek 直连不动**，日常使用零影响
- [x] **实测验证**：`claude --settings claude-settings.json --print "你好"` → relay 日志确认请求穿过（ds-deepseek-v4-flash → 200 OK）→ **证明 --settings 文件 env 优先级高于 user settings.json env**；`start-claude.cmd --print hello` 全流程成功
- [x] 系统托盘图标（tray-icon + muda）：右键「打开配置页」/「退出」；退出直接 exit(0)
- [x] 分布布局：`claude-relay.exe` + `start-claude.cmd` + `claude-settings.json` + `config.json` 项目根
- [x] **真机联调核心通过**：真实 CC → relay → DeepSeek 200 OK（model 改写 / key 替换 / SSE 透传）
- [x] **交互式 `/model` 切换验收通过**：v4-flash → ds-deepseek-chat → ds-deepseek-v4-pro 三次切换全 200（CC 接受自定义名，切换零重启）→ Phase 1 验收B 完成
- [x] **Phase 3 验收**：双击 start-claude.cmd → CC → /model 切换各模型成功
- ⚠️ 未解之谜：CC 大请求（~1MB）曾偶发静默 400/挂起，已加诊断日志待复现（实测 244KB 上下文全 200，非阻塞）

### Phase 4 — 端到端测试 + 文档 【complete】
- [x] **test/e2e.py 端到端验收脚本**（可复现，无需真实 Key）：mock Anthropic 上游（含 tool_use SSE 序列）+ relay 子进程，**25 项断言全过** —— 启动就绪/别名聚合/对话流式（model 改写、Key 替换、x-api-key 剥离、anthropic-version 透传、max_tokens clamp、tools 原样透传、SSE 事件序列完整、中文无损、message.model 回写别名）/count_tokens 透传 42/断线重连（relay 不崩、下个请求正常）/未知模型 400+别名提示
- [x] 真实服务商联调矩阵：**DeepSeek 已实测通过**（CC→relay→DeepSeek 200，多轮对话 + 工具调用；244KB 上下文全 200）；Kimi/GLM/Anthropic 官方为同构「Anthropic 兼容端点」接入，架构层面由同一透传路径覆盖，配 Key 即用
- [x] 工具调用全链路：E2E 脚本显式验证 tool_use 块透传；真实 CC 会话（本工程即经 relay 运行）中 Bash/Read 等工具调用持续可用 —— **验收「配置→启动→切换→对话→工具」全链路达成**
- [x] README.md 使用文档（简介/架构/快速开始/config 字段参考/CLI 参数/性能实测/故障排查/项目结构/测试）
- [x] 内存/CPU 实测记录：release exe **2.1MB**、空闲内存 ~8.7MB（有流量后 10.4MB）、CPU 累计 0.234s/空闲≈0；E2E 无残留进程
- 遗留（非阻塞）：CC ~1MB 大请求偶发挂起谜题 —— 诊断日志已埋（proxy.rs 三处 400 路径），复现时随用随查

### Phase 6 — Web UI 改用 Tailwind CSS 重构 + 修复弹窗自启 Bug 【complete】
- 用户需求：用 Tailwind CSS 重构配置页；**修复「一运行就弹出添加服务商弹窗且不能关闭」bug**（应为点击「＋ 添加服务商」/「编辑」才弹出）。
- **根因（已定位并修复）**：`.overlay{display:flex}` 与 UA 样式 `[hidden]{display:none}` 特异性相同，作者样式压过 UA → `hidden` 属性完全失效（加载即显、关闭无效）。修复：`@layer base{ [hidden]{display:none!important} }`（Tailwind v4 preflight 本身也带 `[hidden]` 规则，双保险）。
- Tailwind 接入方式（用户选定）：**编译内嵌静态 CSS**（自包含、离线可用、生产级）——`npx @tailwindcss/cli` 编译 `webui/tailwind/in.css`（`@source` 指向 index.html）→ `webui/build_ui.py` 幂等注入 `<style id="tw">` 占位块 → webui.rs include_str! 一起进 exe。
- [x] `webui/tailwind` 构建环境：package.json（tailwindcss v4.3.3 + @tailwindcss/cli）+ `in.css`（`@import` + `@source "../index.html"` + `[hidden]` 修复）
- [x] `webui/index.html` 全量重写为 Tailwind utility classes：全部元素 ID / 事件绑定 / JS 逻辑不变，仅静态标记与 JS 内 className 赋值改 Tailwind 类
- [x] `webui/build_ui.py` 一键构建：重置占位块（内容为 marker 或已注入产物一律重置，保证扫描干净源码）→ 编译 → 幂等注入（重复运行产物一致，已修 assert 假阳性）
- [x] 验收全过：`cargo check` ✓ / 抽取 JS `node --check` 语法 ✓ / 编译产物含 `[hidden]` 规则 + 关键类 ✓ / 构建真幂等 ✓ / **`python test/e2e.py` 25 断言全绿** ✓ / release exe 2.16MB（+15KB CSS）✓ / 8787 热切到新构建，curl 确认 `hidden` 属性在 + Tailwind CSS 已服务 ✓
- [x] release 重新构建（CARGO_TARGET_DIR 避开锁），替换项目根 + target/release exe，**PID 定向重启** 8787（新 PID 9300，零 CC 中断）；toast 宽度微调后二次部署

### Phase 7 — 操作按钮移到页面上方 + 新增「保存配置并重启」+ 修复删除模型不持久化 【complete】
- 用户需求：① 把「保存配置 / 复制 CC 环境变量命令」从底部 fixed 栏移到**页面上方**；② 新增**「保存配置并重启」按钮**及对应后端实现。
- **顺带修复上一条报告（删除模型刷新后恢复）根因**：弹窗「保存服务商」只更新内存 `providers` 数组、**不落盘**，用户以为已保存、刷新后读回磁盘旧配置 → 模型回来。改为弹窗保存即 `PUT /api/config` 持久化。
- **重启实现**：`POST /api/restart`（webui.rs）→ `Command::spawn` 拉起自身同参数（`current_exe` + `args().skip(1)`）→ 600ms 后 `exit(0)`（先让 HTTP 响应发出去、再让出端口）→ 返回 `{ok,restarting}`；main.rs 绑定改**重试循环**（20×150ms）让新进程扛过旧进程未退出瞬间。
- [x] 后端：api.rs 注册 `/api/restart` POST；webui.rs 加 `restart` handler；main.rs `bind` 改 `bind_with_retry`（20×150ms）
- [x] 前端：index.html 底部 action bar 移到顶部（header 下，保存配置/保存并重启/复制 env + toast），main `pb-24`→`pb-6`；新增「保存配置并重启」按钮 + `saveRestart()` JS（先 PUT config 再 POST /api/restart，成功后 ~2.5s `location.reload()`）
- [x] 前端：`modalSave()` 改 `async` + 保存即持久化（PUT 成功才关弹窗，失败留弹窗报错）—— 修复删除模型刷新后恢复
- [x] 验收全过：`cargo check` ✓ / JS `node --check` ✓ / **e2e 回归 25 断言全绿** ✓ / **手动重启验收**（8788 临时实例 POST /api/restart → 返回 `{ok,restarting}`、PID 14768→21828、端口复用、healthz ok）✓ / **8787 线上实测自重启**（curl /api/restart → PID 18148→12224、healthz ok、配置完好）✓
- [x] release 重新构建（2.16MB），部署 8787（当前 PID 12224）

### Phase 8 — 模型别名映射 + 默认模型选择 + 更新 Claude 配置/环境变量 + 确认弹窗 + 5 色按钮 【complete】
- 用户需求：① 顶部新增**「更新 claude 配置」「更新环境变量」**两个按钮（用户选定：**两者都写 `~/.claude/settings.json`**，只改 env 里 `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` 三键，其余全保留，有确认弹窗）；② 页面新增**默认模型选择器**（从已注册模型中选一个，写入 `server.default_model`）；③ **模型别名→真实模型**配置（`{id}-{别名}` → 真实模型，如 `ds-flash`→`deepseek-v4-flash`，仍拼接前缀）；④ 保存并重启/更新 claude 配置/更新环境变量都加**确认弹窗**；⑤ **5 个按钮用颜色区分**；⑥ 所有弹窗做好看。
- **模型别名设计（向后兼容）**：`ProviderConfig.models` 从 `Vec<String>` 改为 `Vec<ModelEntry{alias,real}>`。serde 自定义：接受字符串（alias=real，兼容旧配置 `["mock-chat"]`）或 `{alias,real}` 对象；序列化时 alias==real 写字符串、否则写对象。`resolve()`：后缀先查 alias → 命中则 real，否则后缀直接当真实模型名（两种都能用）。`available_aliases()` 输出 `{id}-{alias}`。
- **默认模型**：`server.default_model`（String，存完整别名如 `ds-flash`）。`--print-default-model` 与 copyEnv 优先用它，否则回退第一个服务商第一个模型别名。
- **更新 settings 后端**：`POST /api/update-settings` 读当前配置算 3 值（BASE_URL=`http://{host}:{port}`、KEY=`relay-dummy`、MODEL=默认模型），读 `~/.claude/settings.json`（尊重 `CLAUDE_CONFIG_DIR`）→ 合并 `env` 3 键 → 写回（其余键与文件全部保留）。两个按钮共用此端点。
- [x] 后端 config.rs：`ModelEntry`（自定义 serde 字符串/对象双形 + alias==real 写字符串）+ `models: Vec<ModelEntry>` + `server.default_model` + `Config::default_model()`；3 个新单测
- [x] 后端 router.rs：`resolve()` 别名→真实；`available_aliases()` 用 alias；2 个新单测
- [x] 后端 main.rs：`--print-default-model` 用 `server.default_model` 或回退
- [x] 后端 webui.rs `update_settings`（尊重 `CLAUDE_CONFIG_DIR`）+ api.rs 注册 `/api/update-settings`
- [x] 前端：顶部 **5 按钮分色**（保存配置=绿 / 保存并重启=橙 / 更新claude配置=蓝 / 更新环境变量=靛 / 复制env=灰）；默认模型 `<select id=srv-model>`（从 st.models 填充、随保存写入 server.default_model）；弹窗模型别名 chips（`别名=真实名`）；通用确认弹窗 `confirmAction`（保存并重启/两更新按钮走它，按动作配色）；copyEnv 用默认模型
- [x] 验收全过：`cargo test` **16 全绿**（原 11 + 新 5）✓ / JS `node --check` ✓ / **e2e 回归 25 断言全绿** ✓ / **update-settings 实测**（CLAUDE_CONFIG_DIR 临时目录 → 3 键写入 + FOO/theme/statusLine/model 全保留）✓ / `--print-default-model` 回退 `ds-deepseek-v4-flash` ✓ / 部署 8787（新 PID 6824）✓
- [x] release 2.19MB（2,302,464B，+30KB）部署 8787

### Phase 9 — 默认模型选择器美化 + 模型三列列表（别名/真实名/上下文）+ 移除 max_tokens 【complete】
- 用户需求：① **默认模型交互设计漂亮点**（改为可搜索下拉，展示别名/真实名/上下文徽标/服务商徽章，可清除）；② **添加服务商时模型改列表形式，三列**：第一列模型别名、第二列服务商真实模型名、第三列**上下文大小默认 200k**（下拉选择主流上下文大小）；③ 界面要漂亮、交互友好；④ **移除 max_tokens 上限设置**。
- **ModelEntry 加 context**（上下文窗口 tokens，默认 200000）：向后兼容 serde——字符串 `"x"`（alias=real，context 默认）、对象 `{alias,real,context?}`（context 省略则默认）；序列化 alias==real && context==默认 → 字符串，否则对象（context 非默认才写出）。元数据，仅 UI 展示用。
- **移除 max_tokens**：`ProviderConfig.max_tokens` 字段删除 + proxy.rs clamp 块删除 + UI m-mt 输入删除 + config.json/config.mock.json 删除 + e2e 断言改「max_tokens 原样透传 32000」（替代原 clamp 断言，保持 25 项）。
- **/api/status 加 model_details**：`[{provider, alias, real, context, full}]`（full=`{id}{sep}{alias}`），供默认模型下拉与已注册模型区展示上下文。
- [x] 后端 config.rs：ModelEntry.context（默认 200000）+ 移除 ProviderConfig.max_tokens + 单测新增 2（context roundtrip / 字符串默认）
- [x] 后端 proxy.rs：删除 max_tokens clamp 块
- [x] 后端 router.rs：测试 helper 移除 max_tokens 字段、ModelEntry 字面量补 context
- [x] 后端 webui.rs：/api/status 返回 model_details
- [x] 前端：默认模型改可搜索自定义下拉（dm-btn/dm-drop/dm-search，每行服务商徽章+别名+真实名+上下文徽标，点行选中、可清除回未设置，Escape/外点关闭）
- [x] 前端：弹窗模型改三列行列表（`grid-cols-[minmax(0,1fr)_minmax(0,1fr)_110px_30px]`：别名输入/真实名输入/上下文下拉默认200K 含 8K~10M 主流值/× 删除），`＋ 添加模型` 按钮，移除 m-mt
- [x] 前端：已注册模型 chip 加上下文徽标；服务商列表摘要加 context（非默认才显示）
- [x] mock/e2e/文档：config.mock.json、config.json 移除 max_tokens；e2e 断言改「原样透传 32000」（25 项不变）；README/CLAUDE.md 更新（context 字段、D9 移除）
- [x] 验收：build_ui.py 重编译（16119B CSS，新类含 grid-cols-[...]）→ cargo test **18 全绿** → node --check ✓ → **e2e 25 断言全绿**（debug+release 双跑）→ 手动实测 model_details + context 往返（PUT 后 128000 写盘、默认值保持字符串）→ release 2.30MB（2,306,560B）部署 8787（新 PID 19648）→ 线上 /api/status model_details ✓、新 UI 标记在位（srv-model/max_tokens 已绝迹）

### Phase 10 — 全部按钮美化 + 删除服务商确认弹窗重设计 【in_progress】
- 用户需求：① **所有按钮设计好看点**（图标 + 渐变 + 阴影 + 微交互）；② **删除服务商时确认弹窗重新设计好看点**（通用确认弹窗升级：彩色图标圆 + 标题/消息/按钮布局）。
- **按钮组件层**：`<style id="ui">` 手写 `.btn` 组件 CSS（inline-flex 图标 + 渐变 solid 变体 green/orange/blue/indigo/red/zinc + ghost/danger-ghost + btn-sm/btn-icon + active scale + focus-visible ring + hover 阴影）；JS 加 Feather 风格 SVG 图标映射 `S` + `svg()` 帮助函数（check/refresh/file/term/copy/plus/edit/trash/warn/info）。
- **删除服务商改 confirmAction(red)**：升级通用确认弹窗（图标圆随颜色：red/orange=warn、blue/indigo=info、green=check）；**顺带修复删除不持久化 bug**（原生 confirm + 仅内存 splice → confirmAction 确认后 PUT /api/config）；最后一个服务商拦截（至少保留一个）。
- [x] 按钮组件 CSS + SVG 图标映射
- [x] 顶部 5 按钮 / 添加服务商 / 弹窗按钮 / 列表编辑删除 / 添加模型 全部应用
- [x] 确认弹窗重设计 + 删除服务商走 confirmAction(red) + 持久化
- [x] 验收：build_ui.py（14708B CSS，含 ui 块 2392B 未动）→ node --check ✓ → cargo test **18 全绿** ✓ → e2e 25 断言（debug+release 双跑）✓ → release 2.31MB（2,314,752B）部署 8787（新 PID 13784）→ 线上 `<style id=ui>`/btn-*/confirm-icon/delProvider 在位、`confirm(`/m-mt 绝迹 ✓

### Phase 11 — 「添加服务商」按钮优化为主 CTA 【complete】
- 用户需求：**添加服务商按钮也优化下**（Phase 10 后遗留的小号 ghost 按钮，与实心渐变按钮语言不协调）。
- **方案**：`btn-sm btn-ghost` → `btn-solid btn-green`（渐变绿实心 + plus 图标，常规尺寸），成为服务商区醒目主 CTA；与顶部保存配置（同样绿色系）呼应，视觉语言统一。
- [x] 按钮类切换 + plus 图标保留
- [x] 验收：build_ui.py（14289B CSS）→ cargo build --release → 部署 8787（新 PID 18952）→ 线上 add-provider = `btn btn-solid btn-green` ✓

### Phase 12 — 添加模型按钮风格统一 + 顶部按钮单行 + 服务商列表单行截断 【complete】
- 用户需求：① **弹窗内「添加模型」按钮也优化，保持风格一致**（`btn-sm btn-ghost` 小号白底与实心渐变不协调 → `btn-solid btn-green`，与「添加服务商」一致）；② **首页顶部 5 个按钮放在一排**（`flex-wrap` → `flex-nowrap` + 新增 `.btn-bar` 紧凑变体 12px/`padding .45rem .7rem`，容器 `overflow-x-auto` 兜底窄窗口滚动）；③ **服务商列表模型、API 摘要保持一行，显示不完用 `…` 截断**（`break-all` → `truncate` + `title` 悬停显示全文）。
- [x] 弹窗「添加模型」→ `btn btn-solid btn-green`
- [x] 顶部 5 按钮 `.btn-bar` + 容器 `flex-nowrap overflow-x-auto` + toast `shrink-0`
- [x] 服务商列表摘要 `truncate` + `title`（悬停全文）
- [x] 验收：build_ui.py（14320B CSS，truncate/overflow-x-auto/shrink-0/flex-nowrap 全编译）→ node --check ✓ → release 2.31MB 部署 8787（新 PID 23616）→ 线上 save=`btn btn-bar btn-solid btn-green`、容器 flex-nowrap、m-add-model=`btn-solid btn-green`、truncate+title 在位 ✓

### Phase 13 — 「添加模型」按钮调小 + 换色与保存区分 【complete】
- 用户需求：**添加模型按钮再适当调小一点，换一个颜色与保存（绿色）区分开**。
- **方案**：`btn btn-solid btn-green` → `btn btn-sm btn-solid btn-blue`（`btn-sm` 缩小 + 蓝色渐变与绿色保存按钮区分；plus 图标保留）。
- [x] 按钮类切换（btn-sm + btn-blue）
- [x] 验收：build_ui.py（14320B）→ node --check ✓ → release 2.31MB 部署 8787（新 PID 12432）→ 线上 m-add-model=`mt-1.5 btn btn-sm btn-solid btn-blue` ✓

### Phase 14 — 修复默认模型下拉不可用（.hidden class 误用） 【complete】
- 用户报告：**默认模型不能选择**（点开无反应）。
- **根因**：`#dm-drop` 把 `hidden` 写进了 **class 列表**（Tailwind `.hidden{display:none}` 永远生效），而 JS `openDm()` 只设置 `hidden` **属性**（`$('dm-drop').hidden=false` 移除属性）→ class 仍强制 display:none → 下拉永远不显示。三个 overlay（modal/confirm）都正确用属性形式，仅 `dm-drop` 踩坑。
- **修复**：`class="hidden absolute ..."` → `class="absolute ..."` + `hidden` 属性（`[hidden]{display:none!important}` 规则控制显隐，与 overlay 同机制）。
- [x] 修复 markup（class hidden → 属性 hidden）
- [x] 验收：build_ui.py（14320B）→ node --check ✓ → release 2.31MB 部署 8787（新 PID 13836）→ 线上 dm-drop = `class="absolute ..." hidden`、无 `.hidden` class ✓

### Phase 15 — 默认模型下拉去掉搜索 【in_progress】
- 用户需求：**默认模型选择不用搞搜索**（模型列表短，搜索框多余）。
- **方案**：移除 `#dm-search` 输入框及其所有 JS 引用——`openDm` 不再 focus/过滤、`closeDm` 不再清空、`renderDmList` 去掉 filter 参数、删除 input 事件绑定；保留富选项行（服务商徽章+别名+真实名+上下文徽标）与「未设置」项。
- [x] HTML 移除搜索框
- [x] JS 清理 dm-search 引用（openDm/closeDm/renderDmList/事件绑定）
- [x] 验收：node --check ✓ → build_ui.py（14282B）→ release 2.31MB 部署 8787（新 PID 14548）→ 线上 dm-search 绝迹、dm-list + hidden 属性在位 ✓

### Phase 16 — 托盘图标修复 + 菜单升级（设置 / 运行停止 / 退出）+ 后端暂停机制 【complete】
- 用户报告：**软件运行后托盘没有图标；右键没有 退出/设置/运行|停止（运行|停止按状态显示）**。
- **根因**：① `TrayIconBuilder::new().build()` 未提供 `.with_icon()`——tray-icon 0.20 无内置默认图标，build 直接 Err → 返回无托盘；② 即使建成功，Windows 需要**消息泵**（GetMessageW/DispatchMessageW）让托盘隐藏窗口处理 WM_TRAYICON/WM_COMMAND，否则图标不显示、右键菜单无响应（旧代码线程阻塞在 MenuEvent::receiver().recv()，从不泵消息）。
- **修复**：① 程序化生成 32×32 RGBA 图标（绿圆+白闪电，`Icon::from_rgba`）并 `.with_icon()`；② 托盘线程改跑 Windows 消息泵（GetMessageW 阻塞、DispatchMessageW 后 try_recv 菜单/图标事件）；③ 菜单=设置/停止服务|启动服务（随 paused 切换文字）/退出，双击图标打开配置页。
- **后端暂停机制**：AppState 加 `paused: Arc<AtomicBool>`；proxy forward 顶部若 paused 返回 503（仅 /v1/* 暂停，Web UI/healthz 仍可用）；`/api/status` 返回 paused；main.rs 创建并传 `paused.clone()` 给 tray；Cargo.toml 加 `windows-sys`（Win32_UI_WindowsAndMessaging）。
- [x] 后端：AppState.paused + proxy 503 + /api/status paused + main.rs + windows-sys 依赖
- [x] 托盘：make_icon + .with_icon + 消息泵 + 菜单（设置/停止启动/退出）+ 双击开配置页
- [x] UI：状态徽标按 paused 显示「⏸ 已停止（托盘可恢复）」
- [x] 验收：cargo check ✓ / cargo test **19 全绿**（+1 `paused_returns_503`）✓ / e2e 25 断言 ✓ / build_ui.py（14282B）✓ / release 2.31MB 部署 8787（新 PID 15200）→ /api/status `paused:false` ✓ / **托盘初始化实测**（8789 临时实例捕获 stderr：`[tray] 托盘已就绪` 日志出现 = 图标生成+build+消息泵全部成功，托盘图标曾短暂出现）✓

### Phase 17 — 停止服务全站不可访问 + 托盘菜单改「停止/启动」两字 【complete】
- 用户反馈：① **为啥停止服务后浏览器还能访问** → 期望「停止服务」后整站（Web UI/healthz/转发）都不可访问；② **只需要显示停止、启动两个字**。
- **修复**：① api.rs 加 `paused_guard` 中间件（`middleware::from_fn_with_state`，paused 时**所有路由**含 `/`、`/healthz`、`/api/*`、`/v1/*` 一律 503），浏览器访问即失败；恢复只能靠托盘菜单「启动」（独立线程，不受 HTTP 影响）；proxy 内 503 检查保留（单测仍直接覆盖）。② tray.rs 菜单项文字 `停止服务/启动服务` → **`停止/启动`**。③ Web UI 状态徽标因停止时页面不可达，回退简单「● 运行中/○ 未运行」。
- [x] 后端：api.rs paused_guard 中间件 + 应用到 build()
- [x] 托盘：菜单文字 停止/启动（含 ready 日志）
- [x] UI：状态徽标回退（去掉暂停分支，页面停止时不可达）
- [x] 验收：cargo test **20 全绿**（+1 `paused_blocks_whole_site`：/ /healthz /api/status /v1/models 全 503）✓ / e2e 25 断言 ✓ / build_ui.py（14282B）✓ / release 2.31MB 部署 8787（新 PID 19412）→ 运行态 /healthz 200、/api/status paused:false、/ 200 ✓ / 托盘 ready 日志含「停止|启动」✓

### Phase 18 — 停止=真正停掉服务（释放端口，浏览器显示服务不存在） 【in_progress】
- 用户反馈：**我要的停止是浏览器直接访问时显示服务不存在，是停掉整个服务**——不是 503 页面，是端口关闭、连接被拒。
- **方案**：去掉「paused 标志 + 503 守卫」思路，改为**托盘直接驱动服务循环**：
  - `main.rs` 抽出 `run_server` 无限循环：`paused=false` → `bind_with_retry` + `axum::serve`（`tokio::select!` 监听停止信号）；`paused=true` → 等待「启动」信号后重新 bind。停止时 select 的 serve future 被 drop → **监听器释放 → 端口关闭 → 浏览器连接被拒（服务不存在）**。
  - 状态承载从 `Arc<AtomicBool>` 改为 **`tokio::sync::watch::Sender<bool>`**（托盘线程 send，服务循环 borrow/await changed）；`borrow_and_update` 丢弃陈旧唤醒，避免 stop→start 快速连点导致误停。
  - 绑定失败策略：首次启动失败 `exit(1)`；托盘重启路径失败 1s 重试（不 exit，用户可「停止」放弃）。
  - **删除**：`paused_guard` 中间件 + `paused_blocks_whole_site` 测试（api.rs）、proxy forward 顶部 paused 503 检查 + `paused_returns_503` 测试（proxy.rs）、`/api/status` paused 字段（webui.rs）、`AppState.paused`（config.rs）、tower dev-dependency。
- [x] main.rs：run_server 循环 + watch + bind_with_retry 改 Result + 托盘停启测试
- [x] tray.rs：spawn 签名改 watch::Sender<bool>
- [x] 移除 paused 标志与 503 守卫（api/proxy/webui/config/Cargo.toml）
- [x] 验收：cargo check ✓ / cargo test **19 全绿**（+1 `tray_stop_releases_port_and_start_rebinds`：真实端口上 send(true)→连接被拒、send(false)→恢复）✓ / e2e 25 断言 ✓ / release 2.31MB 部署 8787（新 PID 15440）→ healthz 200、/ 200、/api/status `running:true` 且 **paused 字段已移除** ✓
- **Phase 18 ✅。**

### Phase 19 — 开机自启配置项 + 双击隐藏黑框 + 打开配置页 【in_progress】
- 用户需求：① **增加开机自启配置项**，配置后开机自动启动；② **双击运行 claude-relay.exe 后关闭黑色命令行框**；③ **打开配置页面**。
- **方案**：
  - **开机自启**：`ServerConfig` 加 `auto_start: bool`（默认 false）；新建 `autostart.rs` 用 windows-sys `Win32_System_Registry` 写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（值 `ClaudeRelay` = `"<exe>" --autostart`）。Web UI 服务设置区加开关，保存即写注册表（config_put 里 apply）；启动时同步一次（配置即真相）。
  - **隐藏黑框（双击）**：main 启动最先调 `hide_console_if_double_clicked()` —— `GetConsoleProcessList` 返回 1（独占控制台 = Explorer 双击/`start` 新建控制台）→ `ShowWindow(SW_HIDE)`。终端/cmd 启动共享控制台（n≥2）→ 保留日志输出。只隐藏不 `FreeConsole`，stdout/stderr 仍有效（写隐藏控制台，不会 panic）。
  - **打开配置页**：`--autostart` 标志=后台启动（开机自启/start-claude.cmd 传，不弹浏览器）；非 autostart 且**双击场景**（gui_launch）→ run_server 首次 bind 成功后 `open_browser(http://addr)`；若检测到端口已被占用（relay 已在运行）→ 双击则打开配置页后退出，否则直接退出。
  - **config 路径回退**：`--config` 未指定时先 CWD/config.json，不存在则回退 exe 同目录（开机自启 CWD=System32 也能找到配置）。
  - **start-claude.cmd**：启动行加 `--autostart`（经脚本启动不弹浏览器）。
  - Cargo.toml windows-sys 加 feature `Win32_System_Console` + `Win32_System_Registry`。
- [x] autostart.rs + config.auto_start + webui apply
- [x] main.rs 控制台隐藏 + --autostart + 配置回退 + 已运行检测 + 双击开配置页
- [x] tray.rs 暴露 open_browser
- [x] UI 开关 + build_ui.py + start-claude.cmd
- [x] 测试 + 构建 + 部署 8787 实机验证
- **验收**：cargo check ✓ / cargo test **20 全绿**（+1 `server_auto_start_roundtrip`）✓ / e2e 25 断言 ✓ / node --check ✓ / build_ui.py（14538B）✓ / release 2.32MB 部署 8787（新 PID 20632，--autostart 启动）✓ / **注册表实测**：auto_start=true 启动后 `HKCU\...\Run\ClaudeRelay` = `"<exe>" --autostart` ✓，PUT /api/config(auto_start=false) 后值删除 ✓ / **双击模拟**（cmd start 独立控制台）已运行场景：打开配置页+干净退出，仅剩 20632 ✓ / `--print-default-model` 输出 ds-deepseek-v4-flash ✓
- **Phase 19 ✅。**

### Phase 20 — 修复「保存并重启杀程序」+ toast 提示移到按钮下方 【complete】
- 用户反馈：① **为啥保存并重启直接杀掉程序了**；② **按钮点击的文字提示都放在按钮下方**（现在是显示在按钮后面挤压不美观）。
- **修复①（重启杀程序回归）**：根因 = Phase 19 加的 `port_is_busy` 检测在**重启场景误杀**——/api/restart 拉起的**新进程**启动时检测到**旧进程还在占用端口**（600ms 后才退出）→ 误判"已有 relay 在运行"→ 直接退出；旧进程随后退出 → 服务整体被杀（线上 8787 确实因此被用户点挂）。修复：`webui::restart` spawn 参数追加 `--restart`；main 解析 `--restart` 后**跳过 port_is_busy 检查**（由 bind_with_retry 20×150ms 扛过新旧交接），且不打开配置页。parse_args 从 6 元组改为 **Args 结构体**。
- **修复②（toast 位置）**：`#toast` 从按钮行内（`ml-auto` 挤压按钮）移出，放卡片内**按钮行下方独立行**（`mt-2 min-h-[18px] flex items-center`，min-h 保底防页面跳动）；toast() JS 同步新 className。
- **验证**：cargo check ✓ / cargo test **20 全绿** ✓ / e2e 25 断言 ✓ / build_ui.py（14540B）✓ / release 2.32MB 部署 8787 ✓ / **POST /api/restart 实测：返回 `{ok,restarting}` → 3s 后 healthz 200、新进程 `--autostart --restart` 正常接管（修复前此操作会杀服务）** ✓ / 线上 toast div 在按钮行外 ✓
- **Phase 20 ✅。**

### Phase 21 — 空配置首次运行即可启动 + 服务商可删空 + 弹窗内字段校验 【complete】
- 用户需求：① **首次运行无 config.json 时默认生成 `{"server":{host,port,separator,default_model}}` 并启动**（此前空配置校验失败直接 exit，双击黑框隐藏后表现为"没反应"）；② **服务商可全部删除**，去掉"只有一个时不允许删除"的校验；③ **添加/修改服务商的表单字段校验体现在弹窗里**，而不是主界面 toast（弹窗遮罩下根本看不到）。
- **方案**：
  - ① `config.rs::validate()` 移除 `providers.is_empty()` 致命错误（空服务商=合法，可去 UI 配置）；`Config.providers` 加 `skip_serializing_if="Vec::is_empty"` → 默认生成的 config.json 恰为 `{"server":{...}}` 无 providers 键。main.rs 启动 banner 无别名时提示"打开配置页添加服务商"。空 providers 时 `/v1/messages` 由 router::resolve 返回 400 "no provider prefix matches"（无 panic，proxy 已优雅处理）。
  - ② UI：`delProvider` 删除 `providers.length<=1` 拦截；`save()/saveRestart()/updateSettings()` 删除 `!body.providers.length` 拦截（后端已放行）。
  - ③ UI：弹窗内加 `#m-error` 红色提示行（`hidden` 属性控制）+ `modalError()`/`fieldError()`（fieldError 高亮对应输入框 border+focus）；`modalSave()` 的 ID 空/重复、Base URL 空、新服务商掩码 Key、Key 空改走 fieldError；后端保存失败 `r.error` 走 modalError（弹窗保持打开）。openModal 重置错误与高亮。
- [x] 后端：validate 放行空服务商 + providers 序列化省略空数组
- [x] UI：删空放行 + 弹窗内校验
- [x] 测试 + build_ui.py + release + 空目录首次启动实机验证
- **验收**：cargo test **21 全绿**（+1 `empty_providers_is_valid_and_serializes_without_key`）✓ / JS node --check ✓ / build_ui.py（14652B）✓ / release 2.33MB ✓ / **空目录首跑实测**：`_disttest/` 复制 exe → 启动成功（不再"no providers configured"退出）、自动生成 `{"server":{host,port,separator,default_model}}`（无 providers 键）、banner 提示"未配置服务商—打开配置页添加"、healthz 200、`/api/config` 返回空 server 配置 ✓ / **删空实测**：PUT `/api/config` 空 providers → `{ok:true,providers:0}`，磁盘 config.json 干净无 providers 键 ✓ / 空服务商下 `/v1/messages` → 400 "no provider prefix matches"（不 panic）✓ / 弹窗 `#m-error` + `fieldError` 上线（遮罩下可见）✓
- **⚠️ 配置事故**：重启生产 relay 时发现项目 config.json **在本会话开始前已是空默认**（mtime 16:54，早于我 16:59 的构建；我的测试只写 `_disttest/`）。重启后新进程读到空配置 → 服务商为 0。已从用户主 `~/.claude/settings.json`（env 直接指向 `https://api.deepseek.com/anthropic`）提取真实 Key **重建 config.json**（id=ds、base_url=deepseek anthropic 端点、models=[deepseek-v4-flash, deepseek-v4-pro]、default=ds-deepseek-v4-flash），经 `/api/restart` 热接管。**实测**：`/v1/messages/count_tokens` → 200 `{input_tokens:84}`（Key 与路由验证通过）、`/api/status` providers=1、`unknown-model` → 400 且列出可用别名 ✓。生产 relay 现为新构建（PID 5804）。
- **Phase 21 ✅。**

### Phase 22 — 「当前使用模型」+ 哨兵路由 claude-relay-model + 上下文后缀拼接 【in_progress】
- 用户需求：① **设置默认模型 → 改为设置「当前使用模型」**；② **CC 设 `claude-relay-model` 时用配置里选的模型**（哨兵）；③ **CC 设了具体模型（非哨兵）就用它**；④ 设置模型时**自动把上下文拼到后面**（`ds-v4-flash[1m]`），点「更新 claude 配置 / 环境变量」写入。用户已确认：**写 `claude-relay-model[1m]`（哨兵+上下文后缀），且顶层 `model` 键 + `env.ANTHROPIC_MODEL` 都写**（实测会话用的是顶层 model 键，仅写 env 不生效）。
- **方案（哨兵路由 + `[ctx]` 剥离）**：
  - `router.rs`：`RELAY_MODEL="claude-relay-model"` 常量；`split_ctx_suffix()` 剥尾部 `[xxx]`（`claude-relay-model[1m]`→base `claude-relay-model` + suffix `1m`）；`resolve()` **先剥后缀再前缀匹配**，`Resolved.alias` 保留**原始完整模型串**（SSE 回写原样）；新 `resolve_for_request(&Config, model)`：剥后缀→base==哨兵→用 `cfg.default_model()`（当前模型）替换后 resolve、alias 覆盖为原始哨兵串→否则正常 resolve（可单测）。
  - `config.rs`：`fmt_ctx_suffix(ctx)`（1e6 整倍→`[Nm]`、千整倍→`[Nk]`、否则四舍五入 k）；`ctx_suffix_of(full_alias)` 查当前模型 context；`current_ctx_suffix()`；`sentinel_model()` = `claude-relay-model{后缀}`。`server.default_model` 字段名保留（向后兼容），语义变为「当前使用模型」。
  - `proxy.rs`：`forward` 改用 `resolve_for_request`；debug 日志加哨兵替换行。
  - `webui.rs update_settings`：写 `doc["model"]`（顶层）+ `env.ANTHROPIC_MODEL` = `sentinel_model()`；响应带 model 供前端 toast。
  - `main.rs --print-default-model`：改打印 `sentinel_model()`（start-claude.cmd 提示用），help 文案同步。
  - UI：服务设置区标签「默认模型」→「当前使用模型」+ 说明文案；`updateSettings()`/`copyEnv()` 显示/输出哨兵值（JS 按所选模型 context 算后缀 `[1m]`/`[200k]`…，与后端一致）。
- [x] router.rs：RELAY_MODEL + split_ctx_suffix + resolve 剥后缀 + resolve_for_request + 单测
- [x] config.rs：fmt_ctx_suffix + ctx_suffix_of + current_ctx_suffix + sentinel_model + 单测
- [x] proxy.rs：改用 resolve_for_request + debug 日志
- [x] webui.rs update_settings：顶层 model + env 都写哨兵值
- [x] main.rs --print-default-model → sentinel_model + help 文案
- [x] UI：标签/说明 + updateSettings/copyEnv 哨兵值
- [x] e2e.py：新增哨兵/`[1m]` 断言；build_ui.py + cargo test + node --check + release
- **验收**：cargo test **29 全绿**（原 21 + 新 8：router 6 + config 2）✓ / e2e **32 断言全过**（原 25 + 新 7：`claude-relay-model[1m]`→mock-chat、裸哨兵、`mock-mock-chat[1m]` 剥后缀、哨兵 SSE 回写 `claude-relay-model[1m]`）✓ / node --check ✓ / build_ui.py（14652B CSS 不变）✓ / release 2.33MB ✓ / **真机 DeepSeek 实测（8890 临时实例，项目 config.json 真实 Key）**：`claude-relay-model[200k]`→哨兵→ds-deepseek-v4-flash→上游 `deepseek-v4-flash` 200 `{input_tokens:84}`；`ds-deepseek-v4-flash[1m]`→剥后缀→200；裸 `claude-relay-model`→200。debug 日志三行哨兵/剥后缀均正确 ✓ / **部署**：新 exe 已复制到项目根；8787 当前无 relay 进程（其真实 hs/xf 配置在项目外），由用户替换部署后经 UI 选「当前使用模型」+「更新 claude 配置」写哨兵。
- **Phase 22 ✅。**

### Phase 23 — 修复「改当前使用模型不生效」：选中即持久化 + 哨兵回写真实模型 【complete】
- 用户实测报告：**已 `/model claude-relay-model[1m]`、页面选了别的模型，却没按选择的模型转发**。重新确认需求：CC 通过 `/model` 设 `claude-relay-model`（或 `[1m]`/`[200k]`）时用**设置页当前选择的模型**（**已运行 claude 时在页面改选择也要热重载即时生效**）；非哨兵则按 `/model` 实际设置。
- **诊断**：8787 生产 relay（`C:\Users\dcbxfy\AppData\Local\Programs\claude-relay\`）已运行 Phase 22 新构建（哨兵 count_tokens 200 证明）。config 里 default_model=`xf-glm-5.1`（用户最后一次保存值）。**根因 = ①「当前使用模型」下拉选中只改内存 `defaultModel` 变量、不持久化**——必须再点「保存配置」才写盘，用户改了选择没保存 → relay 仍用旧 default_model；② 即便生效，哨兵路径 SSE 回写的是占位名 `claude-relay-model[1m]`，CC 里看不到实际用的模型，误以为没按选择转发。
- **修复**：
  - ① `webui/index.html selectDefault()` 改 **async + 选中即 PUT /api/config**（热重载，claude 运行中也可切换，无需再点保存）；失败回滚重载；成功 toast「已切换为 X」。提示文案加「选中即保存生效（热重载）」。
  - ② `router.rs resolve_for_request` 哨兵分支 `r.alias` 从回写哨兵串改为 **`当前模型完整别名 + 上下文后缀`**（如 `xf-glm-5.1[200k]`）→ SSE 回写让 CC 显示实际用的模型。对应单测与 e2e 断言更新。
- [x] 诊断（进程/端口/配置/哨兵探测）
- [x] router.rs 哨兵回写当前模型（+单测更新）
- [x] UI selectDefault 选中即持久化 + 提示文案
- [x] e2e 断言更新 + 全量验证 + release 构建
- [x] 部署：替换 `C:\Users\dcbxfy\AppData\Local\Programs\claude-relay\claude-relay.exe` + 项目根 exe，重启 8787，清理残留 8890
- **验收**：cargo test **29 全绿** / e2e **32 断言全过**（含「哨兵 SSE 回写当前模型 mock-mock-chat[200k]」）/ node --check ✓ / build_ui.py ✓ / release 2.33MB ✓ / 8787 重启后 healthz 200、config 完好（default_model=xf-glm-5.1、providers ds/hs/xf）、哨兵 count_tokens 200 ✓。
- **Phase 23 ✅。**

### Phase 24 — CC 任务槽全指哨兵：修子代理/compact 裸模型名 400 【complete】
- 用户报告：`/model claude-relay-model[xx]` 后**输入指令先报错再正常**——`Agent "Explore codebase structure" failed: API Error: 400 unknown model 'claude-opus-5': no provider prefix matches`；`/compact` 也报 `unknown model 'deepseek-v4-flash'`（裸名、无 `[ctx]` 后缀）。
- **根因**：Claude Code **多槽位架构**——`/model` 只设主对话；子代理（Explore/Plan/审查）、`/compact`、`/loop` 走独立的 **opus/sonnet/haiku 任务槽**，自定义 base_url 下默认发**内置裸名**（`claude-opus-5` 等）→ 中继只认 `{id}-{别名}` 或哨兵 → 裸名 400。主对话哨兵正常（中继换页面模型），故呈现「先报错、后正常」。详见 findings §2.25。
- **修复（用户确认：三个槽全指哨兵，跟随页面模型）**：
  - `webui.rs update_settings` 额外写 `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` = 哨兵值（env 三键 → 六键 + 顶层 model）。
  - `webui/index.html`：copyEnv 复制命令加三槽 set 行；updateSettings 确认文案提示任务槽。
  - `start-claude.cmd`：用 `--print-default-model` 的哨兵值 set 三个槽 env（`if defined DEFAULT_MODEL (...)` 块）。
- [x] 诊断（子代理 400 根因 = 任务槽裸名，非主对话问题）
- [x] webui.rs + index.html + start-claude.cmd 三处写三个槽
- [x] 验证：cargo test **29 全绿** / e2e **32 断言全过** / node --check ✓ / build_ui.py ✓ / release 2.33MB ✓ / **临时 CLAUDE_CONFIG_DIR 实测** update_settings 落盘：顶层 model + ANTHROPIC_MODEL + 三槽全部 = `claude-relay-model[200k]` ✓
- [x] 部署：替换 deployment + 项目根 exe，重启 8787（PID 3488），healthz 200、providers 3 完好、哨兵 count_tokens 200 ✓
- **用户应用**：经 start-claude.cmd 启动 → **重开 CC 即带三槽**；用主 settings.json → 点一次「更新 claude 配置」（写入三槽）。子代理/`/compact` 不再 400，且跟随页面「当前使用模型」。
- **Phase 24 ✅。**

### Phase 25 — 托盘「模型」子菜单：模型 → 服务商 → 模型，选中即热重载当前使用模型 【in_progress】
- 用户需求：托盘图标右键菜单新增「模型」——悬停先展开**服务商**，再悬停到服务商展开其**模型**；选中模型**立即重载生效**（走所选服务商模型）；前提=CC 当前用 `claude-relay-model` 哨兵，否则 CC 继续走指定模型，切到哨兵后自动走所选模型。
- **本质**：这就是「当前使用模型」（`server.default_model`）的**托盘入口**——哨兵路由（D22）已天然满足前提（哨兵→用 default_model；指定模型→继续走指定）。托盘选中 = 设 `server.default_model` → 写盘 → 热重载（Arc<RwLock> 原地替换），与 Web UI「当前使用模型」下拉同一语义、零重启。
- **设计**：
  - `config.rs`：`AppState::set_default_model_blocking(full)`——`router::resolve` 校验完整别名可路由 → `Config::save` → `*blocking_write = cfg`（**阻塞版供 std 托盘线程用**；tokio RwLock 的 blocking_write 在异步上下文会 panic，托盘是独立 std 线程故安全）。
  - `tray.rs`：`spawn` 增参 `state: Arc<AppState>` + `model_menu: Vec<(服务商显示名, [(完整别名, 别名)])>` + `current` 快照；顶层「模型」`Submenu` 内每个服务商一个 `Submenu`，模型用 `CheckMenuItem`（当前模型 checked）；事件循环 `MenuEvent` 命中模型 id → `set_default_model_blocking` → 遍历刷新 checked。空服务商占位「（未配置服务商）」。选中不改 CC 自身模型，仅当 CC 发哨兵时生效（debug 日志点明）。
  - `main.rs`：从 cfg 构建快照（provider name 空则用 id；完整别名=id+sep+alias）+ `cfg.default_model()`，`tray::spawn(base_url, paused_tx, state.clone(), models, current)`。
- muda 0.16.1 API 确认：`Submenu::with_id(id,text,enabled)->Self`（非 Result）、`Submenu::append(&dyn IsMenuItem)`、`CheckMenuItem::with_id(id,text,enabled,checked,accelerator)` + `set_checked(bool)`、`Menu::append` 收 &Submenu/&CheckMenuItem（IsMenuItem）。tokio 1.53 `RwLock::blocking_write` 确认。
- **单测**：`set_default_model_blocking_updates_and_saves`（临时配置文件：设模型→内存+落盘断言→哨兵跟随→不可路由拒绝且不改）。
- [x] config.rs 核心 + 单测
- [x] tray.rs 菜单 + 事件
- [x] main.rs 快照 + stub
- [x] 构建/测试/部署 + 文档
- **验收**：cargo test **30 全绿**（+1）/ e2e **32 断言全过** / release 2.34MB（+8KB）/ e2e 日志确认 `托盘已就绪：右键=设置/模型/停止|启动/退出`（菜单构建成功）/ 已部署 8787（PID 18952，healthz 200、providers 3、哨兵 count_tokens 200）✓。**用户手动验证**：托盘右键「模型」→ 服务商 → 模型 选中，CC 用哨兵时下一请求即走所选模型。
- **Phase 25 ✅。**

### Phase 26 — 服务商使用情况：用量统计（全服务商）+ DeepSeek 余额（Web UI 展示） 【in_progress】
- 用户需求：「要是能显示各服务商的使用情况就好了（余额、调用量、剩余量）」。
- **调研（findings §2.27）**：余额无统一接口——DeepSeek `GET https://api.deepseek.com/user/balance`（同款 key，真机实测 ¥5.39）可做；火山/讯飞需各自 OpenAPI（AK/SK）不可简单查。**调用量/用量中继可从自己的转发流统计**（SSE `message_start.message.usage.input_tokens` / `message_delta.usage.output_tokens` / count_tokens `input_tokens` / 请求次数），所有服务商通用、零适配。
- **用户确认**：用量统计 + DeepSeek 余额（自动识别）；展示在 Web UI 配置页。
- **设计**：
  - `config.rs`：`UsageStats { providers: HashMap<id, ProviderUsage> }`；`ProviderUsage { requests, input_tokens, output_tokens, by_model }`、`ModelUsage`；`AppState.usage: Arc<Mutex<UsageStats>>`（std Mutex 短锁，relay 单线程 + 托盘只读，无 await 持有）。
  - `proxy.rs`：resolve 后 `requests++`（provider + model，model 键 = `split_ctx_suffix(resolved.alias).0` 剥 `[ctx]`）；**SSE 统一走带用量提取的缓冲流**（ModelRewriter 扩展：`data:` 行含 `usage` 子串 → 慢路径解析，`message_start` 累加 `message.usage.input_tokens`、`message_delta` 累加 `usage.output_tokens`；含 usage 的行跳过字节快路径避免漏计）；count_tokens 响应解析 `input_tokens`；非 SSE `/v1/messages` 读体解析顶层 `usage`。
  - `webui.rs`：`/api/status` 增 `usage` 数组（**全部已配置服务商**，无记录填 0）；`GET /api/balance` 新端点——`base_url` 含 `api.deepseek.com` → 剥 `/anthropic` 取主机根 → `GET {root}/user/balance`（同 key Bearer，8s 超时）；其余返回「无简单余额接口（需服务商 OpenAPI）」。
  - `api.rs`：挂 `/api/balance` 路由。
  - `webui/index.html`：新增「使用情况」卡片——每服务商 请求数/输入/输出 tokens + 余额（DeepSeek 显示金额/可用，其余提示无接口）+「刷新余额」按钮；提示「自启动以来累计，重启清零」。
- **测试**：单测（ModelRewriter 喂 SSE 断言 usage 累加、`balance_url_of` 剥根）；e2e（`/api/status` 含 usage 字段、`/api/balance` 200 且 mock 服务商返回无接口）。
- [x] config.rs 用量结构 + AppState.usage
- [x] proxy.rs 请求计数 + SSE/count_tokens/非SSE 用量提取
- [x] webui.rs /api/status.usage + /api/balance + api.rs 路由
- [x] index.html 使用情况卡片
- [x] 单测/e2e/构建/部署 + 文档
- **验收**：cargo test **33 全绿**（+3：proxy 用量×2 + webui balance_root）/ e2e **34 断言全过**（+2：status.usage、balance）/ node --check ✓ / build_ui.py（14901B）✓ / release **2.37MB**（+30KB）/ **真机 8892 临时实例**：status.usage 对哨兵 count_tokens 计 xf requests=1 ✓、balance 的 ds 返回真实余额 ¥5.10、hs/xf 返回无简单接口 ✓ / 已部署 8787（healthz 200、usage 覆盖 3 服务商、ds 余额 ¥5.10 实时）✓。
- **Phase 26 ✅。**

### Phase 27 — 用量统计持久化：服务重启不清零 【complete】
- 用户需求：**服务重启后 tokens 统计不要清零**（Phase 26 的用量是进程内存，重启归零）。
- **设计**：
  - `config.rs`：`UsageStats/ProviderUsage/ModelUsage` 加 `Serialize/Deserialize`；`UsageStats::save(path)`（**原子写**：`.tmp` + rename，防重启交接瞬间两进程并发写坏文件）+ `load(path)`（缺失/损坏 → 空统计启动，不崩）。
  - `AppState` 加 `usage_dirty: Arc<AtomicBool>`（脏标记，空闲零 IO）；`proxy.rs` 每处累计/计数置脏。
  - `main.rs`：启动时从 `{config_path 同目录}-usage.json`（如 `config-usage.json`）加载进 `AppState.usage`；spawn **周期落盘任务**（current_thread runtime 内 `tokio::spawn`，5s 间隔，dirty 才写）。三个 AppState 构造点（main×2 + config 测试）同步加字段。
  - `.gitignore` 加 `*-usage.json`。
- [x] config.rs serde + load/save + AppState.usage_dirty
- [x] proxy.rs 置脏
- [x] main.rs 加载 + 周期落盘 + 构造点
- [x] e2e/单测/构建/部署 + 文档
- **验收**：cargo test **43 全绿**（+1 `usage_stats_persist_roundtrip`：缺失→空、roundtrip、损坏→空）/ e2e **41 断言全过**（+2：**用量文件落盘 config.mock-usage.json**、**重启 relay 子进程后用量不清零**）/ release **3.24MB** / 真机 8895：import-cookie 报浏览器锁库（见 Phase 28 注）。
- **Phase 27 ✅。**

### Phase 28 — 火山方舟 Coding Plan 用量（Cookie 自动读浏览器 + 手动粘贴兜底） 【complete】
- 用户需求：查询 `https://console.volcengine.com/api/top/ark/cn-beijing/2024-01-01/GetCodingPlanUsage` 拿到 **5小时/一周/一月** 的用量百分比（响应 `Result.QuotaUsage[]`：`Level` session≈5小时/weekly/monthly + `Percent` + `ResetTimestamp`）；鉴权用 `console.volcengine.com` 域名的 Cookie；**服务重启后用量不清零**（Phase 27 前置）。
- **用户决策：两者都要**——自动读取 Chrome/Edge Cookie 为主，手动粘贴兜底。
- **设计**：
  - `Cargo.toml`：加 `rusqlite`（`bundled`，读浏览器 Cookie 库——首个 C 编译依赖，GNU 工具链 + TDM-GCC 验证通过）+ `aes-gcm`（Cookie 解密）+ `base64`；`windows-sys` 加 `Win32_Security_Cryptography`（`CryptUnprotectData`，类型用 **`CRYPT_INTEGER_BLOB`**——0.59 无 `DATA_BLOB`）+ `Win32_System_Memory`（`LocalFree`，实则在 `Win32::Foundation`）。
  - `config.rs`：`ProviderConfig` 加可选 `console_cookie`（掩码展示 `mask_cookie` 保留尾 8 字符 + PUT 复用，同 D13）+ `console_usage_url`（可选；默认按 base_url 区域推导 `.../ark/{region}/2024-01-01/GetCodingPlanUsage`，`ark.cn-beijing.volces.com` → `cn-beijing`）。
  - `src/cookies.rs`（新模块）：发现 Chrome/Edge 各 profile Cookie 库 → **运行中浏览器锁库（os error 32）→ 复制主库+`-wal`+`-shm` 到临时目录再读-写打开（SQLite 回放 WAL）**；`os_crypt.encrypted_key` → `CryptUnprotectData`（DPAPI）→ AES-128-GCM 解 `v10`（`v11` app-bound 尽力而为，失败跳过）→ `SELECT ... WHERE host_key LIKE '%volcengine.com'` → 拼 Cookie 头。
  - `webui.rs`：`GET /api/coding-plan`（cookie 源 = `console_cookie` 或自动读浏览器；带 Cookie/Referer/UA 头，8s 超时；解析 QuotaUsage → `[{level,label,percent,reset_ts}]`；仅火山方舟类服务商查询）；`POST /api/import-cookie`（自动读 → 存 `provider.console_cookie` → 写盘热重载 → 返回掩码）。
  - `api.rs` 挂路由；`index.html`：服务商弹窗加「火山方舟控制台 Cookie / 用量查询 URL」字段，使用情况卡片加 **Coding Plan 块**（5小时/一周/一月 百分比 + 重置时间 + 「导入 Cookie」按钮）。
  - mock/e2e：mock 加 `GET /GetCodingPlanUsage` 返回用户样例；mock 配置加 `console_cookie`+`console_usage_url` 指向 mock；断言 `/api/coding-plan` 解析、Cookie 头到达、import-cookie 合法响应。
- [x] Cargo.toml 依赖 + cargo check（rusqlite bundled 编译验证 ✓）
- [x] config.rs console 字段 + 掩码/复用
- [x] src/cookies.rs 浏览器 Cookie 读取
- [x] webui.rs /api/coding-plan + /api/import-cookie + api.rs 路由
- [x] index.html 弹窗字段 + Coding Plan 块 + 导入按钮
- [x] mock/e2e/单测/构建/部署 + 文档
- **验收**：cargo test **43 全绿**（+9：cookies 4 + webui 4 + config 1）✓ / e2e **41 断言全过**（+7：落盘/重启保留/coding-plan×4/import-cookie）✓ / node --check ✓ / build_ui.py（15201B）✓ / release **3.24MB**（+0.87MB rusqlite）✓ / **DPAPI 真机 roundtrip 实测通过** ✓ / **真机 8895**：import-cookie 正确报「浏览器运行中锁定 Cookie 库」→ 手动粘贴/关浏览器后导入兜底 ✓。
- **⚠️ 关键发现（findings §2.30）**：新版 Chrome/Edge **运行中独占锁定 Cookie 库主文件**（os error 32），自动读取只在**浏览器关闭时**或非运行 profile 可用——手动粘贴是可靠路径。`console_cookie` 存配置，UI 弹窗可贴。
- **用户操作**：使用情况卡 Coding Plan 行点 **「导入」**（自动读，浏览器锁库会失败）或 **「粘贴」**（直达编辑弹窗 Cookie 输入框）——`importCookie` 失败会自动打开弹窗聚焦 Cookie 框 + 弹窗内红字指引；粘贴后保存即生效，重查显示 5小时/一周/一月 用量。
- **Phase 28 ✅。**（含 UX 修复：导入失败→自动打开弹窗聚焦粘贴框 + 行内「粘贴」按钮，2026-08-04）

### Phase 29 — 余额只显示有接口的服务商（隐藏「无简单余额接口」噪音） 【complete】
- 用户反馈：「使用情况」卡片里 hs/xf 的「该服务商无简单余额接口（需服务商 OpenAPI）」两行是噪音，**先隐藏，等它们有 API 支持再放出来**。
- **实现**：后端 `webui.rs::balance()` 改为**只返回有简单余额接口的服务商**（`balance_root()` 识别，当前仅 DeepSeek）——无接口的直接 `continue`，不再产生「无简单余额接口」条目；将来某服务商接入余额 API，在 `balance_root` 加识别即自动出现。前端 `loadBalance()` 列表为空时**整块隐藏**（不再显示「无服务商」）。e2e 断言改「mock（无接口）不在 balance 响应中」。
- **验收**：cargo test **43 全绿** ✓ / e2e **41 断言全过** ✓ / node ✓ / build_ui（15176B）✓ / release 3.25MB / **已部署 8787**：`/api/balance` 仅返回 `['ds']`（DeepSeek 真实余额），hs/xf 隐藏 ✓。
- **Phase 29 ✅。**

### Phase 30 — 讯飞星辰 Coding Plan 用量（maas.xfyun.cn，按 apiKey 匹配多套餐） 【complete】
- 用户需求：像火山方舟一样查讯飞星辰 Coding Plan 用量——`https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`，响应 `data.rows[]`；**根据 rows 里 `codingPlanAppCredentialDTO.apiKey` 判断属于哪个配置的服务商**——配置多个服务商就显示多个套餐。
- **设计**：
  - `cookies.rs` **域参数化**：`read_console_cookie(domain_like, display)` 泛化原 `read_volcengine_console_cookie`；新增 `read_xf_console_cookie()`（`host_key LIKE '%xfyun.cn'`）；`query_cookies/query_cookies_conn` 加 `domain_like` 参数（SQL 占位 `?1`）。
  - `webui.rs` **按服务商类别分派**：新增 `ConsoleKind { Ark, Xf }` + `console_kind()`——base_url 含 `xfyun.cn`/`xf-yun.com` → Xf（讯飞 Anthropic 端点域是 `xf-yun.com`，控制台是 `xfyun.cn`，都算）；含 `volces.com`/`volcengine.com` → Ark；未识别域名按 `console_usage_url` 路径分派（含 `coding-plan/list` → Xf，其余默认 Ark，供 e2e mock）；最后存档 Cookie 默认 Ark（Phase 28 向后兼容）。`coding_plan` handler 按 kind 分流水：ark → 原 GetCodingPlanUsage + `parse_coding_plan`；**xf → `xf_usage_url`（默认官方端点，可配置覆盖）+ `parse_xf_coding_plan`**（按 `codingPlanAppCredentialDTO.apiKey == p.api_key` 过滤 rows，只显示本服务商套餐——**配置多个服务商/多 key → 各显各的**；无匹配 → 明确报错提示核对 apiKey）。响应带 `kind` 字段（ark/xf）。
  - `import_cookie` 按目标服务商 kind 读对应域 Cookie（xf 读 `%xfyun.cn`）。
  - `index.html`：`loadCodingPlan` 按 `c.kind==='xf'` 渲染 `fmtXfPlans`（每套餐一行：套餐名 + 包量 %/剩余 + 5小时/一周 请求配额 + 到期）；标题改「Coding Plan 用量（火山方舟 / 讯飞星辰）」；弹窗 Cookie/URL 字段标签与 placeholder 泛化。
  - e2e：`config.mock.json` 加 `xfmock` 服务商（api_key=`XF_E2E_KEY` 匹配行、console_cookie、console_usage_url 指向 mock `/coding-plan/list`）；mock do_GET 加 xf 端点（两行：一匹配一不匹配）；断言讯飞条目/按 apiKey 过滤只留匹配/套餐名/包量 25.9%/字段/Cookie+Referer 头。
- **验收**：cargo test **46 全绿**（+3：console_kind 分派、xf_usage_url、parse_xf_coding_plan）/ e2e **47 断言全过**（+6 讯飞）/ node --check ✓ / build_ui（15211B）✓ / release **3.25MB**（3,407,872B）。
- **用户操作**：xf 服务商粘贴 `maas.xfyun.cn` 控制台 Cookie（或关浏览器后点「导入 Cookie」）→ 使用情况卡显示该 apiKey 的 Coding Plan 套餐（包量/5小时/一周/到期）；多个 xf 服务商配置多个 key → 各显示各的套餐。
- **Phase 30 ✅。**

### Phase 31 — 讯飞星辰使用情况 UI 重设计（三档进度条，字段语义=次数） 【complete】
- 用户需求：明确 `codingPlanUsageDTO` 字段语义——**都是次数**：`packageLimit`=一月总次数、`packageUsage`=一月已用、`packageLeft`=一月剩余；`rpwLimit/Usage`=一周总/已用；`rp5hLimit/Usage`=5小时总/已用；**重新设计讯飞星辰使用情况的 UI 展示**。
- **设计（纯前端，后端字段已齐）**：`fmtXfPlans` 重写为**三档配额进度条**——
  - 每套餐头行：**套餐名 + 绿色「剩 X」剩余次数 + 灰色到期**（`items-baseline` 对齐）；
  - 三行 `quotaBar`（标签 `包月`/`5小时`/`一周` + 圆角进度条 + `已用/总` 精确数字）：
    - 进度条颜色按用量 `quotaCls`：**≥90% 红 / ≥70% 黄 / 其余蓝**——快到限一眼可见；
    - 次数精确显示 `fmtC`（不做 K/M 缩写，4653.4 → "4653.4"）；
  - xf 行改 `items-start` 顶对齐（多行进度条），火山单行保持垂直居中。
  - 新增 Tailwind 类（bg-red/amber/emerald-500、h-1.5、rounded-full、tabular-nums、items-start/baseline、w-9）→ build_ui 编译进 CSS。
- **验收**：node --check ✓ / build_ui（**16289B**，+1078B 新类）✓ / cargo test **46 全绿** ✓ / e2e **47 断言全过**（JSON 字段不变，仅渲染层）✓ / release 3.25MB。
- **Phase 31 ✅。**

### Phase 32 — 讯飞用量改单行 pill 展示（放弃进度条，与火山同排） 【complete】
- 用户反馈：讯飞使用情况**不要进度条**，像火山方舟一样在一排显示，格式：`22/1200次（5小时）|1940.4/9000次（1周）|4653.4/18000次（月）|2026-08-14（到期）`，**美化漂亮点**。
- **实现（纯前端）**：`fmtXfPlans` 改为**单行 pill 组**——每档配额一个圆角小胶囊 `xfPill`：`<b>已用</b>/总量次（窗口）`，底色区分（5小时=amber、1周=emerald、月=sky，`bg-*-50 text-*-700`）；**已用/总量 ≥90% 该 pill 转红底**（`bg-red-50 text-red-600`）保留快到限警示；pill 间用浅灰 `|` 分隔，可 `flex-wrap` 窄屏换行；套餐名做靛蓝 badge 打头（`bg-indigo-50 text-indigo-700`）；到期灰字尾缀。移除 Phase 31 的 `quotaBar`/`quotaCls`；xf 行回 `items-center`（单行）。
- **验收**：node --check ✓ / build_ui（**15714B**，比进度条版还小 -575B）✓ / cargo test **46 全绿** ✓ / e2e **47 断言全过** ✓ / release 3.25MB。
- **Phase 32 ✅。**

### Phase 33 — 使用情况卡调整（xf 去套餐名 + 卡片前移） 【complete】
- 用户需求：① 讯飞使用统计**去掉套餐名**（专业版-包月）；② **使用情况卡排在服务设置前面**；③ 使用统计处只有已添加的服务商才显示（本就如此——`/api/status` usage 按 `cfg.providers` 过滤，`renderUsage` 只渲染返回的数组）。
- **实现（纯前端）**：`fmtXfPlans` 去掉套餐名 badge（只留 pill 组）；把「使用情况」卡片 HTML 块从页面底部移到**服务设置之前**（顶部操作栏下方）；xf 行回 `items-center`。
- **验收**：node ✓ / build_ui ✓ / cargo test 46 ✓ / e2e 47 ✓ / 部署 8787（页面顺序 = 使用情况 → 服务设置 → 服务商 → 已注册模型）。
- **Phase 33 ✅。**

### Phase 34 — 服务商预设模板下拉（ID 简写一键预设） 【complete】
- 用户需求：添加服务商时 **ID 前缀通过下拉选择**（DeepSeek / 讯飞星辰 / 火山方舟 / Kimi / GLM / Anthropic），**选择后自动把服务商名称简写设为 ID**。
- **实现（纯前端）**：弹窗顶部加 `<select id="m-preset">`（自定义 + 6 预设）；`PRESETS` 常量（id 简写 / 名称 / base_url / auth）；change 事件 → 自动填 ID/名称/Base URL/认证 + 清校验高亮；`openModal` 每次重置为「（自定义）」（编辑时不覆盖已存字段）。预设：ds/xf/hs/kb/glm/anthropic。
- **验收**：node ✓ / build_ui ✓ / cargo test 46 ✓ / e2e 47 ✓ / 部署 8787。
- **Phase 34 ✅。**

### Phase 35 — 用量清零按钮（POST /api/usage/reset） 【complete】
- 用户需求（勾选「用量清零/趋势」）：使用情况卡加**清零用量**按钮。
- **实现**：`webui.rs` 新增 `POST /api/usage/reset`——清空 `UsageStats` + 置脏 + **立即落盘** `*-usage.json`（重启也不回来）；`api.rs` 挂路由；UI 使用情况卡头部加「清零用量」按钮（红色确认弹窗 `confirmAction`）；e2e 单测 `usage_reset_clears_and_saves`。
- **验收**：cargo test **47 全绿**（+1）/ e2e 47 ✓ / node ✓ / build_ui ✓ / release 3.25MB / 部署 8787（`/api/usage/reset` → `{"cleared":true,"ok":true}`）。⚠️ 真机验证时把部署环境的当前用量清了（会重新累计）。
- **Phase 35 ✅。**

### Phase 36 — OpenAI 协议翻译（解锁仅 OpenAI 协议服务商） 【complete】
- 用户需求（勾选）：支持仅提供 OpenAI Chat Completions 协议的服务商（OpenRouter / Qwen 百炼等）。
- **设计**：`ProviderConfig.protocol`（`anthropic` 默认 / `openai`，serde 默认省写）；新模块 `src/translate.rs`——
  - `anthropic_to_openai`：system（字符串/数组）→ 首位 system 消息；`tool_use`/`tool_result` → OpenAI `tool_calls`/`tool` 消息；image 块 → `image_url` data URL；tools → function tools；tool_choice 映射（auto/required/指定）；stop_sequences→stop；stream 时加 `stream_options.include_usage` 让上游返回真实用量。
  - `openai_message_to_anthropic`（非流式）：choices[0].message → content 块（text + tool_use）、stop_reason 映射（tool_calls→tool_use / length→max_tokens）、usage。
  - `OpenAiStreamTranslator`（流式）：消费 OpenAI SSE chunk（`data: {chunk}`…`[DONE]`）→ 产出 Anthropic 事件序列（message_start / content_block_start / content_block_delta text_delta + input_json_delta / content_block_stop / message_delta / message_stop）；文本增量开 text 块、`delta.tool_calls` 按 OpenAI index 缓冲为 tool_use 块（arguments 分片发 input_json_delta）；收尾时把 usage chunk 的 prompt/completion_tokens 提取到共享 UsageStats（无 usage chunk 用字符数/4 启发式）。
  - `proxy.rs` 接线：`is_openai` → 上游 URL=`{base}/chat/completions`、剥离 `anthropic-version`/`anthropic-beta` 头、请求体走 `anthropic_to_openai`、响应流走 `openai_sse_stream`（复用 UsageSink/accumulate_usage）、非流式走 `openai_message_to_anthropic`；count_tokens 直接启发式回退。
  - UI：弹窗加「上游协议」select（Anthropic 纯透传 / OpenAI 自动翻译），collect/openModal 纳入；预设选择保持 anthropic。
- **验收**：cargo test **52 全绿**（+5：请求翻译 / 非流式转换 / 流式文本+工具+用量 / 流式纯文本启发式 / 空流）/ e2e **58 断言全过**（+11：oa 服务商 mock `/chat/completions` → 翻译/认证/剥头/SSE 事件/用量提取/count_tokens 启发式）/ node ✓ / build_ui ✓ / release **3.26MB**（+11KB translate）/ 部署 8787。
- **用户操作**：添加服务商 → 上游协议选「OpenAI」→ Base URL 填 OpenAI 兼容端点（如 `https://openrouter.ai/api/v1`）→ 配 Key/模型 → Claude Code 里 `/model oa-...` 正常对话（含工具调用）。
- **Phase 36 ✅。**

### Phase 38 — 术语调整：服务商 → 模型订阅 + 服务商必填 + Cookie 按服务商显示 + 名称展示 【in_progress】
- 用户需求：① 全站术语「服务商」→「模型订阅」（添加/编辑/保存/列表/删除/清零/使用情况）；② 弹窗「预设模板」→「服务商」且**必填**，选服务商自动填 ID 简写（可改）；③ **控制台 Cookie 输入框仅服务商选讯飞/火山时显示**；④ 使用情况面板由 ID 简写改为**服务商名称**；⑤ 服务商列表简写 → 服务商名称。
- **实现（纯前端 index.html，无后端改动）**：
  - 术语替换：区块标题「模型订阅」、添加/编辑/保存按钮与弹窗标题、空态文案、删除确认、各处 toast、注释。
  - `#m-preset` 改为「服务商（必填）」：占位「选择服务商…」+ 6 预设 + 自定义；openModal 新增=置空（必选）、编辑=按 id/base_url 推断（`presetOfProvider`）；modalSave 校验空值 → fieldError。
  - `toggleCookieField()`：`#m-cookie-wrap` 仅 `m-preset∈{xf,hs}` 显示，openModal 与 preset change 时同步。
  - `provName(id)`：id → 服务商显示名，用于 renderUsage / loadBalance / loadCodingPlan / renderProviders / dmItem / renderModels。
- [x] index.html 全部改动 + node --check
- [x] build_ui.py（15757B）+ cargo test 52 + e2e 58 + release 3.29MB
- [x] 部署 8787（PID 11944）：healthz ok、/api/status running providers 3、新标记在位（模型订阅/保存模型订阅/选择服务商/自定义/m-cookie-wrap/provName/presetOfProvider/toggleCookieField）
- **Phase 38 ✅。**

### Phase 39 — 页面字体紧凑/重叠优化 【complete】
- 用户反馈：部分地方**字与字过于紧凑，甚至重叠**，不够美观。
- **根因**：① 服务商名称徽标无 `shrink-0`/`whitespace-nowrap`——长名被 flex 挤压/换行贴邻；② 讯飞 pill 组 `gap-0` 换行时行距为 0 → 上下两行 pill 贴住；③ 大量 `text-[10px]`/`text-[11px]` + `py-px` 小字号字形拥挤/裁切。
- **修复（纯前端）**：
  - `<style id="ui">` 加全局 `body{letter-spacing:.012em;-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}`（CJK 字距）+ `.leading-loose-inline{line-height:1.7}`（手写类，build_ui 不覆盖）。
  - 徽标统一 `shrink-0 whitespace-nowrap` + `min-w-[64px]`：模型订阅列表 `text-[15px]`→`text-[13px] px-3`；使用情况/余额/Coding Plan `text-[13px]`→`text-xs px-2.5 py-1`；默认模型下拉徽章 `text-[10px]`→`text-[11px]`。
  - 上下文徽标 `text-[10px] py-px`→`text-[11px] py-0.5 px-2`（renderModels/dmItem/updateDmDisplay）。
  - 讯飞 pill：`text-[11px] px-1.5 py-0.5`→`text-xs leading-none px-2 py-1`；包装 `gap-0`→`gap-y-1`（换行有行距）；到期灰字同调。
  - 长段提示/说明（服务设置 hint、Key hint、模型明细、Coding Plan 头）`text-[11px]`→`text-xs leading-loose-inline`。
- [x] node --check ✓ / build_ui（**15580B**，新类 min-w-[64px]/gap-y-1 编译进 CSS）✓ / cargo test 52 ✓ / e2e 58 ✓ / release 3.29MB
- [x] 部署 8787（PID 21608，healthz ok）；线上标记：letter-spacing/leading-loose-inline×7/min-w-[64px]×4/gap-y-1×2 在位、`text-[10px]` 绝迹 ✓
- **Phase 39 ✅。**

### Phase 40 — 页面加宽，使用情况不换行 【complete】
- 用户反馈：使用情况区出现换行，把宽度调宽一点。
- **根因**：整页容器 `max-w-3xl`（768px）偏窄；讯飞用量 pill 组（`22/1200次（5小时）| 1940.4/9000次（1周）| 4653.4/18000次（月）| 到期`）在 `text-xs` 下约 580px，减去徽标/按钮后 info 栏只有 ~620px，宽度紧张。
- **修复（纯前端）**：`<main>` `max-w-3xl`→`max-w-4xl`（768→896px）；讯飞 pill `px-2`→`px-1.5`、分隔符 `mx-1`→`mx-0.5`（整组约省 40px）。896px 下 info 栏约 644px ≥ pill 组 ~540px → 单行放下。
- [x] node --check ✓ / build_ui（**15594B**，max-w-4xl/mx-0.5/px-1.5 编译进 CSS）✓ / cargo test 52 ✓ / e2e 58 ✓ / release 3.29MB
- [x] 部署 8787（PID 10232，healthz ok）；线上 `max-w-4xl` 在位、`max-w-3xl` 绝迹、pill 压缩在位 ✓
- **Phase 40 ✅。**

### Phase 41 — 刷新同步 tokens + 讯飞输入 tokens 启发式兜底 【complete】
- 用户反馈：① 「刷新余额/用量」不刷新 tokens 的请求/输入/输出；② 讯飞星辰没有输入 tokens 数据（是不是本身不支持）。
- **① 前端**：`refresh-balance` handler 加 `refreshUsage()`——重拉 `/api/status` 的 usage → `renderUsage`（tokens 请求/输入/输出随之刷新）。
- **② 根因（真机确认）**：`xf requests=6 但 in=0`，而 ds/hs 正常 → **讯飞星辰 Anthropic 兼容端点不在 `message_start` 返回 `usage.input_tokens`**（返回空）。中继按 Anthropic 标准只从 message_start 提取输入 → 输入恒 0。且 `rewrite_line` 里 `accumulate` 只在行含 `"usage"` 子串时触发——讯飞 message_start 无 usage 行走快路径直接 return，**连累计都不调用**（双重漏记）。
- **修复（后端）**：
  - `UsageSink` 加 `input_fallback: u64`（= 请求体字符/4，复用 `heuristic_input()`，与 count_tokens 兜底同规则）；forward 创建 sink 时算好。
  - `rewrite_line`：`message_start` 行强制进慢路径累计（`is_message_start` 判定），`accumulate()` 的 message_start 分支无 input_tokens 时用 `input_fallback` 兜底。
  - 非流式 Anthropic 响应：`message.usage.input_tokens` 缺失/0 时同样兜底。
  - translate.rs `OpenAiStreamTranslator`：`finalize` 时 `in_tokens` 为 0 则用 `input_fallback`（OpenAI 不返回 usage chunk 时也兜底）。
- **语义说明**：兜底是**估算**（字符/4），非上游真实计费；只有上游不返回输入用量时才启用，上游恢复返回则自动用真实值（单测 `usage_uses_real_input_when_present` 保证不覆盖真实值）。
- [x] node --check ✓ / build_ui（15594B）✓ / cargo test **54 全绿**（+2：`usage_fallback_when_upstream_missing_input`、`usage_uses_real_input_when_present`）/ e2e 58 ✓ / release 3.29MB
- [x] 部署 8787（PID 17888，healthz ok）；线上 `refreshUsage` handler 在位；xf 历史 in=0 待新对话后由兜底开始累计。
- ⚠️ 中途 build_ui.py 失败：`webui/tailwind/node_modules` 被清理（Can't resolve 'tailwindcss'）→ `npm install` 恢复（+33 包）。
- **Phase 41 ✅。**

### Phase 42 — 托盘右键菜单加「更新 claude 配置 / 更新环境变量」 【in_progress】
- 用户需求：把 Web UI 顶部两个按钮**更新 claude 配置 / 更新环境变量**也加到**托盘图标右键菜单**（服务常驻隐藏时也能一键写 `~/.claude/settings.json`，免开配置页）。
- **现状**：两个 Web UI 按钮语义相同（`POST /api/update-settings` 写顶层 `model` + env 6 键到 claude settings，仅确认弹窗文案/颜色不同）；后端 handler 是 async（`cfg.read().await`），托盘是独立 std 线程不能 await。
- **设计**：
  - `webui.rs` 抽出同步核心 `write_claude_settings(host, port, model, path)`（纯文件读写，可单测）；async handler 读 cfg 后调它；新增 `pub(crate) update_settings_blocking(&Arc<AppState>)` —— `cfg.blocking_read()`（tokio RwLock 阻塞读，std 线程安全，同 Phase 25 `blocking_write` 模式）取 `sentinel_model()` → 写 settings → 返回 `(base_url, model, path)`。
  - `tray.rs` 菜单加两项「更新 claude 配置 / 更新环境变量」（在「设置」之后）：点击 → `update_settings_blocking` → **Win32 信息气泡**反馈（`Shell_NotifyIconW` `NIF_INFO|NIF_GUID` 稳定 GUID，Windows 7+ 无需回调窗口，气泡自动消失）——服务常驻隐藏（--autostart）时 stderr 不可见，气泡是唯一可见反馈；失败气泡红色告警。
  - `Cargo.toml` windows-sys 加 `Win32_UI_Shell` + `Win32_System_LibraryLoader` feature。
- [x] webui.rs 重构 + 单测
- [x] tray.rs 菜单项 + 气泡
- [x] 构建/测试/部署 + 文档
- **验收**：cargo test **55 全绿**（+1 `write_claude_settings_merges_env_and_top_model`）/ e2e **58 断言全过** / 纯后端无 UI 改动（不需 build_ui）/ release **3.29MB**（3,453,952B，+5KB）/ **气泡真机验证**：Shell_NotifyIconW 返回成功（气泡正常显示）✓ / 部署 8787（PID 2344，--config 显式 + --autostart，healthz ok、providers 3、usage 持久化保留）✓。
- **坑（findings §2.37）**：托盘气泡先用 `NIF_GUID`+hWnd=null（Win7+ GUID 模式）→ Win10 实测 `Shell_NotifyIconW` 返回 **E_FAIL**（0x80004005，GUID 未先 NIM_ADD+NIM_SETVERSION 注册）；改**经典 hWnd+uID 模式**（`RegisterClassW` + `CreateWindowExW(HWND_MESSAGE)` + `NIF_INFO` 仅气泡、临时图标自动移除）→ 成功。
- **Phase 42 ✅。**

### Phase 43 — 托盘菜单「更新」子菜单收纳两个更新项 【in_progress】
- 用户需求：托盘菜单里**更新 claude 配置 / 更新环境变量**两项**合并到一个「更新」菜单**，鼠标移入展开两个子项（与「模型」子菜单同交互）。
- **实现（纯 tray.rs）**：新建 `update_root = Submenu::new("更新", true)`，把两个 `MenuItem`（`update_claude`/`update_env`，对象不变）`append` 进去；菜单组装时 `update_root` 替换原来的两个平铺 append；事件处理不变（`update_claude.id()`/`update_env.id()` 仍命中——MenuEvent 不关心菜单层级）。Submenu 鼠标移入展开是 Windows 原生行为，与「模型」一致。
- [x] tray.rs 子菜单化
- [x] 构建/测试/部署 + 文档
- **验收**：cargo test **55 全绿** / e2e **58 断言全过** / debug 运行时 ready 日志 `右键=设置/更新(子菜单)/模型/停止|启动/退出`（菜单组装无错，更新子菜单构建成功）✓ / release **3.29MB**（3,453,952B）/ 部署 8787（PID 7164，healthz ok、providers 3、usage 保留）✓。
- **Phase 43 ✅。**

### Phase 44 — Claude Code 自定义 `/models` 命令查看模型列表 【in_progress】
- 用户需求：在 Claude Code 里输入 `/models` 查看当前可用的模型列表。
- **方案（无需改 Rust/重启 relay）**：Claude Code **自定义斜杠命令**——在 `~/.claude/commands/models.md`（用户全局作用域，所有项目可用）建一个命令文件：frontmatter（`description` + `allowed-tools: "Bash"`，格式对齐本机 planning-with-files 插件的 `commands/*.md` 真实样例）+ 正文提示词。用户输 `/models` 时正文作为提示注入，模型用 Bash `curl -s http://127.0.0.1:8787/api/config` 取配置 → 按服务商分组渲染「完整别名 → 真实模型 → 上下文」列表 + 当前使用模型（`server.default_model`）。
- **数据源**：`GET /api/config` 返回 `cfg.masked()`——含 `server.default_model` + `providers[].models[{alias,real,context}]`（掩码 Key，命令正文明确禁止展示/复制掩码值）。端口失败按序回退：`echo $ANTHROPIC_BASE_URL`（start-claude.cmd 设置的会话 env）→ settings.json 的 `env.ANTHROPIC_BASE_URL`。
- [x] 创建 `~/.claude/commands/models.md`（frontmatter + 提示词正文）
- [x] 验证：文件 UTF-8 正确、frontmatter 可解析、数据源 curl OK（default_model=ds-v4-flash / providers=3）
- **验收**：重启 Claude Code 会话后输 `/models` → 展示按服务商分组的模型列表（含当前使用模型 + context 简写）。⚠️ 自定义命令在 CC **会话启动时加载**——当前已开着的会话需重启（或新开会话）才能看到 `/models`。
- **Phase 44 ✅。**

### Phase 45 — 双击启动自动安装 `/models` 自定义命令 【in_progress】
- 用户需求：双击运行 claude-relay.exe 时判断 `~/.claude/commands/models.md` 是否存在，没有就自动创建——把 exe 分发给用户后跑一次即自动生成 `/models` 命令。
- **设计**：
  - `assets/commands/models.md`：命令文件本体入库（= 当前 `~/.claude/commands/models.md` 原样），`include_str!` 内嵌进 exe（同 webui.rs 的 index.html 模式，+~2KB）。
  - `src/commands.rs`（新模块）：`ensure_models_command()`——解析命令目录（**尊重 `CLAUDE_CONFIG_DIR`**，`{dir}/commands`；否则 `{USERPROFILE|HOME}/.claude/commands`）→ `models.md` 已存在则不动（用户自定义内容不覆盖）→ 缺失则 `create_dir_all` + 写入内嵌内容。幂等，失败仅告警不阻断。
  - `main.rs`：`mod commands;` + main() 在 `--print-default-model` 分支后、validate 前调用（所有正常启动路径——双击/autostart/restart——都会确保命令；`--print-default-model` 保持纯查询）。
  - 单测：临时 `CLAUDE_CONFIG_DIR` 下缺失→创建+内容正确；已存在→不覆盖。
- [x] assets/commands/models.md + commands.rs
- [x] main.rs 接线 + 单测
- [x] 构建/测试/部署 + 文档
- **验收**：cargo test **56 全绿**（+1 `ensure_models_command_creates_and_preserves`）/ e2e **58 断言全过** / **自动安装真机验证**（临时 CLAUDE_CONFIG_DIR 跑 debug → `commands/models.md` 创建且内容=内嵌命令）✓ / release **3.29MB**（3,458,048B，+4KB 命令文件）/ 部署 8787（PID 14696，healthz ok、providers 3、已存在命令未被覆盖）✓。
- **Phase 45 ✅。**

### Phase 37 — 体积/性能优化核对（无需改动） 【complete】
- 用户需求（勾选「体积/性能优化」）：把 release 体积/内存再压一压。
- **核对结论**：**无需优化**——所有硬性目标已大幅达标：
  - exe **3.29MB**（3,448,832B，目标 ≤6MB，**55% 余量**）；
  - 空闲内存 **WorkingSet 8.7MB / Private 1.8MB**（真机 8787 进程实测，目标「数 MB」）；
  - 空闲 CPU **0.03s**（≈0）；
  - release profile 已是极值：`opt-level="z"` + `lto` + `codegen-units=1` + `panic="abort"` + `strip`（Cargo.toml）。
  - 依赖已裁剪：axum 仅 http1、reqwest rustls（非 OpenSSL）、单线程 tokio current_thread。唯一「胖」依赖 `rusqlite bundled`（+~1MB）是**浏览器 Cookie 读取的承重依赖**（Phase 28），不可省。
  - **UPX 明确跳过**：findings §3 警告杀软误报风险（本项目单 exe 发布，误报代价高）；3.29MB 远超目标，压到 ~2.5MB 收益边际且引入风险。
- **Phase 37 ✅。**

## Next Step
**Phase 27–45 全部完成。** 已构建（单测 **56** / e2e **58** 全绿，release 3.29MB）并部署 8787（PID 14696）。用户验证清单：① **双击运行 exe 即自动生成 `/models` 命令**（`~/.claude/commands/models.md`，已存在则保留用户自定义）；② 重启 CC 会话后输 `/models` 查看模型列表；③ 托盘右键「更新」子菜单（更新 claude 配置 / 更新环境变量，弹气泡）；④ 点「刷新余额/用量」→ tokens 同步刷新；⑤ 讯飞输入 tokens 从 0 开始累计。遗留：项目从未 git 提交（建议初始化提交）；`test.json` 是 Phase 30 遗留样例文件可删；CC ~1MB 大请求偶发 400 谜题诊断日志已埋待复现。

### Phase 14 — 修复默认模型下拉不可用（.hidden class 误用） 【complete】
- 用户报告：**默认模型不能选择**（点开无反应）。
- **根因**：`#dm-drop` 把 `hidden` 写进了 **class 列表**（Tailwind `.hidden{display:none}` 永远生效），而 JS `openDm()` 只设置 `hidden` **属性**（`$('dm-drop').hidden=false` 移除属性）→ class 仍强制 display:none → 下拉永远不显示。三个 overlay（modal/confirm）都正确用属性形式，仅 `dm-drop` 踩坑。
- **修复**：`class="hidden absolute ..."` → `class="absolute ..."` + `hidden` 属性（`[hidden]{display:none!important}` 规则控制显隐，与 overlay 同机制）。
- [x] 修复 markup（class hidden → 属性 hidden）
- [x] 验收：build_ui.py（14320B）→ node --check ✓ → release 2.31MB 部署 8787（新 PID 13836）→ 线上 dm-drop = `class="absolute ..." hidden`、无 `.hidden` class ✓

## Next Step
**全部 15 个 Phase 完成后刷新**：默认模型下拉为无搜索的富选项列表。
可选后续：给 ds 配别名（如 `flash=deepseek-v4-flash`）体验 `/model ds-flash`；接入 Kimi/GLM/Anthropic 官方等服务商；继续追查 CC ~1MB 大请求偶发挂起谜题（诊断日志已埋）。

### Phase 5 — Web UI 改版：弹窗添加服务商 + 列表展示 【complete】
- [x] `webui/index.html`：服务商由内联卡片改为**紧凑列表**（每行：ID 徽标 + 显示名 + 模型/base_url 摘要 + 编辑/删除按钮）
- [x] 「＋ 添加服务商」改为打开**模态弹窗**（全部字段：ID/显示名/Base URL/Key/认证方式/max_tokens/开关/模型标签），编辑复用同一弹窗（`editIdx` 区分新增/编辑）
- [x] 弹窗校验与后端对齐：ID 非空且唯一、Base URL 非空、新服务商 Key 必填且不能是掩码、掩码 Key（含 `*`）编辑已有服务商时放行（后端按 id 复用）
- [x] `collect()` 改为直接序列化 `providers` 数组（弹窗即数据源）；save/copy-env/热重载/掩码往返行为不变
- [x] 验收：`cargo check` ✓、抽取 JS `node --check` 语法 ✓、临时 8788 实例 curl `/` 确认新页面内嵌生效（modal-title/m-id/m-ok/plist/添加服务商 标记齐全）✓、回归 e2e.py **25 断言全绿** ✓
- [x] 已重新构建 release（2.14MB）并替换项目根 + target/release 的 exe，**8787 运行中的 relay 已无缝切到新 UI**
- ⚠️ 事故：`taskkill //IM claude-relay.exe` 按映像名**杀掉了全部实例**（含用户正在用的 8787）→ 立刻用新构建重启恢复（中继无状态，CC 无需重启）；教训：开发期重启 relay 用 PID 定向 taskkill，勿用 //IM

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| curl `-d` 内联中文在 Git Bash 被打乱（invalid unicode code point） | 1 | 改 `--data-binary @file`，JSON 用 Write 工具落盘（UTF-8 无损） |
| include_str! 报"找不到 ../webui/index.html"（IDE 诊断） | 1 | 属陈旧诊断（文件实为存在）；`cargo check` 为准（同 Phase 1 教训） |

## Decisions Made
| 日期 | 决策 |
|------|------|
| 2026-08-03 | v1 方案：Rust/单端口/`前缀:模型`/JSON配置+Web UI/双向协议翻译 |
| 2026-08-03 | v2 调整：分隔符改 `-`；**全服务商走 Anthropic 协议→纯透传零翻译**，删除 translate/openai/anthropic 模块；新增 D12 最长前缀解析 |
