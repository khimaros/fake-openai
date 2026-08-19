// the programmable response queue. specs enter from a startup file, from stdin,
// or from the admin api, and are popped in order by the chat handler.

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::Path;
use std::thread;

use serde_json::{json, Value};

use crate::config::DEFAULT_MODEL;
use crate::response::{Frame, HttpResponse, CONTENT_TYPE_SSE};
use crate::state::Shared;

// a programmed response. either high-level fields the binary renders into an
// SSE stream or a single chat.completion json, or a raw body returned verbatim.
//
// fault/timing fields: `error` returns an openai-style error body; `chunks`
// streams one content delta each; `truncate` ends a stream with no finish frame
// or [DONE]; `chunk_delay_ms` paces deltas and `hang_ms` holds the socket open
// before closing -- together they drive a harness's streaming and timeout paths.
#[derive(Clone)]
pub struct ResponseSpec {
    raw_body: Option<String>,
    content_type: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_chunks: Option<Vec<String>>,
    chunks: Option<Vec<String>>,
    tool_calls: Option<Value>,
    error: Option<Value>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    status: u16,
    delay_ms: u64,
    chunk_delay_ms: u64,
    hang_ms: u64,
    truncate: bool,
    // set at request time (not from the spec json) when the context-window
    // behavior is on: overrides the reported prompt/total usage with a value
    // derived from the actual request size.
    derived_prompt: Option<u64>,
}

// completion tokens reported alongside a derived prompt when the spec carries no
// usage of its own -- a small nonzero count so total_tokens stays realistic.
const DERIVED_COMPLETION_TOKENS: u64 = 16;

impl ResponseSpec {
    pub fn from_value(v: &Value) -> ResponseSpec {
        ResponseSpec {
            raw_body: str_field(v, "raw_body"),
            content_type: str_field(v, "content_type"),
            content: str_field(v, "content"),
            reasoning: str_field(v, "reasoning"),
            reasoning_chunks: string_vec(v, "reasoning_chunks"),
            chunks: string_vec(v, "chunks"),
            tool_calls: v.get("tool_calls").cloned(),
            error: v.get("error").cloned(),
            finish_reason: str_field(v, "finish_reason"),
            usage: v.get("usage").cloned(),
            status: v.get("status").and_then(|x| x.as_u64()).unwrap_or(200) as u16,
            delay_ms: u64_field(v, "delay_ms"),
            chunk_delay_ms: u64_field(v, "chunk_delay_ms"),
            hang_ms: u64_field(v, "hang_ms"),
            truncate: v.get("truncate").and_then(|x| x.as_bool()).unwrap_or(false),
            derived_prompt: None,
        }
    }

    // override the reported prompt/total usage with a value derived from the
    // request size (set by the context-window behavior), keeping any spec
    // completion count. consumes and returns self so the handler can chain it.
    pub fn with_derived_prompt(mut self, prompt_tokens: u64) -> ResponseSpec {
        self.derived_prompt = Some(prompt_tokens);
        self
    }

