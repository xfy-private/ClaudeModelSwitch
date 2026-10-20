# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

> 二次开发请先读 `DEVELOPMENT.md`（技术栈/模块/扩展点/坑）。

## 项目概述

**ClaudeRelay** — Rust 编写的本地中转服务，部署在 Codex 与多个 LLM 服务商之间。CC 通过 `ANTHROPIC_BASE_URL=http://127.0.0.1:8787` 指向它，使用 `/model {前缀}-{模型名}` 动态切换服务商和模型，零重启。

核心思路：**全 Anthropic 协议纯透传** — 改 model → 换 Key → 原样转发 → SSE 流式透传回 CC，不翻译协议。

### 目标约束

- exe 体积 ≤ 6MB（当前 release 约 2.1MB）
- 空闲内存 ~数 MB（单线程 tokio current_thread）
- CPU 空闲 ≈ 0
- 单端口（默认 8787，仅绑 127.0.0.1）

## 构建与运行

```bash
# 开发构建（debug 模式）
cargo build

# 发布构建（优化体积）
python webui/build_ui.py  # 1) 编译 Tailwind CSS 并注入 webui/index.html（仅改过 UI 后需要）
cargo build --release     # 2) 重新打包
# 产物位于 target/release/Codex-relay.exe (~2.16MB)

# 运行（使用项目根 config.json）
cargo run -- [--debug] [--host 127.0.0.1] [--port 8787] [--config config.json]

# 运行测试
cargo test

# 检查编译（不生成二进制）
cargo check

# 打印第一个服务商的第一个模型别名（供 start-Codex.cmd 使用）
cargo run -- --print-default-model
```

**注意**：本项目目标工具链为 `x86_64-pc-windows-gnu`（非 MSVC）。TDM-GCC 10.3 已补齐 `libgcc_eh.a` 空归档，无需 `.cargo/config.toml` 额外配置。

## 架构

```
Codex ──(Anthropic Messages)──▶ ClaudeRelay (127.0.0.1:8787)
                                        │
                                        ├─ /v1/messages          ← proxy.rs (POST, SSE 透传)
                                        ├─ /v1/messages/count_tokens ← proxy.rs (转发 + 启发式回退)
                                        ├─ /v1/models            ← api.rs (聚合别名)
                                        ├─ /healthz              ← api.rs
                                        ├─ /                     ← webui.rs (配置页 HTML)
                                        └─ /api/config, /api/status, /api/restart, /api/update-settings ← webui.rs
                                              │
                              ┌───────────────┼───────────────┐
                              ▼               ▼               ▼
                        DeepSeek          Kimi          Anthropic
                        (各自 Anthropic 兼容端点)
```

### 模块结构

