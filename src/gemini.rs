//! Phase 56：Gemini 原生协议翻译--让 Claude Code（Anthropic Messages）能用**仅提供
//! Gemini generateContent 协议**的服务商（Google AI Studio 官方等）。
//!
//! 三块（同 D36 OpenAI 翻译模式）：请求体翻译（Anthropic -> Gemini
//! GenerateContentRequest）、非流式响应翻译（GenerateContentResponse -> Anthropic
//! message）、流式 SSE 翻译（`streamGenerateContent?alt=sse` 的 `data:{json}` ->
//! Anthropic 事件序列）。用量从转发流提取（Gemini `usageMetadata`），与纯透传路径
//! 同一套 UsageStats。
//!
//! 关键差异（相对 OpenAI 翻译器）：
//! - 上游 URL 含模型名（`/v1beta/models/{model}:generateContent`），由 proxy 拼；
//!   流式走独立端点 `:streamGenerateContent?alt=sse`，body 无 `stream` 字段。
//! - SSE **无 `[DONE]` 哨兵**，流自然结束 -> `finish()` 收尾。
//! - 角色只有 `user`/`model`，**要求交替** -> 翻译时合并连续同角色消息。
//! - 工具调用：`functionCall:{name,args}`（args 是对象非字符串）；工具结果
//!   `functionResponse:{name,response}`--Anthropic `tool_result` 只带 `tool_use_id`，
//!   需扫描历史 assistant `tool_use` 块建立 id->name 映射。
//! - 思维链往返：Gemini `thought:true` part 携带 `thoughtSignature`（函数调用时必须
//!   回传），映射到 Anthropic thinking 块的 `signature` 字段承载。

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::config::UsageStats;
use crate::proxy::UsageSink;

// ---------------- 请求体翻译：Anthropic Messages -> Gemini GenerateContentRequest ----------------

/// 把 Anthropic Messages 请求体翻译为 Gemini GenerateContentRequest。
/// `stream` 字段不进 body（流式由端点选择，proxy 按 body.stream 拼 URL）。
pub fn anthropic_to_gemini(body: &Value, model: &str) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("model".into(), Value::String(model.into()));

    // system -> systemInstruction（字符串或 [{type:text,text}] -> parts[{text}]）
    if let Some(sys) = body.get("system") {
        let text = system_to_text(sys);
        if !text.is_empty() {
            out.insert("systemInstruction".into(), json!({ "parts": [{ "text": text }] }));
        }
    }

    // messages -> contents（role assistant->model；合并连续同角色）
    let mut contents: Vec<Value> = Vec::new();
    // tool_use_id -> 函数名映射（tool_result 只有 id，Gemini functionResponse 要 name）
    let mut tool_names: std::collections::HashMap<String, String> = Default::default();
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            // 先扫本条消息的 tool_use 块登记 id->name（assistant 消息）
            if m.get("role").and_then(|x| x.as_str()) == Some("assistant") {
                if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                    for blk in blocks {
                        if blk.get("type").and_then(|x| x.as_str()) == Some("tool_use") {
                            if let (Some(id), Some(name)) = (
                                blk.get("id").and_then(|x| x.as_str()),
                                blk.get("name").and_then(|x| x.as_str()),
                            ) {
                                tool_names.insert(id.to_string(), name.to_string());
                            }
                        }
                    }
                }
            }
            convert_content(m, &tool_names, &mut contents);
        }
    }
    out.insert("contents".into(), Value::Array(contents));

    // tools -> functionDeclarations；tool_choice -> toolConfig
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let decls: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                    "description": t.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                    "parameters": t.get("input_schema").cloned()
                        .unwrap_or(json!({"type":"object","properties":{}})),
                })
            })
            .collect();
        out.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
        if let Some(tc) = body.get("tool_choice") {
            let mut mode = json!("AUTO");
            if let Some(name) = tc.get("name").and_then(|x| x.as_str()) {
                if tc.get("type").and_then(|x| x.as_str()) == Some("tool") {
                    // {type:tool,name} -> ANY + 只允许该函数
                    out.insert(
                        "toolConfig".into(),
                        json!({ "functionCallingConfig": {
                            "mode": "ANY", "allowedFunctionNames": [name]
                        }}),
                    );
                    mode = Value::Null; // 已写入，跳过下方统一写入
                }
            } else if tc.get("type").and_then(|x| x.as_str()) == Some("any") {
                mode = json!("ANY");
            }
            if !mode.is_null() {
                out.insert("toolConfig".into(), json!({ "functionCallingConfig": { "mode": mode } }));
            }
        }
    }

    // 标量参数 -> generationConfig
    let mut gen = serde_json::Map::new();
    if let Some(v) = body.get("max_tokens") {
        gen.insert("maxOutputTokens".into(), v.clone());
    }
    if let Some(v) = body.get("temperature") {
        gen.insert("temperature".into(), v.clone());
    }
    if let Some(v) = body.get("top_p") {
        gen.insert("topP".into(), v.clone());
    }
    if let Some(stops) = body.get("stop_sequences").and_then(|s| s.as_array()) {
        gen.insert("stopSequences".into(), Value::Array(stops.clone()));
    }
    // thinking:{budget_tokens} -> thinkingConfig（includeThoughts 让上游回思维链，
    // thoughtSignature 随 thought part 返回，函数调用场景必须回传）
    if let Some(budget) = body
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64())
    {
        gen.insert(
            "thinkingConfig".into(),
            json!({ "thinkingBudget": budget, "includeThoughts": true }),
        );
    }
    if !gen.is_empty() {
        out.insert("generationConfig".into(), Value::Object(gen));
    }
    Value::Object(out)
}