    pub fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        insert_opt(&mut m, "raw_body", self.raw_body.as_ref().map(|s| json!(s)));
        insert_opt(
            &mut m,
            "content_type",
            self.content_type.as_ref().map(|s| json!(s)),
        );
        insert_opt(&mut m, "content", self.content.as_ref().map(|s| json!(s)));
        insert_opt(&mut m, "reasoning", self.reasoning.as_ref().map(|s| json!(s)));
        insert_opt(
            &mut m,
            "reasoning_chunks",
            self.reasoning_chunks.as_ref().map(|c| json!(c)),
        );
        insert_opt(&mut m, "chunks", self.chunks.as_ref().map(|c| json!(c)));
        insert_opt(&mut m, "tool_calls", self.tool_calls.clone());
        insert_opt(&mut m, "error", self.error.clone());
        insert_opt(
            &mut m,
            "finish_reason",
            self.finish_reason.as_ref().map(|s| json!(s)),
        );
        insert_opt(&mut m, "usage", self.usage.clone());
        m.insert("status".to_string(), json!(self.status));
        m.insert("delay_ms".to_string(), json!(self.delay_ms));
        m.insert("chunk_delay_ms".to_string(), json!(self.chunk_delay_ms));
        m.insert("hang_ms".to_string(), json!(self.hang_ms));
        m.insert("truncate".to_string(), json!(self.truncate));
        Value::Object(m)
    }

    pub fn delay_ms(&self) -> u64 {
        self.delay_ms
    }

    // render to an error body, a raw body, a stream, or a single json object.
    pub fn render(&self, stream: bool) -> HttpResponse {
        if let Some(err) = &self.error {
            return self.render_error(err);
        }
        if let Some(raw) = &self.raw_body {
            let ct = self
                .content_type
                .clone()
                .unwrap_or_else(|| CONTENT_TYPE_SSE.to_string());
            return HttpResponse::new(self.status, &ct, raw.clone().into_bytes());
        }
        if stream {
            self.render_sse()
        } else {
            self.render_json()
        }
    }

    // an openai-style error body. a bare status of 200 becomes 400, since an
    // error spec implies a failed request.
    fn render_error(&self, err: &Value) -> HttpResponse {
        let status = if self.status == 200 { 400 } else { self.status };
        let body = match err {
            Value::Object(_) => {
                let mut e = err.clone();
                e.as_object_mut()
                    .unwrap()
                    .entry("type")
                    .or_insert_with(|| json!("invalid_request_error"));
                json!({ "error": e })
            }
            other => {
                json!({"error": {"message": other, "type": "invalid_request_error", "code": null}})
            }
        };
        HttpResponse::json(status, &body)
    }

    fn render_sse(&self) -> HttpResponse {
        let pieces = self.content_pieces();
        let mut frames: Vec<Frame> = Vec::new();
        // reasoning streams first (as `reasoning_content`, the field openai-compatible
        // endpoints use), before the visible content. multiple `reasoning_chunks`
        // stream one delta each (paced by chunk_delay_ms) to exercise incremental
        // reasoning rendering, just like `chunks` does for content.
        for (i, piece) in self.reasoning_pieces().iter().enumerate() {
            let delay = if i == 0 { 0 } else { self.chunk_delay_ms };
            frames.push(frame(delay, self.reasoning_chunk(piece)));
        }
        // one content delta per piece; tool_calls and the leading role ride the
        // first delta. with no pieces, still emit a single role delta.
        if pieces.is_empty() {
            frames.push(frame(0, self.delta_chunk(None, self.tool_calls.clone())));
        } else {
            for (i, piece) in pieces.iter().enumerate() {
                let delay = if i == 0 { 0 } else { self.chunk_delay_ms };
                let tc = if i == 0 {
                    self.tool_calls.clone()
                } else {
                    None
                };
                frames.push(frame(delay, self.delta_chunk(Some(piece), tc)));
            }
        }
        // a truncated stream stops here: no finish frame, no [DONE].
        if !self.truncate {
            frames.push(frame(0, self.finish_chunk()));
            frames.push(frame(0, "data: [DONE]\n\n".to_string()));
        }
        // the buffered concatenation is byte-identical when nothing paces or
        // cuts the stream, so the common path keeps a single write.
        if self.is_timed() {
            HttpResponse::streamed(self.status, CONTENT_TYPE_SSE, frames, self.hang_ms)
        } else {
            let body = frames.into_iter().flat_map(|f| f.data).collect();
            HttpResponse::sse(self.status, body)
        }
    }

    fn render_json(&self) -> HttpResponse {
        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), json!("assistant"));
        message.insert("content".to_string(), json!(self.content_pieces().concat()));
        let reasoning = self.reasoning_pieces().concat();
        insert_opt(
            &mut message,
            "reasoning_content",
            (!reasoning.is_empty()).then(|| json!(reasoning)),
        );
        insert_opt(&mut message, "tool_calls", self.tool_calls.clone());
        let body = json!({
            "id": "1", "object": "chat.completion", "created": 0, "model": DEFAULT_MODEL,
            "choices": [{"index": 0, "message": Value::Object(message), "finish_reason": self.finish()}],
            "usage": self.usage(),
        });
        HttpResponse::json(self.status, &body)
    }

    // the content delivered, split into streamed pieces: `chunks` if given, else
    // the single `content`, else nothing.
    fn content_pieces(&self) -> Vec<String> {
        if let Some(chunks) = &self.chunks {
            chunks.clone()
        } else if let Some(c) = &self.content {
            vec![c.clone()]
        } else {
            vec![]
        }
    }

    // the reasoning delivered, split into streamed pieces: `reasoning_chunks` if
    // given, else the single `reasoning`, else nothing. mirrors content_pieces.
    fn reasoning_pieces(&self) -> Vec<String> {
        if let Some(chunks) = &self.reasoning_chunks {
            chunks.clone()
        } else if let Some(r) = &self.reasoning {
            vec![r.clone()]
        } else {
            vec![]
        }
    }

    // a streaming `data:` frame carrying one assistant delta.
    fn delta_chunk(&self, content: Option<&str>, tool_calls: Option<Value>) -> String {
        let mut delta = serde_json::Map::new();
        delta.insert("role".to_string(), json!("assistant"));
        if let Some(c) = content {
            delta.insert("content".to_string(), json!(c));
        }
        insert_opt(&mut delta, "tool_calls", tool_calls);
        let chunk = json!({
            "id": "1", "object": "chat.completion.chunk", "created": 0, "model": DEFAULT_MODEL,
            "choices": [{"index": 0, "delta": Value::Object(delta), "finish_reason": null}],
        });
        format!("data: {chunk}\n\n")
    }

    // a streaming `data:` frame carrying one reasoning delta (`reasoning_content`,
    // which openai-compatible endpoints use for thinking output).
    fn reasoning_chunk(&self, text: &str) -> String {
        let delta = json!({ "role": "assistant", "reasoning_content": text });
        let chunk = json!({
            "id": "1", "object": "chat.completion.chunk", "created": 0, "model": DEFAULT_MODEL,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
        });
        format!("data: {chunk}\n\n")
    }

    // the terminating frame: an empty delta with the finish reason and usage.
    fn finish_chunk(&self) -> String {
        let chunk = json!({
            "id": "1", "object": "chat.completion.chunk", "created": 0, "model": DEFAULT_MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": self.finish()}],
            "usage": self.usage(),
        });
        format!("data: {chunk}\n\n")
    }

    // a stream needs the raw socket only when something paces or cuts it.
    fn is_timed(&self) -> bool {
        self.chunk_delay_ms > 0 || self.hang_ms > 0 || self.truncate
    }

    fn finish(&self) -> String {
        self.finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string())
    }

    fn usage(&self) -> Value {
        if let Some(prompt) = self.derived_prompt {
            let completion = self
                .usage
                .as_ref()
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|c| c.as_u64())
                .unwrap_or(DERIVED_COMPLETION_TOKENS);
            return json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion,
            });
        }
        self.usage.clone().unwrap_or_else(
            || json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}),
        )
    }
}

