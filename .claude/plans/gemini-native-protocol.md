# Phase 56：Gemini 原生协议翻译（protocol=gemini）

仿照 D36（OpenAI 协议翻译）模式，新增 `protocol: "gemini"`：CC 的 Anthropic Messages 请求翻译为 Gemini 原生 `generateContent` / `streamGenerateContent?alt=sse`，响应/流式翻译回 Anthropic 协议。端点默认 Google AI Studio（`https://generativelanguage.googleapis.com`，`x-goog-api-key` 认证）。

## 1. 新模块 `src/gemini.rs`（翻译层，~500 行 + 测试）

main.rs 注册 `mod gemini;`。

### 请求 `anthropic_to_gemini(body, model) -> Value`
- `system` -> `systemInstruction: {parts:[{text}]}`
- `messages` -> `contents`：role `assistant`->`model`、`user`->`user`；**合并连续同角色 content**（Gemini 要求 user/model 交替）
- 内容块映射：
  - `text` -> `{text}`
  - `image`（base64）-> `{inlineData:{mimeType, data}}`
  - `thinking`（assistant 回传）-> `{text, thought:true, thoughtSignature: signature}`（Gemini 2.5 函数调用要求思维签名往返；Anthropic thinking 块自带 `signature` 字段，正好承载 `thoughtSignature`）
  - `tool_use` -> `{functionCall:{name, args}}`
  - `tool_result` -> `{functionResponse:{name, response:{result: 文本}}}`；**name 由扫描历史 assistant tool_use 块建立 `tool_use_id -> name` 映射**（Anthropic tool_result 只带 id）；content 数组取各 text 拼接
- `tools` -> `[{functionDeclarations:[{name, description, parameters}]}]`
- `tool_choice` -> `toolConfig.functionCallingConfig`：auto->AUTO / any->ANY / {type:tool,name}->ANY+allowedFunctionNames
- `max_tokens`/`temperature`/`top_p` -> `generationConfig.{maxOutputTokens,temperature,topP}`
- `stop_sequences` -> `generationConfig.stopSequences`
- `thinking:{budget_tokens}` -> `generationConfig.thinkingConfig:{thinkingBudget, includeThoughts:true}`
- 不拷贝 `stream`（流式由端点选择，不在 body）

### 非流式响应 `gemini_message_to_anthropic(v, alias) -> Value`
- `candidates[0].content.parts`：`{text}` -> text 块；`{text, thought:true}` -> thinking 块（带 signature）；`{functionCall}` -> tool_use 块（id 生成 `toolu_gemini_N`）
- `finishReason`：STOP->end_turn / MAX_TOKENS->max_tokens / SAFETY 等->refusal
- `usageMetadata`：promptTokenCount->input_tokens；candidatesTokenCount+thoughtsTokenCount->output_tokens

### 流式 `GeminiStreamTranslator`（接口同 `OpenAiStreamTranslator`：new/push/finish）
- `?alt=sse` 产出 `data: {GenerateContentResponse}` 行，**无 [DONE]**，流自然结束 -> `finish()` 收尾
- text part -> text_delta（惰性开 text 块）；thought part -> thinking 块；functionCall part -> tool_use 块（args 一次性 `input_json_delta`）
- `usageMetadata`：message_start 用 promptTokenCount（缺失用 `input_fallback` 启发式，同 Phase 41/46）；收尾 accumulate_usage（复用 `UsageSink`/`accumulate_usage`）
- finishReason -> stop_reason 映射同上

## 2. `src/proxy.rs`（分派，仿 is_openai）
- `is_gemini = provider.protocol == "gemini"`
- URL：`{base}/v1beta/models/{real_model}:generateContent`（非流式）/ `:streamGenerateContent?alt=sse`（body `stream=true`）
- 认证：`auth == "x-goog-api-key"` -> `x-goog-api-key: {key}` 请求头（新增认证方式）
- 剥 `anthropic-version`/`anthropic-beta` 头（同 is_openai 分支）
- `count_tokens` -> 直接启发式回退（同 is_openai）
- 非 2xx：Gemini 错误体 `{"error":{code,message,status}}` -> 复用 `openai_error`（扩展：`error.type` 取不到时回退 `error.status`），产出 Anthropic `type:error`
- SSE：新增 `gemini_sse_stream` 包装（同 `openai_sse_stream` 的 unfold 模式）
- 非流式：`gemini_message_to_anthropic` + 用量累计

## 3. `webui/index.html`（+ build_ui.py 重编译）
- `PRESETS` 加 `{id:'gemini', name:'Gemini', base:'https://generativelanguage.googleapis.com', auth:'x-goog-api-key', protocol:'gemini'}`
- 预设 change 处理器：`$('m-protocol').value = pr.protocol || 'anthropic'`（当前硬编码 anthropic）
- `m-auth` 加 `<option value="x-goog-api-key">x-goog-api-key</option>`
- `m-protocol` 加 `<option value="gemini">Gemini（自动翻译，原生 generateContent）</option>`
- `presetOfProvider`：base_url 含 `generativelanguage.googleapis.com` -> gemini
- Base URL 占位提示补 Gemini 示例

## 4. 测试
- `src/gemini.rs` 单元测试（~8 个）：请求翻译（system/tools/tool_use->tool_result 映射/合并同角色/stop_sequences/thinkingConfig）、非流式响应（含 thought/functionCall/usage）、流式翻译（text+tool+usage）、错误翻译
- `test/e2e.py`：mock 上游加 `:generateContent`/`:streamGenerateContent` 端点 + gemini 服务商，断言翻译后 body/`x-goog-api-key` 头/剥 anthropic-version/SSE 事件序列/用量提取/count_tokens 启发式
- `cargo test` 全绿后 `python webui/build_ui.py` -> `cargo build --release`

## 5. 文档
- CLAUDE.md：模块表加 `src/gemini.rs`、关键设计决策 D56、`protocol` 配置说明补 gemini、测试数量更新
- DEVELOPMENT.md 扩展点补一笔

## 用户配置示例（完成后）
```json
{
  "id": "gemini", "name": "Gemini",
  "base_url": "https://generativelanguage.googleapis.com",
  "api_key": "AIza...", "auth": "x-goog-api-key", "protocol": "gemini",
  "models": [{ "alias": "pro", "real": "gemini-2.5-pro", "context": 1048576 }]
}
```
CC 里 `/model gemini-pro` 即用。
