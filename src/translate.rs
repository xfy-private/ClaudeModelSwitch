//! Phase 36：OpenAI 协议翻译——让 Claude Code（Anthropic Messages）能用**仅提供 OpenAI
//! Chat Completions 协议**的服务商（OpenRouter / Qwen 百炼兼容模式等）。
//!
//! 三块：请求体翻译（Anthropic → OpenAI）、非流式响应翻译（OpenAI → Anthropic）、
//! 流式 SSE 翻译（OpenAI chunk → Anthropic 事件序列）。用量从中继自己的转发流提取
//!（OpenAI usage chunk 的 prompt/completion_tokens），与纯透传路径同一套 UsageStats。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::config::UsageStats;
use crate::proxy::UsageSink;

// ---------------- 请求体翻译：Anthropic Messages → OpenAI Chat Completions ----------------

/// 把 Anthropic Messages 请求体翻译为 OpenAI Chat Completions 请求体。
/// 只映射语义等价字段；`thinking`/`metadata` 等 OpenAI 不支持的直接丢弃。
pub fn anthropic_to_openai(body: &Value, model: &str) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("model".into(), Value::String(model.into()));

    let mut msgs: Vec<Value> = Vec::new();
    // system（字符串或数组）→ 首位 system 消息
    if let Some(sys) = body.get("system") {
        let text = system_to_text(sys);
        if !text.is_empty() {
            msgs.push(json!({ "role": "system", "content": text }));
        }
    }
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            convert_message(m, &mut msgs);
        }
    }
    out.insert("messages".into(), Value::Array(msgs));

    // tools → function tools；tool_choice 映射
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let oa: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                        "description": t.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned()
                            .unwrap_or(json!({"type":"object","properties":{}})),
                    }
                })
            })
            .collect();
        out.insert("tools".into(), Value::Array(oa));
        if let Some(tc) = body.get("tool_choice") {
            match tc.get("type").and_then(|x| x.as_str()) {
                Some("any") => {
                    out.insert("tool_choice".into(), json!("required"));
                }
                Some("tool") => {
                    let name = tc.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    out.insert(
                        "tool_choice".into(),
                        json!({ "type": "function", "function": { "name": name } }),
                    );
                }
                _ => {
                    out.insert("tool_choice".into(), json!("auto"));
                }
            }
        }
    }

    // 标量参数
    for k in ["max_tokens", "temperature", "top_p", "stream"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    // stop_sequences → stop
    if let Some(stops) = body.get("stop_sequences").and_then(|s| s.as_array()) {
        out.insert("stop".into(), Value::Array(stops.clone()));
    }
    // 流式请求带上 include_usage，让上游在末尾返回真实 token 用量
    if body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false) {
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Value::Object(out)
}

/// system 字段（字符串或 `[{type:text,text}]`）→ 纯文本。
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