/// system 字段（字符串或 `[{type:text,text}]`）-> 纯文本。
fn system_to_text(sys: &Value) -> String {
    match sys {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 单条 Anthropic 消息 -> 追加到 Gemini contents（role 映射 + 连续同角色合并 +
/// 内容块转 parts）。tool_result 需要 `tool_names`（id->name）解析函数名。
fn convert_content(
    m: &Value,
    tool_names: &std::collections::HashMap<String, String>,
    contents: &mut Vec<Value>,
) {
    let role = match m.get("role").and_then(|x| x.as_str()) {
        Some("assistant") => "model",
        _ => "user",
    };
    let mut parts: Vec<Value> = Vec::new();
    if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
        for blk in arr {
            match blk.get("type").and_then(|x| x.as_str()) {
                Some("tool_use") => {
                    if let Some(fc) = blk.get("name").and_then(|n| n.as_str()) {
                        parts.push(json!({
                            "functionCall": {
                                "name": fc,
                                "args": blk.get("input").cloned()
                                    .unwrap_or_else(|| Value::Object(Default::default())),
                            }
                        }));
                    }
                }
                Some("tool_result") => {
                    let id = blk.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or("");
                    let name = tool_names.get(id).map(String::as_str).unwrap_or("tool");
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": { "result": tool_result_text(blk) },
                        }
                    }));
                }
                Some("image") => {
                    if let Some(src) = blk.get("source") {
                        if let (Some(mime), Some(data)) = (
                            src.get("media_type").and_then(|x| x.as_str()),
                            src.get("data").and_then(|d| d.as_str()),
                        ) {
                            parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                        }
                    }
                }
                Some("thinking") => {
                    // Phase 56：thinking 块回传 -> thought part（思维签名往返，
                    // Gemini 函数调用要求 thoughtSignature 原样回传）
                    if let Some(t) = blk.get("thinking").and_then(|x| x.as_str()) {
                        let sig = blk
                            .get("signature")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let mut p = json!({ "text": t, "thought": true });
                        if !sig.is_empty() {
                            p["thoughtSignature"] = Value::String(sig.into());
                        }
                        parts.push(p);
                    }
                }
                _ => {
                    if let Some(t) = blk.get("text").and_then(|x| x.as_str()) {
                        parts.push(json!({ "text": t }));
                    }
                }
            }
        }
    } else if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
        parts.push(json!({ "text": s }));
    }
    if parts.is_empty() {
        return;
    }
    // Gemini 要求 user/model 交替：连续同角色 -> parts 并入前一条
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(|x| x.as_str()) == Some(role) {
            if let Some(arr) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                arr.append(&mut parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

/// tool_result 的 content（字符串或数组）-> 文本。
fn tool_result_text(blk: &Value) -> String {
    match blk.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------- 非流式响应翻译：GenerateContentResponse -> Anthropic message ----------------

/// 把 Gemini 非流式响应翻译为 Anthropic message 对象。`alias` 是回写给 CC 的模型名。
pub fn gemini_message_to_anthropic(v: &Value, alias: &str) -> Value {
    let candidate = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);

    let mut blocks: Vec<Value> = Vec::new();
    let mut tool_seq: u64 = 0;
    if let Some(parts) = candidate.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
        for p in parts {
            if let Some(fc) = p.get("functionCall") {
                tool_seq += 1;
                blocks.push(json!({
                    "type": "tool_use",
                    "id": format!("toolu_gemini_{tool_seq}"),
                    "name": fc.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                    "input": fc.get("args").cloned()
                        .unwrap_or_else(|| Value::Object(Default::default())),
                }));
                continue;
            }
            let Some(text) = p.get("text").and_then(|x| x.as_str()) else { continue };
            if p.get("thought").and_then(|x| x.as_bool()).unwrap_or(false) {
                // thought part -> thinking 块（signature 承载 thoughtSignature，回传时还原）
                let sig = p.get("thoughtSignature").and_then(|x| x.as_str()).unwrap_or("");
                blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig,
                }));
            } else if !text.is_empty() {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }

    let stop_reason = map_finish_reason(candidate.get("finishReason").and_then(|x| x.as_str()));
    let (input, output) = extract_usage(v.get("usageMetadata"));
    json!({
        "id": v.get("responseId").and_then(|x| x.as_str()).unwrap_or("msg_gemini").to_string(),
        "type": "message",
        "role": "assistant",
        "model": alias,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input, "output_tokens": output },
    })
}