| 文件 | 职责 |
|------|------|
| `src/main.rs` | CLI 解析（**`Args` 结构体**：`--config`/`--host`/`--port`/`--debug`/`--print-default-model`/`--autostart`/`--restart`）、**`hide_console_if_double_clicked()`**（D19：GetConsoleProcessList n==1 → SW_HIDE 隐藏黑框）、`resolve_config_path()`（CWD 缺失回退 exe 同目录，开机自启 CWD=System32 可用）、`port_is_busy()`（500ms 探测已运行；**`--restart` 时跳过**——防自重启误判忙端口杀服务，Phase 20）、tokio current_thread runtime 启动、启动 banner、`bind_with_retry()`（20×150ms 绑定重试，返回 Result；首次失败 exit(1)，托盘重启路径 1s 重试）；`--print-default-model` 用 `Config::sentinel_model()`（D22 哨兵值）；创建 `AppState`；**`run_server` 服务循环**（D18：watch 驱动托盘 停止/启动，`tokio::select!` 监听停止信号、drop serve 释放端口；`open_browser` 参数=双击首次就绪后开配置页）；`tray::spawn` |
| `src/main.rs`（stderr 落盘） | **`redirect_stderr_to_log()`（Phase 62）**：main 首行把进程 stderr 重定向到 exe 同目录 `relay.log`——`windows-sys SetStdHandle(STD_ERROR_HANDLE)` 换柄（Rust std stderr 惰性取柄，须早于任何 `eprintln!`/panic；File `mem::forget` 防句柄被 drop 关闭；>5MB 启动截断防膨胀；打开失败保持原 stderr）。背景：`--autostart` 隐藏控制台后 stderr 不可见，断流 `[warn]`（D61）、托盘/卡片/cookie 续期告警全部丢失，排障无据。生产排障：`relay.log` 尾部即最近诊断 |
| `src/autostart.rs` | **开机自启注册**（D19）：windows-sys `Win32_System_Registry` 写 `HKCU\...\CurrentVersion\Run`（值 `ClaudeRelay` = `"<exe>" --autostart`）；`apply(true)` 注册、`apply(false)` 删除（幂等）；非 Windows no-op。注册表函数被 `Win32_Security` feature 门控 |
| `src/commands.rs` | **Codex 自定义命令自动安装**（Phase 45）：`ensure_models_command()` 确保 `~/.Codex/commands/models.md`（`/models` 命令）存在——**已存在则不动**（不覆盖用户自定义）、缺失则写入 `include_str!("../assets/commands/models.md")` 内嵌内容；命令目录**尊重 `CLAUDE_CONFIG_DIR`**（`{dir}/commands`，否则 `{USERPROFILE|HOME}/.Codex/commands`，与 `webui::claude_settings_path` 同规则）；main 在 `--print-default-model` 后调用，失败仅告警。**分发 exe 跑一次即自动生成 `/models`** |
| `src/config.rs` | `Config`/`ProviderConfig`/`ServerConfig` 结构体、JSON 读写、校验（端口范围/服务商 id 唯一性/前缀冲突告警；**服务商允许为空**，D21——首次运行/删空后去配置页添加即可）、`masked()` 掩码 Key、**`ModelEntry{alias,real,context}` 模型别名**（自定义 serde：字符串或 `{alias,real,context?}` 双形，alias==real 且 context==默认写字符串；context 默认 200000，仅展示元数据）、`Config::default_model()`（优先 `server.default_model` 否则回退第一个别名）、`Config::sentinel_model()/current_ctx_suffix()/ctx_suffix_of()`（D22 哨兵值 `Codex-relay-model[上下文]`）、`Config.providers`（`skip_serializing_if="Vec::is_empty"`，空配置首跑生成的 config.json 无 providers 键，D21）、`ServerConfig.auto_start`（`#[serde(default, skip_serializing_if="is_false")]`，默认 false 不落盘）、`AppState`（cfg/client/debug/config_path/started；无停止标志——停止状态在 main 的 watch 通道）、`AppState::set_default_model_blocking(full)`（D25：托盘设「当前使用模型」，校验可路由→写盘→`blocking_write` 热重载，阻塞版勿在异步上下文调用）、**`UsageStats/ProviderUsage/ModelUsage` + `AppState.usage/usage_dirty`**（D26/D27：调用量/用量统计——请求数+输入/输出 tokens 按服务商+模型，自启动以来累计；**serde 持久化**：`save()` 原子写 `.tmp`+rename、`load()` 缺失/损坏→空，main 5s 周期落盘重启不清零；`std::sync::Mutex` 短锁）、**`ProviderConfig.console_cookie/console_usage_url` + `Config::mask_cookie`**（D28/D30：火山方舟/讯飞星辰 Coding Plan 用量查询的 Cookie 与 URL，掩码展示保留尾 8 字符 + PUT 复用） |
| `src/router.rs` | `resolve()` 按**已配置服务商 id 做最长前缀匹配**（如 `ds-deepseek-chat` → provider=`ds`、real_model=`deepseek-chat`）；**后缀先查该服务商模型别名 → 命中则发 real，否则后缀直接当真实模型名**；`available_aliases()` 推导全部可用别名（`{id}-{alias}`） |
| `src/cookies.rs` | **浏览器 Cookie 库读取（D28/D30，域参数化）**：发现 Chrome/Edge 各 profile `Network/Cookies` + `Local State`；**浏览器运行中独占锁库**（os error 32）→ 复制主库+`-wal`+`-shm` 到临时目录读-写打开（SQLite 回放 WAL）；`os_crypt.encrypted_key` DPAPI 解密 → AES-128-GCM 解 `v10`（`v11` app-bound 尽力）→ SQLite 查 `host_key LIKE ?1` → 拼 Cookie 头。统一入口 `read_console_cookie(domain_like, display)`：火山方舟 `read_volcengine_console_cookie()`（`%volcengine.com`）/ 讯飞星辰 `read_xf_console_cookie()`（`%xfyun.cn`）。供 `/api/coding-plan` 兜底 + `/api/import-cookie`；锁库时报「手动粘贴或关浏览器后导入」 |
| `src/proxy.rs` | `/v1/messages` 和 `/v1/messages/count_tokens` 的转发核心：解析 body→前缀匹配→改写 model→替换认证头→白名单透传头→上游请求→SSE 流式透传（可选 model 名回写）。`ModelRewriter` 双轨：紧凑 JSON 字节快路径 + JSON 递归改写慢路径；**所有 SSE 统一走缓冲流并提取用量**（D26：`message_start` 输入 + `message_delta` 输出 tokens，含 `usage` 的行跳过字节快路径防漏计；纯透传无 usage 行零解析）。`heuristic_tokens()` 字符数/4 兜底。**Phase 36（D36）**：`is_openai`（`provider.protocol=="openai"`）→ 上游 URL=`{base}/chat/completions`、剥 `anthropic-version/beta` 头、请求体走 `translate::anthropic_to_openai`、流式走 `openai_sse_stream`（translate 翻译器）、非流式走 `openai_message_to_anthropic`、count_tokens 直接启发式；`UsageSink`/`accumulate_usage` 提为 `pub(crate)` 供 translate 复用。**Phase 59/61（D61）**：三个流包装器（`sse_watch_stream`/`openai_sse_stream`/`gemini_sse_stream`）重构为外壳+可注入内核（`*_inner`），上游中途断流两种形态统一兜底——Err（RST/网络错误）与 EOF（FIN 干净掐流，reqwest `Ok(None)`）都不再向响应体抛错/伪装收尾，改为发协议规范 `error` 事件（`sse_error_chunk`，api_error+上游原因）+干净收尾+`[warn]` 日志；EOF 判据=`ModelRewriter.saw_stop`（message_stop 字节级检测）或 `stream_terminated()`（translate/gemini）。（停止语义不在 proxy——服务停止时端口已释放，请求根本到不了这里） |
| `src/translate.rs` | **OpenAI 协议翻译（D36）**：`anthropic_to_openai`（请求体：system→system 消息、tool_use/tool_result→tool_calls/tool 消息、image→image_url、tools→function、tool_choice 映射、stop_sequences→stop、stream→stream_options.include_usage）；`openai_message_to_anthropic`（非流式响应：content/tool_calls→content 块、stop_reason 映射、usage）；`OpenAiStreamTranslator`（流式：OpenAI SSE chunk → Anthropic 事件序列——文本开 text 块、`delta.tool_calls` 按 index 缓冲为 tool_use 块发 input_json_delta、收尾 message_delta+message_stop 并把 usage chunk 的 prompt/completion_tokens 提取到 UsageStats，无 usage 用字符/4 启发式）；**Phase 61（D61）**：`stream_terminated()`（收到 finish_reason 或已按 [DONE] finalize——供包装器判别 EOF=正常结束还是被掐断）+ `alias()` 访问器 |
| `src/api.rs` | axum Router 构建、`/` `GET /v1/models` `/healthz` `/api/config`(GET) 端点；定义 `VERSION`。（停止时端口已释放，请求到不了任何路由，无需停止守卫） |
| `src/webui.rs` | `include_str!("../webui/index.html")` 内嵌配置页；`PUT /api/config`（掩码 Key/`console_cookie` 复用→校验→写盘→热重载→**`autostart::apply(auto_start)` 开关自启**，失败进 warnings）；`GET /api/status`（含 `model_details` 与 **`usage` 用量统计**，D26）；`GET /api/balance`（D26：DeepSeek `GET {主机根}/user/balance` 同 key 查询真实余额，其余返回「无简单余额接口」，8s 超时不缓存）；`GET /api/coding-plan`（D28/D30：Coding Plan 用量——`console_kind` 分派：火山方舟解析 `QuotaUsage[]` 为 5小时/一周/一月 百分比+重置时间，讯飞星辰查 `coding-plan/list` 后**按 `codingPlanAppCredentialDTO.apiKey` 匹配服务商 api_key 过滤 rows**（多个配置/多匹配行 → 多套餐）；cookie=存档或自动读浏览器对应域）；`POST /api/import-cookie`（D28/D30：按目标服务商类别自动读浏览器对应域 → 存 `console_cookie` → 写盘热重载 → 掩码返回）；`POST /api/restart`（spawn 同参数新进程 → 600ms 后 exit(0) 自重启）；`POST /api/update-settings`（把 BASE_URL/KEY/MODEL + **三个任务槽 env** 合并写 `~/.Codex/settings.json`，尊重 `CLAUDE_CONFIG_DIR`，其余保留；任务槽 D24；**Phase 42**：核心逻辑抽同步 `write_claude_settings`，新增 `update_settings_blocking` 供托盘线程 `cfg.blocking_read()` 调用） |
| `src/tray.rs` | Windows 系统托盘（tray-icon + muda）：**程序化生成 32×32 图标**（绿圆+白闪电，`Icon::from_rgba`）+ **Windows 消息泵**（GetMessageW/DispatchMessageW，`windows-sys`）；右键菜单 **设置 / 更新（子菜单：更新 Codex 配置·更新环境变量——Phase 42/43：`update_settings_blocking` 写 `~/.Codex/settings.json` + **Win32 信息气泡**反馈，`Shell_NotifyIconW` `NIF_INFO` 经典 hWnd+uID——隐藏消息窗口 `HWND_MESSAGE`+`DefWindowProcW`，findings §2.37；Phase 43 收纳进「更新」Submenu）/ 模型（子菜单：模型→服务商→模型，`CheckMenuItem` 勾选当前模型，选中 `AppState::set_default_model_blocking` 写盘+热重载，D25）/ 停止\|启动（两字）/ 退出**，双击打开配置页；托盘持有 `watch::Sender<bool>`：点「停止」`send(true)` → run_server 释放端口停服务，点「启动」`send(false)` → 恢复；**`pub open_browser`**（main 双击打开配置页复用）；非 Windows no-op |
| `src/widget.rs` | **桌面悬浮卡片**（Phase 55，GDI+ 重新实现）：标准顶置 popup 窗口（`WS_POPUP\|WS_EX_TOPMOST\|WS_EX_TOOLWINDOW`，不用分层窗口/ULW）显示当前模型的今日用量/余额/Coding Plan（请求/输入/输出三列一行垂直居中+列间细分隔线）。**绘制用 GDI+**（`GdipCreateFromHDC` 画 `WM_PAINT` 的窗口 DC，`Win32_Graphics_GdiPlus` feature，系统 gdiplus.dll 不增 exe 体积；圆角 `SetWindowRgn` 裁剪；`WM_ERASEBKGND=>1` 跳过擦除防闪烁，依赖整面不透明渐变全覆盖）。卡片窗口建在**托盘线程**（消息泵调度）；数据任务（main tokio）每 5s 读 `state.usage` 内存快照、每 30s HTTP 自调 `/api/balance`+`/api/coding-plan?renew=0`，**仅数据变化才 PostMessageW 重绘**（空闲零重绘）。**ShowWindow 显示路径（创建/托盘切换）后强制 `InvalidateRect(erase=1)+UpdateWindow` 同步重绘**（隐藏窗口不收 WM_PAINT + 跳过擦除 -> 未初始化表面位会当背景）。交互：`WM_NCHITTEST`->HTCAPTION 系统原生拖拽/关闭区与柱状图区 HTCLIENT；`WM_MOUSEACTIVATE`->MA_NOACTIVATE 不抢焦点；双击打开配置页；卡片内三按钮（主题/切换模型/关闭）+ 柱状图悬浮明细。位置/可见性/主题存 `*-card.json`。托盘「桌面卡片」CheckMenuItem 控制显隐。非 Windows no-op |
| `webui/index.html` | 纯原生 JS 单文件配置页：模型订阅 CRUD（D38 术语：配置条目=「模型订阅」、上游厂商=「服务商」；**弹窗保存即持久化**，D16；可**全部删除**，D21）、**弹窗内三列模型行列表**（别名/真实名/上下文大小下拉默认 200K，行内增删）、**可搜索默认模型下拉**（每行显示服务商徽章+别名+真实名+上下文徽标，可清除）、Key 掩码、服务设置区 **开机自启 checkbox**（D19，保存即写注册表）、顶部操作栏 **5 按钮分色**（保存配置/保存并重启/更新 Codex 配置/更新环境变量/复制 CC 环境变量命令，均带 SVG 图标）+ 通用确认弹窗（**彩色图标圆** + 按钮配色；删除模型订阅走红色确认 + **确认后 PUT 持久化**）+ **弹窗「服务商」必填下拉**（D38：`#m-preset` 6 预设+自定义，选预设自动填 ID 简写/名称/Base URL/认证、ID 可改；编辑按 id/base_url `presetOfProvider` 推断）+ **控制台 Cookie 仅服务商讯飞/火山时显示**（D38：`toggleCookieField` 切 `#m-cookie-wrap`）+ **弹窗内字段校验**（`#m-error` 红字 + `fieldError()` 高亮对应输入框，D21）+ **「使用情况」卡片**（D26：每服务商 请求数/输入/输出 tokens + 按模型明细 + 余额与刷新按钮；**ID 简写一律 `provName` 显示服务商名称**，D38；**Coding Plan 块** D28/D30/D32：火山 5小时/一周/一月 百分比+重置，讯飞 `fmtXfPlans` **单行彩色 pill 组**——`xfPill` 圆角胶囊 `<b>已用</b>/总量次（窗口）`（5小时=amber/1周=emerald/月=sky，`bg-*-50 text-*-700`，**≥90% 该 pill 转红底**警示），`|` 分隔 + `flex-wrap`，到期灰字，行内 导入/粘贴 按钮）；样式 = **Tailwind 编译产物**（`<style id="tw">`，注入占位块）+ **手写按钮组件层 `<style id="ui">`**（`.btn`/渐变 solid/ghost/danger-ghost/`btn-icon`/`ic-*`，build_ui.py 不覆盖 ui 块）；Feather 风格 SVG 图标映射 `S` + `svg()` |
| `webui/build_ui.py` | UI 一键构建：重置占位块（保证 Tailwind 扫描干净源码）→ `npx @tailwindcss/cli` 编译 → 幂等注入编译产物 → 覆盖 index.html |
| `webui/tailwind/` | Tailwind v4 构建环境：`package.json`（tailwindcss + @tailwindcss/cli）、`in.css`（`@import "tailwindcss"` + `@source "../index.html"` + `[hidden]{display:none!important}` 修复）、`out.css`（编译产物，注入后同随 index.html 进 exe） |
| `build.rs` | 当前为空（历史 workaround 已移除） |

