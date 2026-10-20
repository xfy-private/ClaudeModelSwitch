# ClaudeRelay

**Claude Code 多服务商多模型动态切换中转服务**（Rust 单 exe，零依赖）。

把 Claude Code（下称 CC）通过 `ANTHROPIC_BASE_URL` 指向 ClaudeRelay，即可在 CC 内用
`/model {服务商前缀}-{模型名}` **免重启**地切换到任意服务商的任意模型。

> 核心思路：**全 Anthropic 协议纯透传** —— 改 model → 换 Key → 原样转发 → SSE 流式透传。
> 不改写请求协议，不翻译事件序列，只做「前缀路由 + 认证替换 + 流式透传」。

## 特性

- 🎛️ **可视化配置**：内置 Web 配置页（浏览器打开 `http://127.0.0.1:8787`），服务商增删改查、模型列表、Key 掩码保护、**弹窗保存即持久化热重载**；顶部操作栏 **5 个按钮分色**：保存配置 / **保存配置并重启**（改 host/port 后一键生效）/ **更新 claude 配置** / **更新环境变量**（都只把 3 个 `ANTHROPIC_*` 键写入 `~/.claude/settings.json`，其余保留）/ 复制 CC 环境变量命令
- 🔤 **模型别名 + 上下文**：`models` 可配 `{ "alias": "flash", "real": "deepseek-v4-flash", "context": 128000 }`，在 CC 里 `/model ds-flash` 即可（简写好记，仍拼前缀）；弹窗内三列列表（别名/真实名/上下文大小下拉默认 200K）编辑，默认模型可搜索下拉一键设置 `ANTHROPIC_MODEL`
- 🔄 **`/model` 动态切换**：`/model ds-deepseek-chat` / `/model kb-moonshot-v1-32k`，同会话零重启
- 🧭 **最长前缀匹配**：模型名本身含 `-`（`deepseek-chat`、`gpt-4o`）也不误切，按**已配置服务商 id** 最长前缀命中
- 🔐 **认证替换**：剥离 CC 的 `x-api-key`/`Authorization`，换成服务商真实 Key（`Bearer` 或 `x-api-key` 可配）
- 📦 **极小体积**：release exe 约 **2.16MB**，空闲内存 **~9-10MB**，空闲 CPU **≈0**
- 🎨 **Tailwind CSS 界面**：配置页为 Tailwind v4 编译内嵌静态样式（自包含、离线可用，非 CDN）
- 🪟 **系统托盘**：右键「打开配置页」/「退出」
- 🚀 **一键启动**：`start-claude.cmd` 自动拉起 relay + 加载隔离的 CC 配置

## 架构

```
Claude Code ──(Anthropic Messages)──▶ ClaudeRelay (127.0.0.1:8787)
                                        │
                                        ├─ /v1/messages            ← 转发 + SSE 流式透传
                                        ├─ /v1/messages/count_tokens ← 转发 + 启发式回退
                                        ├─ /v1/models              ← 聚合全部别名
                                        ├─ /healthz                ← 存活探测
                                        ├─ /                       ← 配置 Web UI
                                        └─ /api/config, /api/status ← Web UI 读写
                                              │
                              ┌───────────────┼───────────────┐
                              ▼               ▼               ▼
                        DeepSeek          Kimi          Anthropic
                        (各自 Anthropic 兼容端点，base_url 指向 /v1/messages)
```

## 快速开始

### 1. 构建（可选，已发布 exe 可直接用）

```bash
# 目标工具链 x86_64-pc-windows-gnu（详见 CLAUDE.md）
python webui/build_ui.py  # 编译 Tailwind CSS 并注入 webui/index.html（仅改过 UI 后需要）
cargo build --release
# 产物 target/release/claude-relay.exe（~2.16MB）
# 测试（18 个单测 + 端到端验收）
cargo test
python test/e2e.py        # 端到端验收：配置→启动→切换→对话→工具 全链路 25 项断言
```

### 2. 配置服务商

两种方式任选：