/// 单条 Anthropic 消息 → 追加到 OpenAI 消息列表（tool_use/tool_result 会拆成多条）。
fn convert_message(m: &Value, msgs: &mut Vec<Value>) {
    let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("user");
    match role {
        "user" => {
            let content = m.get("content");
            if let Some(arr) = content.and_then(|c| c.as_array()) {
                let mut text_parts: Vec<Value> = Vec::new();
                for blk in arr {
                    match blk.get("type").and_then(|x| x.as_str()) {
                        Some("tool_result") => {
                            if !text_parts.is_empty() {
                                msgs.push(json!({ "role": "user", "content": text_parts }));
                                text_parts = Vec::new();
                            }
                            let id = blk
                                .get("tool_use_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            msgs.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": tool_result_text(blk),
                            }));
                        }
                        Some("image") => {
                            if let Some(url) = image_data_url(blk) {
                                text_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": { "url": url }
                                }));
                            }
                        }
                        _ => {
                            if let Some(t) = blk.get("text").and_then(|x| x.as_str()) {
                                text_parts.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                }
                if !text_parts.is_empty() {
                    msgs.push(json!({ "role": "user", "content": text_parts }));
                }
            } else {
                msgs.push(json!({
                    "role": "user",
                    "content": content.cloned().unwrap_or(Value::String(String::new())),
                }));
            }
        }
        "assistant" => {
            let mut text: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut reasoning: Vec<String> = Vec::new();
            if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
                for blk in arr {
                    match blk.get("type").and_then(|x| x.as_str()) {
                        Some("tool_use") => {
                            let id = blk.get("id").and_then(|x| x.as_str()).unwrap_or("");
                            let name = blk.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let args = blk
                                .get("input")
                                .cloned()
                                .unwrap_or_else(|| Value::Object(Default::default()));
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": args.to_string() },
                            }));
                        }
                        // Phase 47：OpenCode Go / DeepSeek 系推理模型要求带 tool_calls 的 assistant
                        // 消息把 reasoning_content 原样回传，否则 400。收集 thinking 文本回填。
                        Some("thinking") => {
                            if let Some(t) = blk.get("thinking").and_then(|x| x.as_str()) {
                                reasoning.push(t.to_string());
                            }
                        }
                        _ => {
                            if let Some(t) = blk.get("text").and_then(|x| x.as_str()) {
                                text.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                }
            } else if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
                text.push(json!({ "type": "text", "text": s }));
            }
            let mut om = serde_json::Map::new();
            om.insert("role".into(), "assistant".into());
            if text.is_empty() {
                om.insert("content".into(), Value::Null);
            } else {
                om.insert("content".into(), Value::Array(text));
            }
            if !tool_calls.is_empty() {
                om.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            if !reasoning.is_empty() {
                om.insert("reasoning_content".into(), Value::String(reasoning.concat()));
            }
            msgs.push(Value::Object(om));
        }
        _ => {
            msgs.push(json!({
                "role": role,
                "content": m.get("content").cloned().unwrap_or(Value::Null),
            }));
        }
    }
}

/// tool_result 的 content（字符串或数组）→ OpenAI content 字符串。
fn tool_result_text(blk: &Value) -> Value {
    match blk.get("content") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
                .collect();
            Value::String(parts.join("\n"))
        }
        _ => Value::String(String::new()),
    }
}

/// image 块 → `data:{media_type};base64,{data}` 数据 URL。
fn image_data_url(blk: &Value) -> Option<String> {
    let src = blk.get("source")?;
    let media = src.get("media_type").and_then(|m| m.as_str())?;
    let data = src.get("data").and_then(|d| d.as_str())?;
    Some(format!("data:{media};base64,{data}"))
}

// ---------------- 非流式响应翻译：OpenAI ChatCompletion → Anthropic message ----------------

/// 把 OpenAI ChatCompletion（非流式）翻译为 Anthropic message 对象。
/// `alias` 是回写给 CC 的模型名（`{id}-{alias}`）。
pub fn openai_message_to_anthropic(v: &Value, alias: &str) -> Value {
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);
    let msg = choice.get("message").unwrap_or(&Value::Null);

    let mut blocks: Vec<Value> = Vec::new();
    // Phase 47：推理模型（OpenCode Go / DeepSeek 系）把 reasoning_content 翻成 thinking 块，
    // 让 CC 存进上下文、下一轮回传，上游才不报「reasoning_content must be passed back」。
    if let Some(r) = msg.get("reasoning_content").and_then(|x| x.as_str()) {
        if !r.is_empty() {
            blocks.push(json!({ "type": "thinking", "thinking": r, "signature": "" }));
        }
    }
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        if !s.is_empty() {
            blocks.push(json!({ "type": "text", "text": s }));
        }
    } else if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            if p.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    blocks.push(json!({ "type": "text", "text": t }));
                }
            }
        }
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|x| x.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| Value::Object(Default::default()));
            blocks.push(json!({ "type": "tool_use", "id": id, "name": name, "input": args }));
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }

    let finish = choice
        .get("finish_reason")
        .and_then(|x| x.as_str())
        .unwrap_or("stop");
    let stop_reason = match finish {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        "stop_sequence" => "stop_sequence",
        _ => "end_turn",
    };
    let usage = v.get("usage").unwrap_or(&Value::Null);
    let input = usage.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let output = usage.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0);

    json!({
        "id": v.get("id").and_then(|x| x.as_str()).unwrap_or("msg_openai").to_string(),
        "type": "message",
        "role": "assistant",
        "model": alias,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input, "output_tokens": output },
    })
}