### 关键设计决策

| # | 决策 | 代码位置 |
|---|------|----------|
| D1 | `tokio` current_thread 单线程、`axum` 仅 http1、`reqwest` rustls（不用 OpenSSL） | `Cargo.toml` features, `main.rs:110` |
| D3 | 全 Anthropic 协议纯透传，不翻译协议 | `proxy.rs` |
| D7 | SSE 字节级透传；可选逐行 `model` 字段改写回别名 | `proxy.rs:241-277` `ModelRewriter` |
| D8 | count_tokens 优先转发服务商，失败回退启发式 `字符/4` | `proxy.rs:228-238` |
| D9 | ~~服务商可选 `max_tokens` 上限，转发前 clamp~~ → **Phase 9 已移除**（CC 的 max_tokens 原样透传，由各上游自行处理） | 已删除 |
| D10 | 剥离 CC 认证头，替换为服务商 Key；`anthropic-beta` 可配剥离 | `proxy.rs:135-154` |
| D12 | 模型名按已配置服务商 id **最长前缀匹配** | `router.rs:20-53` |
| D13 | Web UI Key 掩码往返：含 `*` 的 Key PUT 时按 id 复用真实值 | `webui.rs:44-65` |
| D14 | settings 隔离：`start-Codex.cmd` 用 `--settings Codex-settings.json` 加载 relay 指向，主 settings.json 保持日常直连不动 | `start-Codex.cmd` |
| D15 | Web UI 用 Tailwind CSS（编译内嵌静态 CSS，非 CDN）：`@source` 限定扫描 index.html，编译产物注入 `<style id="tw">` 随 include_str! 进 exe | `webui/build_ui.py`, `webui/tailwind/in.css` |
| D16 | Web UI 弹窗「保存服务商」即 `PUT /api/config` **持久化**（成功才关弹窗）；顶部「保存配置并重启」= PUT + `POST /api/restart`（spawn 同参数新进程 + **追加 `--restart`**（Phase 20：防新进程启动时 `port_is_busy` 误判旧进程占用而退出杀服务）、600ms 后 exit 自重启，main `bind_with_retry` 兜底） | `webui/index.html` JS, `webui.rs:restart`, `main.rs:bind_with_retry` |
| D20 | **顶部按钮文字提示固定在按钮下方独立行**：`#toast` 从按钮 flex 行内（`ml-auto` 挤压按钮）移出，放卡片内按钮行下方（`mt-2 min-h-[18px] flex items-center`，min-h 保底防跳动） | `webui/index.html` |
| D21 | **空配置可启动 + 服务商可删空 + 弹窗内字段校验**：`validate()` 不再把「无服务商」当致命错误（首次运行/删空后去配置页添加即可，`/v1/messages` 由 router 返回 400 不 panic）；`Config.providers` `skip_serializing_if="Vec::is_empty"` → 无配置首跑生成的 `config.json` 恰为 `{"server":{host,port,separator,default_model}}`；UI 去掉「至少一个服务商/不能删最后一个」拦截，可保存空服务商；服务商弹窗加 `#m-error` 红字提示 + `fieldError()` 高亮对应输入框（ID 空/重复、Base URL 空、Key 掩码/空、后端错误都显示在弹窗里，主界面 toast 被遮罩盖住看不到） | `config.rs:validate/providers`, `webui/index.html` |
| D22 | **「当前使用模型」+ 哨兵路由 + 上下文后缀**：`server.default_model` 语义改为「当前使用模型」；CC 发 `Codex-relay-model[上下文]`（哨兵）时 `router::resolve_for_request` 用 `default_model` 替换后正常路由，CC 设具体模型（非哨兵）则剥 `[ctx]` 后缀后直接路由（`[1m]`/`[200k]` 后缀在转发前剥离，上游发干净模型名）；`config::sentinel_model()` 生成 `Codex-relay-model[上下文]`（1e6→`[1m]`、千整倍→`[200k]`，与 UI 一致）；`update_settings` 顶层 `model` 键 + `env.ANTHROPIC_MODEL` **都写**哨兵值（实测顶层 model 键优先于 env，只写 env 不生效）；UI「默认模型」→「当前使用模型」 | `router.rs:resolve_for_request/split_ctx_suffix`, `config.rs:sentinel_model`, `proxy.rs:forward`, `webui.rs:update_settings` |
| D24 | **三个任务槽全指哨兵**：CC 是**多槽位模型架构**——`/model` 只设主对话；子代理 / `/compact` / `/loop` 走独立的 opus/sonnet/haiku 任务槽，自定义 base_url 下默认用内置裸名（`Codex-opus-5` 等）→ 中继无前缀可匹配 → 400（Phase 24 现场：`Agent "Explore codebase structure" failed: 400 unknown model 'Codex-opus-5'`）。`update_settings` 额外写 `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` = **哨兵值**（子代理跟随页面「当前使用模型」，用户确认全指哨兵）；`copyEnv` 复制命令与 `start-Codex.cmd` set 块同步写 | `webui.rs:update_settings`, `webui/index.html` JS, `start-Codex.cmd` |
| D25 | **托盘「模型」子菜单一键切换当前使用模型**：右键 模型 → 服务商 → 模型 两级展开，`CheckMenuItem` 勾选当前模型；选中即 `AppState::set_default_model_blocking`（`router::resolve` 校验 → `Config::save` → `blocking_write` 热重载，零重启）。生效语义=D22 哨兵路由：**仅当 CC 发 `Codex-relay-model[ctx]` 哨兵时用所选模型**，CC 指定具体模型则继续走指定，切回哨兵自动跟随。菜单在启动时按配置快照构建一次（Web UI 改配置后需重启托盘刷新） | `tray.rs:spawn`, `config.rs:set_default_model_blocking`, `main.rs`（快照） |
| D26 | **服务商使用情况：调用量/用量统计 + DeepSeek 余额**。用量中继**从自己的转发流**统计（纯透传零适配）：SSE `message_start` 输入 + `message_delta` 输出 tokens、count_tokens 响应、非 SSE 读体，按服务商+模型累加（`UsageStats`，std Mutex 短锁，自启动以来重启清零），`/api/status` 暴露、Web UI「使用情况」卡片展示。**余额无统一接口**：仅 DeepSeek `GET {主机根}/user/balance`（同 key，真机实测 ¥5.39→5.10 可用，余额接口在主机根非 `/anthropic`），`/api/balance` 按 `base_url` 是否含 `api.deepseek.com` 自动识别；火山/讯飞需各自 OpenAPI（AK/SK）不可简单查——**Phase 29：无接口的服务商不再列出（`balance_root` 白名单，`continue` 跳过），待服务商提供可查询 API 后在 `balance_root` 加识别即自动显示** | `proxy.rs`（流内提取）、`config.rs:UsageStats`、`webui.rs:balance/status`、`webui/index.html`（使用情况卡片） |
| D27 | **用量持久化（重启不清零）**：`UsageStats` serde + `save()`（原子 `.tmp`+rename，防 `/api/restart` 新旧进程交接写坏）+ `load()`（缺失/损坏→空不崩）；`AppState.usage_dirty` 脏标记，main 5s 周期落盘（dirty 才写，空闲零 IO）；文件 = 配置同目录 `{配置名去最后扩展}-usage.json`（`config.json`→`config-usage.json`） | `config.rs:UsageStats::save/load`、`main.rs`（落盘任务）、`proxy.rs`（置脏） |
| D28 | **火山方舟 Coding Plan 用量 + 浏览器 Cookie**：`ProviderConfig.console_cookie`（掩码保留尾 8 字符 + PUT 复用）+ `console_usage_url`（可选，默认按 base_url 区域推导 `.../ark/{region}/2024-01-01/GetCodingPlanUsage`）；`cookies.rs` 从 Chrome/Edge Cookie 库读取（rusqlite bundled + DPAPI + AES-128-GCM，**浏览器运行中独占锁库 → 自动读不可用，手动粘贴兜底**）；`GET /api/coding-plan` 解析 `Result.QuotaUsage[]`（`session`≈5小时/`weekly`/`monthly` + `Percent` + `ResetTimestamp`），`POST /api/import-cookie` 存配置；UI 使用情况卡「Coding Plan」块 + 弹窗 Cookie/URL 字段 | `cookies.rs`、`webui.rs:coding_plan/import_cookie`、`config.rs:console_cookie`、`webui/index.html` |
| D30 | **讯飞星辰 Coding Plan 用量（按 apiKey 匹配多套餐）**：与火山方舟同机制——`console_cookie` 存 `maas.xfyun.cn` 控制台 Cookie；`GET /api/coding-plan` 按 **`console_kind`** 分派（base_url 含 `xfyun.cn`/`xf-yun.com` → Xf；volces/volcengine → Ark；未识别域按 `console_usage_url` 路径、存档 Cookie 兜底 Ark），xf 查 `xf_usage_url`（默认 `https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`）解析 `data.rows[]`，**按 `codingPlanAppCredentialDTO.apiKey == p.api_key` 过滤**——多个配置服务商/多匹配行 → 各显各的套餐（包量%/剩余、5小时/一周 请求配额、到期）；`cookies.rs` 域参数化 `read_console_cookie(domain_like, display)`（新增 `read_xf_console_cookie` 查 `%xfyun.cn`）；响应带 `kind` 字段，UI `fmtXfPlans` 渲染；`import-cookie` 按服务商类别读对应域 | `cookies.rs`、`webui.rs:console_kind/parse_xf_coding_plan`、`webui/index.html:fmtXfPlans` |
| D36 | **OpenAI 协议翻译**：`ProviderConfig.protocol`（`anthropic` 默认省写 / `openai`）让仅 OpenAI Chat Completions 协议的服务商（OpenRouter/Qwen 百炼）可用。上游 URL=`{base}/chat/completions`、剥 `anthropic-version/beta` 头；请求体 `anthropic_to_openai`（system→system 消息、tool_use/tool_result→tool_calls/tool、image→image_url、tools→function、tool_choice 映射、stream→include_usage）；响应非流式 `openai_message_to_anthropic`、流式 `OpenAiStreamTranslator`（OpenAI SSE chunk→Anthropic 事件序列，文本/工具块缓冲，收尾提取 usage）。count_tokens 直接启发式。UI 弹窗加「上游协议」select | `translate.rs`、`proxy.rs:forward/is_openai/openai_sse_stream`、`config.rs:protocol`、`webui/index.html:m-protocol` |
| D38 | **术语「服务商」→「模型订阅」+ 服务商必填预设 + Cookie 按服务商显示**：UI 配置条目全称「模型订阅」（添加/编辑/保存/列表/删除/使用情况/清零/确认弹窗全改名），「服务商」特指上游厂商（DeepSeek/讯飞/火山/Kimi/GLM/Anthropic 预设下拉，**必填**）；选预设自动填 ID 简写（可改）、编辑按 id/base_url 推断（`presetOfProvider`）、自定义保留手填；**控制台 Cookie 输入框仅服务商讯飞/火山时显示**（`toggleCookieField`）；使用情况/列表/默认模型下拉的 ID 简写统一 `provName` 显示服务商名称 | `webui/index.html`（`provName`/`presetOfProvider`/`toggleCookieField`/`#m-cookie-wrap`） |
| D39 | **页面字体紧凑/重叠优化**：全局 `body{letter-spacing:.012em;-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}`（CJK 字距，放 `<style id="ui">` 手写块，build_ui 不覆盖）+ `.leading-loose-inline{line-height:1.7}`；徽标/胶囊一律 `shrink-0 whitespace-nowrap min-w-[64px]`；**任何 `flex-wrap` 容器必须带 `gap-y-*`**（防换行贴行）；最小字号 11-12px、垂直内边距 ≥ `py-0.5`；讯飞 pill `text-xs leading-none px-2 py-1`；小字号长段说明用 `text-xs leading-loose-inline` | `webui/index.html`（`<style id="ui">`、徽标/pill/gap-y 类） |
| D40 | **页面加宽防换行**：`<main>` `max-w-3xl`→`max-w-4xl`（768→896px）；单行 nowrap 元素组做宽度预算（估算元素总宽反推容器 min-width），`flex-wrap`+`gap-y-*` 是窄窗口兜底 | `webui/index.html` |
| D41 | **输入 tokens 启发式兜底**：讯飞星辰等上游不在 `message_start` 返回 `usage.input_tokens`（真机确认 in=0）→ `UsageSink.input_fallback`（请求体字符/4，`heuristic_input()` 同 D8 规则）；`rewrite_line` 对 `message_start` 行强制慢路径累计（防快路径漏记）；`accumulate` 无输入时用 fallback；非流式 + translate finalize 同兜底。**估算非计费值**，上游返回真实值则用真实（`usage_uses_real_input_when_present` 锁定） | `proxy.rs:UsageSink/heuristic_input/rewrite_line`, `translate.rs:OpenAiStreamTranslator.finalize` |
| D42 | **托盘「更新」子菜单（更新 Codex 配置 / 更新环境变量）+ Win32 信息气泡**：托盘「更新」Submenu（Phase 43）两项点击 → `webui::update_settings_blocking`（同步核心 `write_claude_settings` 与 async handler 共用；`cfg.blocking_read()` 同 D25 `blocking_write` 模式，std 线程安全）→ 写 `~/.Codex/settings.json` 哨兵 model + env 6 键 → **`Shell_NotifyIconW` 气泡**反馈（服务 `--autostart` 黑框隐藏时 stderr 不可见，气泡是唯一可见反馈；`NIF_INFO` 经典 hWnd+uID——隐藏消息窗口 `HWND_MESSAGE`+`DefWindowProcW`+`OnceLock` 单例；**NIF_GUID+hWnd=null 方案 Win10 实测 E_FAIL**，findings §2.37） | `tray.rs:balloon/balloon_window`, `webui.rs:update_settings_blocking/write_claude_settings` |
| D53 | **Coding Plan 控制台 Cookie 每小时自动续期**（保持登录态不过期，隔天不必重新登录）：**用存档 Cookie 请求用量接口，从响应 `Set-Cookie` 取新 Cookie 续期**——浏览器不必登录（手动粘贴/导入的 Cookie 即登录态）。main 后台任务启动 ~30s 先续期一次、此后每 1h 调 `webui::refresh_console_cookies`（async，reqwest 请求）；`coding_plan` handler 查询时**同样捕获 Set-Cookie 续期**（页面刷新即保鲜）。**Phase 53d 续期按 key 合并（非整体替换）**：`parse_set_cookies` 解析响应 Set-Cookie → `(name,value)`（去属性、同名后者覆盖、**跳过空值**——上游 `sessionid=; Expires=...` 是清除型，存空值会污染存档导致 401）；`merge_cookies(old, updates)` 只把响应里的 key 更新进旧存档（同名覆盖、新 key 追加），**旧存档其它 key 原样保留**（上游通常只续期 session 相关几个 cookie，整体替换会丢 region 等关键 key）；`apply_cookie_renewals` **合并后与存档不同才写盘+热重载**（`.read()/.write().await`，勿在 async 用 blocking 版），请求失败/无 Set-Cookie/合并无变化 → 保留旧值零写盘。浏览器读取（`read_browser_cookie`）仅作 `console_cookie` 为空时兜底。注意 `console_kind` 有「存档 Cookie 即默认 ark」回退——存了 Cookie 的非火山服务商也会被当 ark 请求火山用量端点 | `main.rs`（后台任务）, `webui.rs:refresh_console_cookies/parse_set_cookies/merge_cookies/parse_cookie_str/apply_cookie_renewals/read_browser_cookie` |
| D53c | **`console_cookie` 全链路明文（不再 `*` 掩码）**：用户要求便于调试/核对——`Config::masked()` 只掩 api_key，cookie 原样返回；PUT 直接存前端明文（删掉 cookie 的 `is_masked` 复用分支，api_key 复用保留）；`renewal_json`/`log_cookie_renewal` 诊断 old/new 明文；`/api/import-cookie` 返回明文 `cookie`（字段 `cookie_masked`→`cookie`）。已删 `mask_cookie`。**续期诊断进浏览器 console**（Phase 53b）：`/api/coding-plan` 每服务商带 `renewal`（`status`: renewed/same/none/http_error/request_failed + `http` 状态码 + `old`/`new` 明文），前端 `console.log('[coding-plan]', …)`；`http_error` 分支读 body 截断 200 字符放 `error`（火山方舟 401 时能看到「未登录」等上游原因；**401 上游不会回填 Set-Cookie**，需重新导入）。**Phase 53e 逐 key 对比明细**：`renewal_detail_lines(old,new)` 纯函数生成每 key 对比行（`sessionid: 旧值 → 新值` / `theme: (新增) → 值` / `region: 值（未变）`，key 排序去重）——`log_cookie_renewal` 控制台打印 + `renewal.detail` 数组进浏览器 console | `config.rs:masked`（只掩 key）, `webui.rs:config_put/renewal_json/log_cookie_renewal/renewal_detail_lines/import_cookie/coding_plan`, `webui/index.html:loadCodingPlan` |
| D55 | **桌面悬浮卡片（GDI+ 重新实现）**：原生 Win32 **标准顶置 popup 窗口**（`WS_POPUP \| WS_EX_TOPMOST \| WS_EX_TOOLWINDOW`，**不用分层窗口/ULW**--彻底绕开「分层窗口须经 UpdateLayeredWindow 合成否则不可见」的坑）实时显示当前模型的今日用量/余额/Coding Plan。**绘制用 GDI+**（`Win32_Graphics_GdiPlus` feature，系统 gdiplus.dll，不增 exe 体积；windows-sys 0.59 无 Direct2D/DirectWrite feature 故弃 D2D 方案）`WM_PAINT` 里 `GdipCreateFromHDC` 直接画窗口 DC（GDI+ 画窗口是最可靠用法），圆角用 `SetWindowRgn(CreateRoundRectRgn)` 裁剪；背景整面不透明渐变全覆盖。卡片窗口建在**托盘线程**（消息泵调度）；数据任务（main tokio）每 5s 读 `state.usage` 内存快照（不走 HTTP）、每 30s HTTP 自调 `/api/balance`+`/api/coding-plan?renew=0`（renew=0 只读不写盘），**仅数据变化才 PostMessageW 重绘**（空闲零重绘、CPU≈0）。**所有 ShowWindow 显示路径（创建 + 托盘切换）必须随后 `InvalidateRect(erase=1)+UpdateWindow` 强制同步重绘**：隐藏中的窗口 Windows 不投递 WM_PAINT，`WM_ERASEBKGND=>1` 又跳过擦除，未初始化表面位（DWM 常以屏幕内容填充）会一直当背景--「点击显示把当前页面当卡片背景」的坑。拖拽：`WM_NCHITTEST`->HTCAPTION 系统原生拖拽；关闭区 HTCLIENT 收 `WM_LBUTTONUP` 隐藏；`WM_MOUSEACTIVATE`->MA_NOACTIVATE 不抢 CC 焦点；双击卡片开配置页。位置/可见性存 `*-card.json`（原子写），重启 clamp 进工作区。PMv2 DPI 感知（main 进程级开启），逻辑尺寸按 `GetDpiForSystem` 缩放到物理像素 | `widget.rs`（`GdiEnv`/`paint`/`draw_content`/`card_wndproc`/`toggle_visible`/`compute_snapshot`/`spawn_data_task`），`tray.rs`（「桌面卡片」菜单项），`main.rs`（`WidgetShared::new`+`spawn_data_task`） |
| D58 | **OpenCode Go 网关会话头/UA 注入**：OpenCode Go（`opencode.ai`，base_url 如 `https://opencode.ai/zen/go/v1`）自 2026-09 起要求每个请求带 `x-opencode-session`（GPU 后端路由 + prompt-cache 亲和；缺失 400 `MissingSessionID`）+ **专属 UA**（`my-coding-agent/1.0` 风格，非通用 HTTP/SDK 库名）。ClaudeRelay 是白名单转发，`user-agent`/`x-claude-code-session-id` 本被剥离 → 出站无会话头 + reqwest 通用库 UA → 400。`ProviderConfig::is_opencode()`（base_url 含 `opencode` 或 console_usage_url 含 `/workspace/`/`opencode`，与 webui `console_kind` Oc 分支同口径）命中时转发注入：**`x-opencode-session` 取入站 CC 原生会话头 `x-claude-code-session-id`**（v2.1.86+ 每请求自动发，每段对话唯一、对话内跨轮/resume 稳定），CC 未发 → 进程级 OnceLock 回退值（防辅助路径 400）；**User-Agent 透传 agent 风格 UA（`claude-cli/<v>`）**，通用 HTTP 库 UA（`Python-urllib`/`curl`/`reqwest`/`Go-http-client`…，`is_generic_lib_ua` 词首匹配）或缺失 → 覆盖为专属 `ClaudeRelay/<v>`。仅 opencode 服务商生效，其余协议零改动 | `config.rs:ProviderConfig::is_opencode`、`proxy.rs`（`opencode_session_value`/`opencode_fallback_session`/`opencode_default_ua`/`is_generic_lib_ua`） |
| D59 | **统一自愈式原子写盘**：所有落盘点（config.json、`*-usage.json`、`~/.claude/settings.json`、`/models` 命令、卡片状态）统一走 `config::write_file_atomic`——先写 `.tmp` 再 rename（**此前 config.json 是直写**，写一半断电会损坏配置导致下次起不来，一并修复）；**只读属性自动解除重试**（std `Permissions` 在 Windows 映射 FILE_ATTRIBUTE_READONLY，无需 windows-sys；跨机拷贝/U盘/网盘/压缩包解出常见，表现为 os error 5「拒绝访问」，自愈后打 `[info]` 日志）；杀毒/同步盘瞬时锁定短退避重试 4 次（最多约 600ms，失败路径才 sleep）；父目录缺失自动创建（新装机器首写 `~/.claude`）；最终仍失败（目录 ACL 无写权限，如放进 Program Files）→ os error 5 错误信息附排查指引，UI 原样展示。单测 `write_file_atomic_roundtrip_and_heal`（roundtrip/父目录自动创建/无 tmp 残留/只读自愈） | `config.rs:write_file_atomic/clear_readonly`、`config.rs:Config::save/UsageStats::save`、`webui.rs:write_claude_settings`、`commands.rs:ensure_models_command`、`widget.rs:CardState::save` |
| D60 | **模型行级上游协议覆盖（Phase 60）**：`ModelEntry.protocol: Option<String>`（自定义 serde 双形向后兼容——字符串形/缺省=None、空串归一 None；alias==real+默认上下文+无协议仍序列化为纯字符串，旧配置零 diff）；`router::Resolved` 携带行级协议，proxy 有效协议=**行覆盖??服务商级**（皆空=anthropic 纯透传），URL/请求翻译/响应回译/count_tokens 全部跟随有效协议；UI 模型行五列布局加「上游协议」下拉（跟随服务商/Anthropic 透传/OpenAI/Gemini）。适用：**同一 base_url 同时提供多协议端点**的服务商（one-api 类聚合站 `https://host` 同时有 `/v1/messages` 与 `/chat/completions`）；OpenCode 两 API 族路径不同仍建议分开订阅 | `config.rs:ModelEntry.protocol`、`router.rs:Resolved`、`proxy.rs`（有效协议分派）、`webui/index.html`（模型行「上游协议」下拉） |
| D61 | **上游断流两种形态统一兜底：Err + EOF（Phase 59/61）**：Phase 59 只修 Err 分支（RST/网络错误）——上游对长空闲（thinking 无输出）常被聚合网关 **FIN 干净掐流**（reqwest 返回 `Ok(None)` 非 Err），三个流包装器把 EOF 当正常结束：anthropic 透传收到无 message_stop 的残流（CC 报 `response stream was malformed`）、openai/gemini 翻译器 `finalize()` 伪造完整收尾（tool_use JSON 半截 → CC 报 Invalid tool parameters/静默丢尾），且全程无日志。Phase 61：三内核加终止判据——`ModelRewriter.saw_stop`（字节级 windows 检测行含 message_stop，零解析）、`OpenAiStreamTranslator::stream_terminated()`（收到 finish_reason 或已按 [DONE] finalize，兼容不发 [DONE] 的站）、`GeminiStreamTranslator::stream_terminated()`（收到 finishReason 块）；包装器 EOF 时未终止 → `[warn] 上游流提前结束(疑似网关空闲超时)` 日志 + 发 `sse_error_chunk("上游流提前结束(未收到完整回复)")` + 干净收尾；已终止 → 行为同今。e2e `/eof/chat/completions` 路由（不声明 Content-Length，HTTP/1.0 close-delimited → `Ok(None)`）与 `/trunc`（Content-Length 欺骗 → Err）互为对照覆盖两形态。现场特征：报错集中在「Thought Ns」后的工具调用阶段、时长 60~120s 级——即网关空闲超时掐流 | `proxy.rs`（三包装器 EOF 分支/`ModelRewriter.saw_stop`）、`translate.rs:stream_terminated`、`gemini.rs:stream_terminated`、`test/e2e.py`（/eof 路由+6 断言） |
| D17 | 模型别名映射：`models` 支持 `{alias,real,context}`（context 默认 200000，向后兼容字符串）；`server.default_model` 存默认模型别名；`POST /api/update-settings` 只把 BASE_URL/KEY/MODEL 3 键合并进 `~/.Codex/settings.json` env（尊重 `CLAUDE_CONFIG_DIR`），其余内容保留 | `config.rs:ModelEntry`, `router.rs:resolve`, `webui.rs:update_settings` |
| D18 | **托盘「停止」= 释放端口停掉整个服务**（浏览器访问显示服务不存在），不是 503：main 的 `run_server` 循环用 `tokio::sync::watch::Sender<bool>` 承载停止状态（托盘线程 send、循环 borrow/changed）；停止时 `tokio::select!` drop 掉 `axum::serve` future → Serve 拥有 TcpListener → 端口关闭；启动重新 bind。每轮 `borrow_and_update()` 丢弃陈旧唤醒防误停；async_main 保留 Sender clone 保活通道 | `main.rs:run_server`, `tray.rs:spawn` |
| D19 | **开机自启 + 双击友好**：`server.auto_start`（默认 false）→ `autostart.rs` 写 `HKCU Run` 键 `"<exe>" --autostart`；保存即生效、启动只注册不删除（防测试配置误删）。双击启动：`GetConsoleProcessList` n==1（独占控制台）→ `ShowWindow(SW_HIDE)` 隐藏黑框（只隐藏不 FreeConsole，stdout 仍有效）；`--autostart`=后台启动不弹浏览器；`gui_launch && !autostart` → run_server 首次就绪打开配置页；`port_is_busy` 已运行 → 双击开页面+退出。config 路径 CWD 缺失回退 exe 目录 | `src/autostart.rs`, `main.rs:hide_console_if_double_clicked/port_is_busy`, `config.rs:auto_start` |