- **Web UI**：`start-claude.cmd` 启动后（或手动跑 relay），浏览器打开 `http://127.0.0.1:8787`，
  按界面增删服务商（API 地址 / Key / 三列模型列表：别名、真实名、上下文大小 / 认证方式），保存即生效。
- **手写 `config.json`**：

```json
{
  "server": { "host": "127.0.0.1", "port": 8787, "separator": "-" },
  "providers": [
    {
      "id": "ds",
      "name": "DeepSeek",
      "base_url": "https://api.deepseek.com/anthropic",
      "api_key": "sk-你的Key",
      "auth": "bearer",
      "models": [
        "deepseek-chat",
        { "alias": "flash", "real": "deepseek-v4-flash", "context": 128000 }
      ],
      "strip_beta": false,
      "rewrite_model": true
    },
    {
      "id": "kb",
      "name": "Kimi",
      "base_url": "https://api.moonshot.cn/anthropic",
      "api_key": "sk-你的Key",
      "auth": "bearer",
      "models": ["moonshot-v1-32k"]
    }
  ]
}
```

### 3. 启动并使用

**双击 `start-claude.cmd`**（推荐）——自动完成：检测/拉起 relay → 读默认模型 → 进入 CC。

进入 CC 后：

```
/model ds-deepseek-chat      # 切到 DeepSeek 的 deepseek-chat
/model ds-deepseek-reasoner  # 切到推理模型，零重启立即生效
/model kb-moonshot-v1-32k    # 切到 Kimi
```

也可手动方式（不隔离 settings，直接改环境变量）：

```bat
set ANTHROPIC_BASE_URL=http://127.0.0.1:8787
set ANTHROPIC_API_KEY=relay-dummy        :: 中继会替换成服务商真实 Key
set ANTHROPIC_MODEL=ds-deepseek-chat
claude
```

## 使用详解

### `/model` 切换

- 任意 `{服务商id}-{模型名}` 都会被 CC 原样透传（本机 CC 版本实测接受自定义名）。
- 中继按**最长前缀匹配**：`ds-deepseek-chat` → 服务商 `ds`，真实模型 `deepseek-chat`。
- **别名**：若服务商配了 `{ "alias": "flash", "real": "deepseek-v4-flash" }`，`/model ds-flash` 也会命中并转发 `deepseek-v4-flash`；真实模型名本身同样可用（`ds-deepseek-v4-flash`）。
- 未匹配时返回 400，并在错误信息里列出全部可用别名（`{id}-{alias}`）。

### start-claude.cmd（settings 隔离，D14）

CC 的 `settings.json` 里 `env` 会**覆盖** shell 里 `set` 的同名环境变量——若你的
`~/.claude/settings.json` 里写死了 DeepSeek 直连，shell 的 `set ANTHROPIC_BASE_URL` 会被压过。

`start-claude.cmd` 的解法：复制一份 settings 到项目 `claude-settings.json`，把里面的
`ANTHROPIC_*` 指向 relay，再用 `claude --settings claude-settings.json` 启动。
**主 settings.json 保持日常直连不动**，两种用法彻底隔离。

### 配置字段参考

| 字段 | 说明 | 默认 |
|------|------|------|
| `server.host` / `server.port` | 监听地址（仅绑 127.0.0.1） | `127.0.0.1` / `8787` |
| `server.separator` | 前缀与模型名的分隔符 | `-` |
| `server.default_model` | 默认模型完整别名（如 `ds-flash`），空则回退第一个服务商第一个模型 | `""` |
| `provider.id` | 服务商前缀，全局唯一 | — |
| `provider.name` | 显示名（可选） | `""` |
| `provider.base_url` | **Anthropic 兼容端点根**，中继拼 `/v1/messages` 转发 | — |
| `provider.api_key` | 服务商真实 Key | — |
| `provider.auth` | `bearer`（`Authorization: Bearer`）或 `x-api-key` | `bearer` |
| `provider.models` | 模型列表：字符串（别名==真实名，上下文默认 200K）或 `{ "alias": "简写", "real": "真实模型", "context": 上下文窗口 tokens }`；CC 里 `/model {id}-{alias}` | `[]` |
| `provider.strip_beta` | 是否剥离 CC 的 `anthropic-beta` 头 | `false` |
| `provider.rewrite_model` | 是否把响应流里 `message.model` 改回带前缀别名 | `true` |