// ---------------- 流式响应翻译：OpenAI SSE chunk → Anthropic 事件序列 ----------------

struct ToolState {
    oa_index: u32,
    block_index: usize,
    open: bool,
}

/// 消费 OpenAI 流式 SSE（`data: {chunk}`…`data: [DONE]`），逐行产出 Anthropic SSE
/// （message_start / content_block_start / content_block_delta / content_block_stop /
/// message_delta / message_stop），并在收尾时把真实用量提取到共享 UsageStats。
pub struct OpenAiStreamTranslator {
    buf: Vec<u8>,
    alias: String,
    // 用量收集
    usage: Arc<Mutex<UsageStats>>,
    provider: String,
    model: String,
    dirty: Arc<AtomicBool>,
    /// Phase 41：输入 tokens 启发式兜底（上游 usage chunk 缺 prompt_tokens 时用）。
    input_fallback: u64,
    // 流状态
    started: bool,
    next_index: usize,
    text_index: Option<usize>,
    tools: Vec<ToolState>,
    /// Phase 47：推理增量缓冲（OpenCode Go / DeepSeek 系 `delta.reasoning_content`）。
    reasoning: String,
    /// 当前 thinking 块的 block index（None = 未开）。首个 reasoning_content 到达时开块。
    thinking_index: Option<usize>,
    finish: Option<String>,
    in_tokens: u64,
    out_tokens: u64,
    out_chars: u64,
    emitted_stop: bool,
}

impl OpenAiStreamTranslator {
    pub fn new(alias: String, sink: UsageSink) -> Self {
        OpenAiStreamTranslator {
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
            tools: Vec::new(),
            reasoning: String::new(),
            thinking_index: None,
            finish: None,
            in_tokens: 0,
            out_tokens: 0,
            out_chars: 0,
            emitted_stop: false,
        }
    }