### 数据流（一次 `/v1/messages` 转发）

```
CC 请求 → proxy::messages()
  → 读 body → 解析 JSON → 取 model 字段
  → router::resolve(&providers, model, sep) → (provider_idx, real_model)
  → json["model"] = real_model
  → 白名单透传请求头 (FORWARD_HEADERS)
  → 替换认证头 (Bearer 或 x-api-key)
  → reqwest POST {base_url}/v1/messages
  → 响应: SSE → sse_rewrite_stream() 逐行把真实 model 改回别名
         非 SSE → 字节流透传
  → 响应头白名单透传 (anthropic-*, x-*, content-type, retry-after)
  → 返回 CC
```

### 配置文件格式

```json
{
  "server": { "host": "127.0.0.1", "port": 8787, "separator": "-", "default_model": "ds-flash" },
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
      "rewrite_model": true,
      "protocol": "anthropic",                           // Phase 36：anthropic（默认纯透传）或 openai（自动翻译，仅 OpenAI 协议服务商用）
      "console_cookie": "sessionid=...; __Secure-...",   // 可选：火山方舟/讯飞星辰 Coding Plan 用量查询 Cookie（掩码保存）
      "console_usage_url": "..."                          // 可选：用量查询 URL（默认按服务商推导）
    }
  ]
}
```