### CLI 参数

```
claude-relay [--config config.json] [--host 127.0.0.1] [--port 8787] [--debug]
             [--print-default-model]
```

- `--debug`：打印每次请求的方法/路径/体积、model 改写结果、上游状态码
- `--print-default-model`：打印第一个服务商第一个模型别名后退出（供启动脚本取默认模型）

## 性能指标（实测）

| 指标 | 数值 |
|------|------|
| release exe 体积 | **2.16MB** |
| 空闲内存 | **~8.7MB**（有流量后 ~10MB） |
| 空闲 CPU | **≈0** |
| 单测 | 18 个单元测试全过（proxy 6 + router 7 + config 5） |
| 端到端验收 | 25 项断言全过（`python test/e2e.py`） |

## 故障排查

| 现象 | 排查 |
|------|------|
| CC 报 400 "unknown model" | `/model` 名写错，错误信息里有全部可用别名；或服务商未配置该模型 |
| 一直连不上 DeepSeek | 确认 `base_url` 是该服务商**官方 Anthropic 兼容端点** |
| 启动脚本进不了 relay | 检查 `~/.claude/settings.json` 的 `ANTHROPIC_BASE_URL` 是否写死直连 → 用 `start-claude.cmd`（settings 隔离） |
| 大请求偶发挂起/400 | 已知疑难（CC ~1MB 请求偶发），`--debug` 日志已埋三处诊断点（body 解析失败/无 model/模型解析失败）；复现时把日志发给我们定位 |
| Key 被 UI 掩码显示成 `****abcd` | 正常（D13 掩码往返）：未改动的 Key 保存时会按 id 自动复用真实值，**新服务商不能填掩码 Key** |

## 项目结构

```
├── Cargo.toml            # release 极致体积：opt-level=z + LTO + panic=abort + strip
├── config.json           # 运行配置（含真实 Key，勿提交 git）
├── claude-relay.exe      # release 产物（与脚本同目录分布）
├── start-claude.cmd      # 一键启动：拉起 relay + settings 隔离进 CC
├── claude-settings.json  # settings 副本（env 指向 relay），勿提交 git
├── src/
│   ├── main.rs           # CLI、单线程 tokio runtime、启动 banner
│   ├── config.rs         # 配置结构/校验/读写/热重载（Arc<RwLock>）
│   ├── router.rs         # 最长前缀匹配：`前缀-模型` → (服务商, 真实模型)
│   ├── proxy.rs          # /v1/messages 转发 + SSE 透传 + model 改写 + Key 替换
│   ├── api.rs            # 路由、/v1/models、/healthz
│   ├── webui.rs          # 内嵌配置页 + /api/config(PUT 热重载) + /api/status
│   └── tray.rs           # Windows 系统托盘
├── webui/
│   ├── index.html        # 纯原生 JS 配置页（样式为 Tailwind 编译产物，注入 <style id="tw">）
│   ├── build_ui.py       # UI 一键构建：编译 Tailwind → 幂等注入 index.html
│   └── tailwind/         # Tailwind v4 构建环境（in.css/out.css/package.json）
└── test/
    ├── e2e.py            # 端到端验收（25 断言）
    ├── mock_upstream.py  # mock Anthropic 上游
    ├── config.mock.json  # mock 配置（端口 8788）
    └── measure.ps1       # 内存/CPU 实测
```

## 测试

```bash
cargo test          # 18 个单元测试（proxy 6 + router 7 + config 5）
python test/e2e.py  # 端到端：mock 上游 + relay 子进程，25 项断言
```

端到端覆盖：启动就绪 → 模型别名聚合 → 对话流式（model 改写 / Key 替换 / x-api-key 剥离 /
max_tokens 原样透传 / tools 透传 / SSE 事件序列 / 中文无损 / 模型名回写别名）→ count_tokens →
断线重连（relay 不崩、下个请求正常）→ 未知模型 400。