/// Gemini finishReason -> Anthropic stop_reason。
fn map_finish_reason(f: Option<&str>) -> &'static str {
    match f {
        Some("MAX_TOKENS") => "max_tokens",
        Some("STOP") => "end_turn",
        Some("SAFETY") | Some("PROHIBITED_CONTENT") | Some("RECITATION") => "refusal",
        _ => "end_turn",
    }
}

/// usageMetadata -> (input, output)。Gemini 把思维 tokens 单独计（thoughtsTokenCount），
/// Anthropic 把 thinking 计入输出 -> output = candidates + thoughts。
fn extract_usage(u: Option<&Value>) -> (u64, u64) {
    let u = u.unwrap_or(&Value::Null);
    let input = u.get("promptTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
    let cand = u.get("candidatesTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
    let thoughts = u.get("thoughtsTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
    (input, cand + thoughts)
}

// ---------------- 流式响应翻译：Gemini SSE chunk -> Anthropic 事件序列 ----------------

/// 消费 Gemini 流式 SSE（`streamGenerateContent?alt=sse`，`data:{json}` 行、无 [DONE]），
/// 逐行产出 Anthropic SSE 事件序列，并在收尾把真实用量提取到共享 UsageStats。
pub struct GeminiStreamTranslator {
    buf: Vec<u8>,
    alias: String,
    // 用量收集
    usage: Arc<Mutex<UsageStats>>,
    provider: String,
    model: String,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    /// 输入 tokens 启发式兜底（同 Phase 41：上游 chunk 缺 promptTokenCount 时用）。
    input_fallback: u64,
    // 流状态
    started: bool,
    next_index: usize,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    /// functionCall 直接整体翻译成 tool_use 块（Gemini 不分片流式发 args）。
    tools_open: Vec<(usize, bool)>, // (block_index, open)
    tool_seq: u64,
    finish: Option<String>,
    in_tokens: u64,
    out_tokens: u64,
    out_chars: u64,
    emitted_stop: bool,
}

impl GeminiStreamTranslator {
    pub fn new(alias: String, sink: UsageSink) -> Self {
        GeminiStreamTranslator {
            buf: Vec::new(),
            alias,
            usage: sink.usage,
            provider: sink.provider,
            model: sink.model,
            dirty: sink.dirty,
            input_fallback: sink.input_fallback,
            started: false,
            next_index: 0,
            text_index: None,
            thinking_index: None,
            tools_open: Vec::new(),
            tool_seq: 0,
            finish: None,
            in_tokens: 0,
            out_tokens: 0,
            out_chars: 0,
            emitted_stop: false,
        }
    }

    /// Phase 61：流是否已规范终止（收到过 finishReason 块；Gemini 无 [DONE] 标记）。
    /// 供流包装器判别「EOF = 正常结束」还是「EOF = 上游被掐断」。
    pub(crate) fn stream_terminated(&self) -> bool {
        self.finish.is_some()
    }

    /// Phase 61：诊断日志用别名。
    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }

    /// 追加一块数据，产出完整行（含换行）的 Anthropic SSE 结果。
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.handle_line(&line, out);
        }
    }

    /// 流结束（Gemini 无 [DONE]，连接关闭即结束）：冲刷 + 收尾。
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.handle_line(&line, out);
        }
        self.finalize(out);
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return; // 忽略空行等非 data 行
        };
        let s = String::from_utf8_lossy(payload).trim().to_string();
        if s.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            return;
        };
        // 每个 chunk 都可能带 usageMetadata（末尾 chunk 必带）--随时更新
        let (in_t, out_t) = extract_usage(v.get("usageMetadata"));
        if in_t > 0 {
            self.in_tokens = in_t;
        }
        if out_t > 0 {
            self.out_tokens = out_t;
        }
        let candidate = v.get("candidates").and_then(|c| c.as_array()).and_then(|c| c.first());
        let Some(candidate) = candidate else { return };
        if !self.started {
            self.emit_start(out);
        }
        if let Some(parts) = candidate.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
            for p in parts {
                if let Some(fc) = p.get("functionCall") {
                    self.open_tool(out, fc);
                    continue;
                }
                let Some(text) = p.get("text").and_then(|x| x.as_str()) else { continue };
                if p.get("thought").and_then(|x| x.as_bool()).unwrap_or(false) {
                    self.emit_thinking_delta(out, text);
                } else {
                    self.emit_text_delta(out, text);
                }
            }
        }
        if let Some(fr) = candidate.get("finishReason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                self.finish = Some(fr.to_string());
            }
        }
    }

    fn emit_start(&mut self, out: &mut Vec<u8>) {
        self.started = true;
        // message_start 的 input_tokens：CC 用它跟上下文进度条（同 Phase 46）；
        // Gemini 首 chunk 通常带 promptTokenCount，缺则用请求体启发式兜底。
        let in_tokens = if self.in_tokens > 0 { self.in_tokens } else { self.input_fallback };
        emit(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_gemini_1",
                    "type": "message",
                    "role": "assistant",
                    "model": &self.alias,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": in_tokens, "output_tokens": 0 },
                },
            }),
        );
    }

    /// thought 增量 -> thinking 块（thinking 必须排在 text/tool 块之前，同 Phase 47）。
    fn emit_thinking_delta(&mut self, out: &mut Vec<u8>, text: &str) {
        if self.text_index.is_some() {
            return; // 异常顺序（text 已开）：丢弃，防索引错乱
        }
        if self.thinking_index.is_none() {
            let idx = self.next_index;
            self.next_index += 1;
            self.thinking_index = Some(idx);
            emit(
                out,
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "thinking", "thinking": "" },
                }),
            );
        }
        if !text.is_empty() {
            self.out_chars += text.chars().count() as u64;
            emit(
                out,
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": self.thinking_index.unwrap_or(0),
                    "delta": { "type": "thinking_delta", "thinking": text },
                }),
            );
        }
    }

    fn emit_text_delta(&mut self, out: &mut Vec<u8>, text: &str) {
        if self.text_index.is_none() {
            self.open_text(out);
        }
        if !text.is_empty() {
            self.out_chars += text.chars().count() as u64;
            emit(
                out,
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": self.text_index.unwrap_or(0),
                    "delta": { "type": "text_delta", "text": text },
                }),
            );
        }
    }

    /// functionCall -> tool_use 块（args 整体一次 input_json_delta）。
    fn open_tool(&mut self, out: &mut Vec<u8>, fc: &Value) {
        self.close_thinking(out);
        self.close_text(out);
        self.tool_seq += 1;
        let idx = self.next_index;
        self.next_index += 1;
        let id = format!("toolu_gemini_{seq}", seq = self.tool_seq);
        let name = fc.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let args = fc
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()))
            .to_string();
        emit(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
            }),
        );
        if !args.is_empty() {
            self.out_chars += args.chars().count() as u64;
            emit(
                out,
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": { "type": "input_json_delta", "partial_json": args },
                }),
            );
        }
        self.tools_open.push((idx, true));
    }

    fn open_text(&mut self, out: &mut Vec<u8>) {
        // thinking 块必须先 stop，text 块才能 start（Anthropic 块序）
        self.close_thinking(out);
        self.close_tools(out);
        let idx = self.next_index;
        self.next_index += 1;
        self.text_index = Some(idx);
        emit(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": { "type": "text", "text": "" },
            }),
        );
    }

    fn close_thinking(&mut self, out: &mut Vec<u8>) {
        if let Some(idx) = self.thinking_index.take() {
            emit(out, "content_block_stop", json!({ "type": "content_block_stop", "index": idx }));
        }
    }

    fn close_text(&mut self, out: &mut Vec<u8>) {
        if let Some(idx) = self.text_index.take() {
            emit(out, "content_block_stop", json!({ "type": "content_block_stop", "index": idx }));
        }
    }

    fn close_tools(&mut self, out: &mut Vec<u8>) {
        for t in self.tools_open.iter_mut() {
            if t.1 {
                emit(out, "content_block_stop", json!({ "type": "content_block_stop", "index": t.0 }));
                t.1 = false;
            }
        }
    }

    fn finalize(&mut self, out: &mut Vec<u8>) {
        if self.emitted_stop {
            return;
        }
        self.close_thinking(out);
        self.close_text(out);
        self.close_tools(out);
        if !self.started {
            self.emit_start(out);
        }
        let out_tokens = if self.out_tokens > 0 {
            self.out_tokens
        } else {
            ((self.out_chars as f64) / 4.0).ceil() as u64
        };
        let in_tokens = if self.in_tokens > 0 { self.in_tokens } else { self.input_fallback };
        crate::proxy::accumulate_usage(
            &self.usage,
            &self.dirty,
            &self.provider,
            &self.model,
            in_tokens,
            out_tokens,
        );
        let stop_reason = map_finish_reason(self.finish.as_deref());
        emit(
            out,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": { "output_tokens": out_tokens },
            }),
        );
        emit(out, "message_stop", json!({ "type": "message_stop" }));
        self.emitted_stop = true;
    }
}

