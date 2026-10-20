---
description: 查看当前可用的模型列表（ClaudeRelay 中继配置的所有模型订阅）
allowed-tools: "Bash"
---

从本地 ClaudeRelay 中继获取当前配置的模型列表，整理后展示给用户。

步骤：

1. 用 Bash 执行 `curl -s http://127.0.0.1:8787/api/config` 获取中继配置。
   - 若 8787 端口连接失败（端口可能被改过），按顺序探测实际端口：
     a) `echo $ANTHROPIC_BASE_URL`（start-claude.cmd / 环境变量设置的 base URL，形如 `http://127.0.0.1:XXXX`）；
     b) `cat ~/.claude/settings.json`（若设置了 `CLAUDE_CONFIG_DIR` 则用该目录下的 settings.json）的 `env.ANTHROPIC_BASE_URL`。
     用探测到的端口重试。

2. 解析响应 JSON：
   - `server.default_model` = 当前使用模型完整别名（可能为空字符串）。
   - `providers[]`：每个服务商有 `id`（前缀）、`name`（显示名）、`models[]`（每项 `alias`/`real`/`context`）。

3. 用中文整理输出，格式如下：

   ```
   📋 当前模型列表（ClaudeRelay）

   当前使用模型：{default_model}（未设置则写「未设置，回退第一个别名」）

   {服务商名称}（{id}）
     · {id}-{alias}   → {real}   [{context}]
   ```

   - context 是上下文窗口 tokens，用简写展示：1000000 → `1m`，200000 → `200k`，8000 → `8k` 等（`[200k]`、`[1m]` 样式）。
   - 若 `default_model` 的完整别名出现在某个服务商行首，可加 `（当前）` 标注。
   - 末尾附一句提示：切换模型用 `/model {完整别名}`；或直接在 CC 里输 `/model` 从列表选择。

4. 注意：`api_key` / `console_cookie` 字段是掩码值（`****…`），**不要展示、不要复制到任何输出或文件**；只展示模型列表本身。

5. 若 `providers` 为空数组，提示「尚未配置模型订阅，可打开配置页 http://127.0.0.1:8787 添加」。