// append one spec, or each spec of an array, to the queue.
pub fn push_specs(queue: &mut VecDeque<ResponseSpec>, body: &Value) {
    match body {
        Value::Array(arr) => {
            for v in arr {
                queue.push_back(ResponseSpec::from_value(v));
            }
        }
        _ => queue.push_back(ResponseSpec::from_value(body)),
    }
}

// load specs from a json array/object file, or a jsonl file (one spec per line).
pub fn load_file(state: &Shared, path: &Path) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read responses file {}: {e}", path.display()));
    let mut queue = state.responses.lock().unwrap();
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_array() || v.is_object() => push_specs(&mut queue, &v),
        _ => {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    queue.push_back(ResponseSpec::from_value(&v));
                }
            }
        }
    }
}

// read specs from stdin as jsonl, one per line, appending live.
pub fn spawn_stdin_reader(state: Shared) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match handle.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                        state
                            .responses
                            .lock()
                            .unwrap()
                            .push_back(ResponseSpec::from_value(&v));
                    }
                }
            }
        }
    });
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

// a json array of strings, e.g. the streamed `chunks`.
fn string_vec(v: &Value, key: &str) -> Option<Vec<String>> {
    let arr = v.get(key)?.as_array()?;
    Some(
        arr.iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect(),
    )
}

fn frame(delay_ms: u64, data: String) -> Frame {
    Frame {
        delay_ms,
        data: data.into_bytes(),
    }
}

fn insert_opt(m: &mut serde_json::Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        m.insert(key.to_string(), v);
    }
}