/// 写一条 Anthropic SSE 事件（`event: <e>\ndata: <json>\n\n`）。
fn emit(out: &mut Vec<u8>, event: &str, data: Value) {
    out.extend_from_slice(format!("event: {event}\n").as_bytes());
    let s = data.to_string();
    out.extend_from_slice(format!("data: {s}\n\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::UsageSink;
    use std::sync::atomic::AtomicBool;

    fn sink() -> (UsageSink, Arc<Mutex<UsageStats>>) {
        let usage = Arc::new(Mutex::new(UsageStats::default()));
        let s = UsageSink {
            usage: usage.clone(),
            provider: "gemini".into(),
            model: "gemini-2.5-pro".into(),
            dirty: Arc::new(AtomicBool::new(false)),
            input_fallback: 0,
        };
        (s, usage)
    }

    #[test]
    fn anthropic_to_gemini_translates_body() {
        // 请求翻译：system->systemInstruction、role assistant->model、tool_use/tool_result
        // ->functionCall/functionResponse（id->name 映射）、tools->functionDeclarations、
        // 标量参数进 generationConfig、连续同角色合并
        let body = serde_json::json!({
            "model": "gemini-2.5-pro",
            "system": [{"type":"text","text":"你是助手"}],
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"user","content":[{"type":"text","text":"再来"}]}, // 连续 user -> 合并
                {"role":"assistant","content":[{"type":"text","text":"查一下"},
                    {"type":"tool_use","id":"t1","name":"get_weather","input":{"city":"北京"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"晴"}]}
            ],
            "tools": [{"name":"get_weather","description":"天气","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
            "max_tokens": 32000,
            "temperature": 0.7,
            "stream": true,
            "stop_sequences": ["\n\n"]
        });
        let out = anthropic_to_gemini(&body, "gemini-2.5-pro");
        assert_eq!(out["model"], "gemini-2.5-pro");
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "你是助手");
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "连续 user 合并: {out}");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
        // assistant -> model + functionCall
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "查一下");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "get_weather");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["args"]["city"], "北京");
        // tool_result -> functionResponse（id t1 -> name get_weather）
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "get_weather");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["result"], "晴");
        // tools + generationConfig；stream 不进 body
        assert_eq!(out["tools"][0]["functionDeclarations"][0]["name"], "get_weather");
        assert_eq!(out["generationConfig"]["maxOutputTokens"], 32000);
        assert_eq!(out["generationConfig"]["temperature"], 0.7);
        assert_eq!(out["generationConfig"]["stopSequences"], serde_json::json!(["\n\n"]));
        assert!(out.get("stream").is_none(), "stream 不进 body: {out}");
    }

    #[test]
    fn request_image_and_thinking_map() {
        // image -> inlineData；thinking 块回传 -> thought part（含 signature 往返）
        let body = serde_json::json!({
            "model": "gemini-2.5-pro",
            "messages": [
                {"role":"user","content":[
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}},
                    {"type":"text","text":"这是什么"}
                ]},
                {"role":"assistant","content":[{"type":"thinking","thinking":"想想","signature":"SIG123"}]},
                {"role":"user","content":"继续"}
            ]
        });
        let out = anthropic_to_gemini(&body, "gemini-2.5-pro");
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents[0]["parts"][0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(contents[0]["parts"][0]["inlineData"]["data"], "QUJD");
        assert_eq!(contents[1]["parts"][0]["thought"], true);
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "SIG123");
    }

    #[test]
    fn request_tool_choice_and_thinking_config() {
        // tool_choice {type:tool,name} -> ANY + allowedFunctionNames；
        // thinking budget -> thinkingConfig
        let body = serde_json::json!({
            "model": "gemini-2.5-pro",
            "messages": [{"role":"user","content":"hi"}],
            "tools": [{"name":"f","description":"","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool","name":"f"},
            "thinking": {"type":"enabled","budget_tokens": 8000}
        });
        let out = anthropic_to_gemini(&body, "gemini-2.5-pro");
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"], serde_json::json!(["f"]));
        assert_eq!(out["generationConfig"]["thinkingConfig"]["thinkingBudget"], 8000);
        assert_eq!(out["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
    }

    #[test]
    fn gemini_message_to_anthropic_converts() {
        // 非流式：thought/text/functionCall parts -> thinking/text/tool_use 块；
        // finishReason 映射；usageMetadata（含 thoughtsTokenCount 并入输出）
        let body = serde_json::json!({
            "responseId": "resp_1",
            "candidates": [{
                "content": { "role": "model", "parts": [
                    {"text": "想想", "thought": true, "thoughtSignature": "SIG1"},
                    {"text": "结果"},
                    {"functionCall": {"name": "get_weather", "args": {"city": "北京"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 5, "thoughtsTokenCount": 3, "totalTokenCount": 19}
        });
        let out = gemini_message_to_anthropic(&body, "gemini-pro");
        assert_eq!(out["type"], "message");
        assert_eq!(out["model"], "gemini-pro");
        assert_eq!(out["stop_reason"], "end_turn");
        let c = out["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "thinking");
        assert_eq!(c[0]["signature"], "SIG1");
        assert_eq!(c[1]["type"], "text");
        assert_eq!(c[1]["text"], "结果");
        assert_eq!(c[2]["type"], "tool_use");
        assert_eq!(c[2]["name"], "get_weather");
        assert_eq!(c[2]["input"]["city"], "北京");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 8, "candidates+thoughts");
        // MAX_TOKENS -> max_tokens
        let mut b2 = body.clone();
        b2["candidates"][0]["finishReason"] = serde_json::json!("MAX_TOKENS");
        assert_eq!(gemini_message_to_anthropic(&b2, "gemini-pro")["stop_reason"], "max_tokens");
    }

    #[test]
    fn gemini_stream_translates_text_and_tool_calls() {
        // 流式：text 增量 + functionCall + finish + usageMetadata -> Anthropic 事件序列 + 用量提取
        let (s, usage) = sink();
        let mut tr = GeminiStreamTranslator::new("gemini-pro".into(), s);
        let mut out = Vec::new();
        tr.push(r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"你"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}}
"#.as_bytes(), &mut out);
        tr.push(r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"北京"}}}]}}]}
"#.as_bytes(), &mut out);
        tr.push(r#"data: {"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":7,"totalTokenCount":17}}
"#.as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains("event: message_start"), "got: {s}");
        assert!(s.contains(r#""type":"text_delta""#) && s.contains(r#""text":"你""#));
        assert!(s.contains(r#""type":"tool_use""#) && s.contains(r#""name":"get_weather""#));
        assert!(s.contains(r#""type":"input_json_delta""#) && s.contains(r#""partial_json":"{\"city\":\"北京\"}""#));
        assert!(s.contains(r#""stop_reason":"end_turn""#));
        assert!(s.contains("event: message_stop"));
        // 用量提取：末尾 usageMetadata 的 prompt/candidates tokens
        let u = usage.lock().unwrap();
        let pu = u.providers.get("gemini").unwrap();
        assert_eq!(pu.input_tokens, 10);
        assert_eq!(pu.output_tokens, 7);
    }

    #[test]
    fn gemini_stream_thinking_before_text() {
        // thought part -> thinking 块，且必须先于 text_delta（块序）
        let (s, _) = sink();
        let mut tr = GeminiStreamTranslator::new("gemini-pro".into(), s);
        let mut out = Vec::new();
        tr.push(r#"data: {"candidates":[{"content":{"parts":[{"text":"让我","thought":true}]}}]}
"#.as_bytes(), &mut out);
        tr.push(r#"data: {"candidates":[{"content":{"parts":[{"text":"好"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2}}
"#.as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(r#""type":"thinking""#), "got: {s}");
        let think_pos = s.find("thinking_delta").unwrap();
        let text_pos = s.find("text_delta").unwrap();
        assert!(think_pos < text_pos, "块序错误: {s}");
    }

    #[test]
    fn gemini_stream_empty_still_ends() {
        // 空流 -> 仍有 message_start/message_stop，不 panic
        let (s, _) = sink();
        let mut tr = GeminiStreamTranslator::new("gemini-pro".into(), s);
        let mut out = Vec::new();
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains("event: message_start"), "got: {s}");
        assert!(s.contains("event: message_stop"));
    }

    #[test]
    fn gemini_stream_input_fallback() {
        // 上游缺 promptTokenCount -> 用请求体启发式兜底（Phase 41 同规则）
        let (mut s, usage) = sink();
        s.input_fallback = 50;
        let mut tr = GeminiStreamTranslator::new("gemini-pro".into(), s);
        let mut out = Vec::new();
        tr.push(r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}]}
"#.as_bytes(), &mut out);
        tr.finish(&mut out);
        assert!(String::from_utf8_lossy(&out).contains(r#""input_tokens":50"#));
        let u = usage.lock().unwrap();
        assert_eq!(u.providers.get("gemini").unwrap().input_tokens, 50);
    }
}