- `id`：服务商前缀，全局唯一，如 `ds` → 模型别名 `ds-deepseek-chat`
- `separator`：`{id}{sep}{model}` 的分隔符，默认 `-`
- `models`：字符串（别名==真实名，上下文默认 200K，向后兼容）或 `{ "alias": "简写", "real": "真实模型", "context": 上下文窗口 tokens（可选，默认 200000） }`；CC 里输入 `{id}-{alias}`（如 `ds-flash`）→ 转发时用 `real`。context 仅 UI 展示元数据，不影响转发；**模型串可带 CC 上下文后缀**（如 `ds-v4-flash[1m]`），中继转发前自动剥离（D22）
- `server.default_model`：**当前使用模型**完整别名（如 `ds-v4-flash`）；空则回退第一个服务商第一个模型别名。CC 发 `Codex-relay-model[上下文]` 哨兵时用它替换；另供 `--print-default-model`、复制环境变量、更新 Codex 配置/环境变量生成哨兵值（D22）
- `auth`：`bearer`（Authorization: Bearer）或 `x-api-key`
- `protocol`（可选，默认 `anthropic`）：上游协议——`anthropic` = 纯透传（D3，默认）；`openai` = 请求/响应自动翻译为 OpenAI Chat Completions（Phase 36，解锁 OpenRouter/Qwen 百炼等仅 OpenAI 协议服务商；上游 URL=`{base}/chat/completions`，剥离 `anthropic-version/beta` 头，count_tokens 回退启发式）
- `console_cookie`（可选）：**Coding Plan 用量查询**用的控制台 Cookie——火山方舟 `console.volcengine.com` / 讯飞星辰 `maas.xfyun.cn`（掩码展示保留尾 8 字符，PUT 复用同 Key 机制 D13；留空则查询时自动读浏览器对应域，**浏览器运行中锁库时需手动粘贴或关浏览器后点「导入 Cookie」**）
- `console_usage_url`（可选）：用量查询 URL，默认按服务商推导——火山方舟 `https://console.volcengine.com/api/top/ark/{region}/2024-01-01/GetCodingPlanUsage`（`ark.cn-beijing.volces.com` → `cn-beijing`）；讯飞星辰 `https://maas.xfyun.cn/api/v1/gpt-finetune/coding-plan/list?page=1&size=6`
- 配置热重载：通过 Web UI 保存或直接改 config.json，下一请求自动生效（`Arc<RwLock>` 原地替换，无需重启）