    /// Phase 61：流是否已规范终止（收到过 finish_reason，或已按 [DONE] finalize）。
    /// 供流包装器判别「EOF = 正常结束」还是「EOF = 上游被掐断」。
    pub(crate) fn stream_terminated(&self) -> bool {
        self.finish.is_some() || self.emitted_stop
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

    /// 流结束：冲刷不完整行 + 收尾（关闭块 / message_delta / message_stop / 落用量）。
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.handle_line(&line, out);
        }
        self.finalize(out);
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return; // 忽略 event: / 空行
        };
        let s = String::from_utf8_lossy(payload).trim().to_string();
        if s == "[DONE]" {
            self.finalize(out);
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            return;
        };
        // usage-only chunk（choices 为空，末尾，需 include_usage）
        if let Some(usage) = v.get("usage") {
            if let Some(n) = usage.get("prompt_tokens").and_then(|x| x.as_u64()) {
                self.in_tokens = n;
            } else if self.in_tokens == 0 && self.input_fallback > 0 {
                self.in_tokens = self.input_fallback; // Phase 41：上游缺 prompt_tokens → 启发式兜底
            }
            if let Some(n) = usage.get("completion_tokens").and_then(|x| x.as_u64()) {
                self.out_tokens = n;
            }
        }
        let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) else {
            return; // 无 choice（如纯 usage chunk）→ 等 [DONE] 收尾
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        if !self.started {
            self.emit_start(out);
        }
        // Phase 47：推理增量 → Anthropic thinking 块（OpenCode Go / DeepSeek 系 `reasoning_content`）。
        // 上游按 reasoning→content→tool_calls 顺序流式发送，先到开块保证块序 thinking→text→tool；
        // 若 text 已开（异常顺序）则丢弃推理，避免索引错乱产生非法事件序列。
        if let Some(re) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            if !re.is_empty() && self.text_index.is_none() {
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
                            "content_block": { "type": "thinking", "thinking": "", "signature": "" },
                        }),
                    );
                }
                self.reasoning.push_str(re);
                self.out_chars += re.chars().count() as u64;
                emit(
                    out,
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": self.thinking_index.unwrap_or(0),
                        "delta": { "type": "thinking_delta", "thinking": re },
                    }),
                );
            }
        }
        // 文本增量；空占位分片不能关闭仍在接收参数的工具块。
        if let Some(content) = delta
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
        {
            if self.text_index.is_none() {
                self.open_text(out);
            }
            self.out_chars += content.chars().count() as u64;
            emit(
                out,
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": self.text_index.unwrap_or(0),
                    "delta": { "type": "text_delta", "text": content },
                }),
            );
        }

        // 工具调用增量（按 OpenAI tool index 缓冲，arguments 分片发 input_json_delta）
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let oai = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                if self.text_index.is_some() {
                    self.close_text(out);
                }
                self.close_thinking(out); // Phase 47：开工具块前先关 thinking（无文本直接 reasoning→tool 场景）
                let pos = self.tools.iter().position(|t| t.oa_index == oai);
                let block_index = match pos {
                    Some(p) => self.tools[p].block_index,
                    None => {
                        let id = tc
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.tools.push(ToolState {
                            oa_index: oai,
                            block_index: idx,
                            open: true,
                        });
                        emit(
                            out,
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": {},
                                },
                            }),
                        );
                        idx
                    }
                };
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    if !args.is_empty() {
                        self.out_chars += args.chars().count() as u64;
                        emit(
                            out,
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": { "type": "input_json_delta", "partial_json": args },
                            }),
                        );
                    }
                }
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                self.finish = Some(fr.to_string());
            }
        }
    }

    fn emit_start(&mut self, out: &mut Vec<u8>) {
        self.started = true;
        // Phase 46：CC 用 message_start.usage.input_tokens 跟踪上下文（Context 进度条数据源）。
        // OpenAI 流式默认把 prompt_tokens 放在末尾 usage chunk，流启动时拿不到真实值 →
        // 用请求体启发式（字符/4，与 count_tokens 兜底一致）先顶上；若上游提前在某个 chunk
        // 返回了 prompt_tokens（已解析进 in_tokens），则用真实值。修复 oc/xf 等翻译模型
        // 上下文进度条永远 0 的问题。
        let in_tokens = if self.in_tokens > 0 {
            self.in_tokens
        } else {
            self.input_fallback
        };
        emit(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_openai_1",
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

    fn open_text(&mut self, out: &mut Vec<u8>) {
        // Phase 47：thinking 块必须先 stop，text 块才能 start（Anthropic 块序）
        self.close_thinking(out);
        for t in self.tools.iter_mut() {
            if t.open {
                emit(out, "content_block_stop", json!({ "type": "content_block_stop", "index": t.block_index }));
                t.open = false;
            }
        }
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

    /// Phase 47：关闭 thinking 块（若开着）。thinking 必须排在 text/tool 块之前，故开新块前调用。
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

    fn finalize(&mut self, out: &mut Vec<u8>) {
        if self.emitted_stop {
            return;
        }
        self.close_thinking(out); // Phase 47：收尾先关 thinking（若一直无 text/tool）
        self.close_text(out);
        for t in self.tools.iter_mut() {
            if t.open {
                emit(out, "content_block_stop", json!({ "type": "content_block_stop", "index": t.block_index }));
                t.open = false;
            }
        }
        if !self.started {
            self.emit_start(out);
        }
        let out_tokens = if self.out_tokens > 0 {
            self.out_tokens
        } else {
            ((self.out_chars as f64) / 4.0).ceil() as u64
        };
        // Phase 41：上游未返回 usage chunk（或缺 prompt_tokens）时输入用请求体启发式兜底
        let in_tokens = if self.in_tokens > 0 {
            self.in_tokens
        } else {
            self.input_fallback
        };
        // Phase 26/27：真实用量提取（usage chunk 的 prompt/completion tokens；无则启发式）
        crate::proxy::accumulate_usage(
            &self.usage,
            &self.dirty,
            &self.provider,
            &self.model,
            in_tokens,
            out_tokens,
        );
        let stop_reason = match self.finish.as_deref() {
            Some("tool_calls") => "tool_use",
            Some("length") => "max_tokens",
            Some("stop_sequence") => "stop_sequence",
            _ => "end_turn",
        };
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

// 供测试确认工具缓冲结构（无实际用途，避免 dead_code 告警）。
#[allow(dead_code)]
fn _tool_indexes(tools: &[ToolState]) -> HashMap<u32, usize> {
    tools.iter().map(|t| (t.oa_index, t.block_index)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::UsageSink;

    fn sink() -> (UsageSink, Arc<Mutex<UsageStats>>) {
        let usage = Arc::new(Mutex::new(UsageStats::default()));
        let s = UsageSink {
            usage: usage.clone(),
            provider: "oa".into(),
            model: "gpt-4o-mini".into(),
            dirty: Arc::new(AtomicBool::new(false)),
            input_fallback: 0,
        };
        (s, usage)
    }

    #[test]
    fn stream_empty_content_does_not_close_tool_arguments() {
        let (sink, _) = sink();
        let mut tr = OpenAiStreamTranslator::new("test-model".into(), sink);
        let mut out = Vec::new();
        for delta in [
            json!({"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Read","arguments":"{\"file_path\":"}}]}),
            json!({"content":"","tool_calls":[{"index":0,"function":{"arguments":"\"README.md\"}"}}]}),
            json!({"content":""}),
        ] {
            let line = format!("data: {}\n\n", json!({"choices":[{"delta":delta}]}));
            tr.push(line.as_bytes(), &mut out);
        }
        tr.finish(&mut out);
        let mut open = std::collections::HashSet::new();
        let mut arguments = String::new();
        for line in std::str::from_utf8(&out).unwrap().lines() {
            let Some(payload) = line.strip_prefix("data: ") else { continue };
            let event: Value = serde_json::from_str(payload).unwrap();
            let index = event["index"].as_u64().unwrap_or(0);
            match event["type"].as_str().unwrap() {
                "content_block_start" => {
                    assert_eq!(event["content_block"]["type"], "tool_use");
                    assert!(open.insert(index));
                }
                "content_block_delta" => {
                    assert!(open.contains(&index), "delta for closed block: {event}");
                    arguments.push_str(event["delta"]["partial_json"].as_str().unwrap());
                }
                "content_block_stop" => { assert!(open.remove(&index)); }
                _ => {}
            }
        }
        assert!(open.is_empty());
        assert_eq!(serde_json::from_str::<Value>(&arguments).unwrap(), json!({"file_path":"README.md"}));
    }

    #[test]
    fn thinking_blocks_include_signature_in_stream_and_message() {
        let (sink, _) = sink();
        let mut tr = OpenAiStreamTranslator::new("test-model".into(), sink);
        let mut out = Vec::new();
        tr.push(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n", &mut out);
        tr.push(b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"test\"}}]}\n\n", &mut out);
        tr.finish(&mut out);
        let events: Vec<Value> = std::str::from_utf8(&out).unwrap().lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|payload| serde_json::from_str(payload).unwrap()).collect();
        assert_eq!(events[1]["content_block"]["type"], "thinking");
        let thinking = events.iter().find(|event| event["content_block"]["type"] == "thinking").unwrap();
        assert_eq!(thinking["content_block"]["signature"].as_str(), Some(""));
        let message = openai_message_to_anthropic(&json!({
            "choices":[{"message":{"reasoning_content":"test","content":"done"},"finish_reason":"stop"}]
        }), "test-model");
        assert_eq!(message["content"][0]["signature"].as_str(), Some(""));
    }

    #[test]
    fn anthropic_to_openai_translates_body() {
        // 请求翻译：system→system 消息、tool_use/tool_result→tool/tool_calls、tools→function、
        // stream→stream_options.include_usage、stop_sequences→stop
        let body = serde_json::json!({
            "model": "gpt-4o-mini",
            "system": [{"type":"text","text":"你是助手"}],
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[{"type":"text","text":"查一下"},
                    {"type":"tool_use","id":"t1","name":"get_weather","input":{"city":"北京"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"晴"}]}
            ],
            "tools": [{"name":"get_weather","description":"天气","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
            "max_tokens": 32000,
            "stream": true,
            "stop_sequences": ["\n\n"]
        });
        let out = anthropic_to_openai(&body, "gpt-4o-mini");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手");
        assert_eq!(msgs[1]["content"], "hi");
        // assistant: text + tool_calls（content 与 tool_calls 分离）
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], r#"{"city":"北京"}"#);
        // tool_result → role=tool
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "t1");
        assert_eq!(msgs[3]["content"], "晴");
        // tools 转换 + stream_options
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["stream_options"]["include_usage"], true);
        assert_eq!(out["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(out["max_tokens"], 32000);
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn openai_message_to_anthropic_converts() {
        // 非流式：content + tool_calls → 文本块 + tool_use 块；stop_reason 映射；usage
        let body = serde_json::json!({
            "id":"chatcmpl-x",
            "choices":[{
                "index":0,
                "message":{"role":"assistant","content":"结果",
                    "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"北京\"}"}}]},
                "finish_reason":"tool_calls"
            }],
            "usage":{"prompt_tokens":11,"completion_tokens":5}
        });
        let out = openai_message_to_anthropic(&body, "oa-gpt-4o-mini");
        assert_eq!(out["type"], "message");
        assert_eq!(out["model"], "oa-gpt-4o-mini");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "结果");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["input"]["city"], "北京");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 5);
    }

    #[test]
    fn openai_stream_translates_text_and_tool_calls() {
        // 流式：文本增量 + 工具调用分片 + finish + usage → Anthropic 事件序列 + 用量提取
        let (s, usage) = sink();
        let mut tr = OpenAiStreamTranslator::new("oa-gpt-4o-mini".into(), s);
        let mut out = Vec::new();
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}
".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"},\"finish_reason\":null}]}
".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}
".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"北京\\\"\"}}]},\"finish_reason\":\"tool_calls\"}]}
".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":7}}
".as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains("event: message_start"), "got: {s}");
        assert!(s.contains("event: content_block_start"));
        // serde_json Map 按键排序，断言做顺序无关的键/值子串检查
        assert!(s.contains(r#""type":"text_delta""#) && s.contains(r#""text":"你""#));
        assert!(s.contains(r#""type":"tool_use""#) && s.contains(r#""id":"call_1""#) && s.contains(r#""name":"get_weather""#));
        assert!(s.contains(r#""type":"input_json_delta""#) && s.contains(r#""partial_json":"{\"city\":""#));
        assert!(s.contains(r#""partial_json":"\"北京\"""#));
        assert!(s.contains(r#""stop_reason":"tool_use""#));
        assert!(s.contains("event: message_stop"));
        // 用量提取：usage chunk 的 prompt/completion tokens
        let u = usage.lock().unwrap();
        let pu = u.providers.get("oa").unwrap();
        assert_eq!(pu.input_tokens, 10);
        assert_eq!(pu.output_tokens, 7);
    }

    #[test]
    fn openai_stream_text_only_and_heuristic_usage() {
        // 纯文本流、无 usage chunk → 收尾 message_delta(output 启发式) + end_turn
        let (s, usage) = sink();
        let mut tr = OpenAiStreamTranslator::new("oa-gpt-4o-mini".into(), s);
        let mut out = Vec::new();
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}
".as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(r#""stop_reason":"end_turn""#), "got: {s}");
        assert!(s.contains(r#""type":"text_delta""#) && s.contains(r#""text":"hello""#));
        // 无 usage → 启发式输出（5 字符/4 ≈ 2）
        let u = usage.lock().unwrap();
        assert!(u.providers.get("oa").unwrap().output_tokens >= 1);
    }

    #[test]
    fn openai_stream_empty_still_ends() {
        // 空流（只有 [DONE]）→ 仍有 message_start/message_stop，不 panic
        let (s, _) = sink();
        let mut tr = OpenAiStreamTranslator::new("oa-gpt-4o-mini".into(), s);
        let mut out = Vec::new();
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains("event: message_start"), "got: {s}");
        assert!(s.contains("event: message_stop"));
    }

    // ---- Phase 47：推理模型 reasoning_content 往返（OpenCode Go / DeepSeek 系）----

    #[test]
    fn request_thinking_roundtrips_to_reasoning_content() {
        // 请求翻译：assistant 的 thinking 块 → OpenAI reasoning_content（上游要求带 tool_calls 时回传）
        let body = serde_json::json!({
            "model": "oc-v4-flash",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"让我想想"},
                    {"type":"tool_use","id":"t1","name":"get_weather","input":{"city":"北京"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"晴"}]}
            ]
        });
        let out = anthropic_to_openai(&body, "oc-v4-flash");
        let msgs = out["messages"].as_array().unwrap();
        let asst = &msgs[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["reasoning_content"], "让我想想");
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t1");
    }

    #[test]
    fn nonstream_reasoning_content_becomes_thinking_block() {
        // 非流式：reasoning_content → thinking 块 + content → text 块（thinking 在前）
        let body = serde_json::json!({
            "id":"chatcmpl-r",
            "choices":[{
                "index":0,
                "message":{"role":"assistant","content":"结果","reasoning_content":"推理过程"},
                "finish_reason":"stop"
            }],
            "usage":{"prompt_tokens":11,"completion_tokens":5}
        });
        let out = openai_message_to_anthropic(&body, "oc-v4-flash");
        let c = out["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "thinking");
        assert_eq!(c[0]["thinking"], "推理过程");
        assert_eq!(c[1]["type"], "text");
        assert_eq!(c[1]["text"], "结果");
    }

    #[test]
    fn stream_reasoning_content_becomes_thinking_block() {
        // 流式：reasoning_content 增量 → thinking 块（start/delta/stop），随后 text/tool 正常
        let (s, usage) = sink();
        let mut tr = OpenAiStreamTranslator::new("oc-v4-flash".into(), s);
        let mut out = Vec::new();
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"让我\"},\"finish_reason\":null}]}\n".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"想想\"},\"finish_reason\":null}]}\n".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":null}]}\n".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":7}}\n".as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(r#""type":"thinking""#), "got: {s}");
        assert!(s.contains(r#""type":"thinking_delta""#)
            && s.contains(r#""thinking":"让我""#)
            && s.contains(r#""thinking":"想想""#));
        assert!(s.contains(r#""type":"text_delta""#) && s.contains(r#""text":"好""#));
        assert!(s.contains(r#""type":"tool_use""#) && s.contains(r#""name":"get_weather""#));
        // thinking_delta 必须先于 text_delta（块序 thinking→text→tool）
        let think_pos = s.find("thinking_delta").unwrap();
        let text_pos = s.find("text_delta").unwrap();
        assert!(think_pos < text_pos, "块序错误: {s}");
        // 用量提取：usage chunk 的真实 tokens
        let u = usage.lock().unwrap();
        let pu = u.providers.get("oa").unwrap();
        assert_eq!(pu.input_tokens, 10);
        assert_eq!(pu.output_tokens, 7);
    }

    #[test]
    fn stream_reasoning_without_text_still_orders_blocks() {
        // 无文本内容：reasoning → tool_calls 直接衔接，thinking 块必须在 tool_use 前 stop
        let (s, _) = sink();
        let mut tr = OpenAiStreamTranslator::new("oc-v4-flash".into(), s);
        let mut out = Vec::new();
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"想\"},\"finish_reason\":null}]}\n".as_bytes(), &mut out);
        tr.push("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n".as_bytes(), &mut out);
        tr.finish(&mut out);
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(r#""type":"thinking""#), "got: {s}");
        assert!(s.contains(r#""type":"tool_use""#));
        // thinking 的 stop 事件在 tool_use 的 start 之前
        let think_stop = s.find("thinking").unwrap();
        let tool_start = s.find("tool_use").unwrap();
        assert!(think_stop < tool_start, "块序错误: {s}");
    }
}