### 启动脚本（start-Codex.cmd）

纯 ASCII 批处理，流程：
1. 检测 relay 是否已运行（`curl /healthz`）
2. 未运行则 `start /min` 后台最小化启动（**带 `--autostart`**：D19 下 `start` 新建独立控制台被隐藏、不弹配置页）
3. 等待就绪
4. 用 `--print-default-model` 打印哨兵模型值 `Codex-relay-model[上下文]`（提示用）
5. 设 `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY`/`ANTHROPIC_MODEL` + **三个任务槽 `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`**（=哨兵值，子代理/compact 跟随页面模型，D24）环境变量
6. `Codex --settings Codex-settings.json %*` 启动 CC（settings 隔离）

## 注意事项

- **GNU 工具链**：本项目使用 `x86_64-pc-windows-gnu`。TDM-GCC 10.3 已手工补齐 `libgcc_eh.a` 空归档（见 `findings.md` §1），移走/更换编译器后需重新确认。
- **release profile 极致体积**：`opt-level="z"` + `lto` + `codegen-units=1` + `panic="abort"` + `strip`。不要随意调低这些优化级别。
- **测试 mock 上游**：历史 mock 测试文件在 `test/` 目录（如 `test/mock_upstream.py`）；`cargo test` 当前 **108 个单元测试**（config 21 + proxy 20 + webui 19 + router 14 + widget 10 + translate 9 + gemini 8 + cookies 5 + main 1 + commands 1，含 `tray_stop_releases_port_and_start_rebinds`、`usage_stats_persist_roundtrip`、`query_cookies_reads_volcengine_rows`、`dpapi_roundtrip`、`parse_xf_coding_plan_filters_by_api_key`、`usage_reset_clears_and_saves`、`openai_stream_translates_text_and_tool_calls`、`usage_fallback_when_upstream_missing_input`、`write_claude_settings_merges_env_and_top_model`、`ensure_models_command_creates_and_preserves`、`parse_set_cookies_extracts_and_dedupes`、`merge_cookies_updates_by_key_keeps_others`、`renewal_detail_lists_per_key_comparison`、`cookie_renewal_applies_only_changed`、`parse_oc_usage_zero_usage_missing_reset_ok`、`provider_is_opencode_detects`、`opencode_session_uses_client_header_when_present`、`opencode_session_fallback_is_stable_nonempty`、`generic_lib_ua_detection`、`resolve_carries_model_protocol`、`openai_stream_upstream_error_emits_error_event_and_closes`、`passthrough_stream_upstream_error_emits_error_event`、`passthrough/openai/gemini_stream_premature_eof_emits_error_event`（Phase 61 EOF 截断）、`widget::fmt_tokens/fmt_ctx/provider_color/snapshot_strips_ctx_suffix/snapshot_falls_back_first/snapshot_today_bucket/snapshot_paused/card_state_roundtrip`）；另有 `python test/e2e.py` 端到端验收（mock 上游 + relay 子进程，**110 项断言**，relay 用 `CREATE_NO_WINDOW` 启动；mock 含 `GET /GetCodingPlanUsage`（Phase 28）、`GET /coding-plan/list`（Phase 30，双行按 apiKey 过滤）、`GET /workspace/{id}/go`（Phase 48，含 `monthlyUsage: null` 占位，Phase 52）、`POST /chat/completions`（Phase 36，OpenAI 流式）、`/trunc/chat/completions`（Phase 59，声明 Content-Length 不写完 → reqwest Err 断流）、`/eof/chat/completions`（Phase 61，不声明 Content-Length 干净 FIN → EOF 断流；两者均断言 relay 发规范 `event: error`、不发 message_stop）、`POST :streamGenerateContent`+`:generateContent`（Phase 56，Gemini 流式/非流式 + x-goog-api-key 认证断言）断言；Phase 58 另断言 opencode 服务商转发时把入站 `x-claude-code-session-id` 映射为 `x-opencode-session`、agent UA 透传、通用库 UA 覆盖为 `ClaudeRelay/<v>`、非 opencode 服务商不受影响；三个用量端点带 `Set-Cookie` 头验证续期，ark 端点双 Set-Cookie 验证**按 key 合并**（region 保留/theme 追加，Phase 53d）。
- **浏览器 Cookie 自动读取限制（D28）**：新版 Chrome/Edge **运行中独占锁定 Cookie 库**（os error 32），`/api/import-cookie` 在浏览器运行时读不到——报「关闭浏览器或手动粘贴」；**手动粘贴 `console_cookie` 到服务商配置是可靠路径**（关浏览器后导入也行）。rusqlite(bundled) 是首个 C 编译依赖，release exe 从 2.37MB 增至 **3.24MB**（仍 <6MB）。
- **大请求谜题**：CC 大请求（~1MB）偶发静默 400/挂起。proxy.rs 已加诊断日志（body parse 失败/无 model/model 解析失败三处），待复现定位。排除 relay 大 body 问题（1.2MB 合成请求正常）。
- **生产排障日志（Phase 62）**：运行中 relay 的全部 stderr 诊断（断流 `[warn]` D61、托盘/卡片/cookie 续期告警）落 **exe 同目录 `relay.log`**（append，>5MB 启动截断）——CC 报 malformed/stream 异常时**先看该文件尾部**：有 `[warn] 上游流提前结束/中断` = 上游断流已被中继兜底并显性化；无记录但 CC 报错 = 上游在首字节前断流或上游返回内容残缺但正常闭合（如 tool arguments 仅 `{` 即 finish_reason，CC 报 InputValidationError）——属上游服务质量问题，中继无法判定。改代码后重启 relay 须用 WMI 方式（`Invoke-CimMethod Win32_Process Create`）：Bash/Start-Process 启动的进程会随工具调用结束被连带清理（2026-10-20 实证，源会话「PID 7908 之谜」同因）。
- **Web UI 改版要重跑构建链**：`webui/index.html` 的样式来自 Tailwind 编译产物（`<style id="tw">` 占位块）。改过 UI 后必须 `python webui/build_ui.py` → `cargo build --release` → PID 定向重启 relay，否则 exe 内嵌的是旧样式。build_ui.py 幂等，可重复运行。
- **配置文件是项目敏感信息**：`config.json` 和 `Codex-settings.json` 含真实 API Key，不应提交 git（`.gitignore` 已忽略）。
- **CC 自定义 `/models` 命令（Phase 44/45）**：`~/.Codex/commands/models.md`——输 `/models` 时模型 curl `http://127.0.0.1:8787/api/config` 并按服务商分组渲染「完整别名→真实模型→上下文」+ 当前使用模型。**双击运行 exe 即自动安装**（`src/commands.rs` 内嵌 `assets/commands/models.md`，已存在不覆盖）；**CC 在会话启动时加载自定义命令**，新增命令文件需重启 CC（或新开会话）才可见。可复制该命令文件到其他机器即生效。
