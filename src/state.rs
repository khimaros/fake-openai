// shared server state. one mutex per concern, mirroring the single lock the
// python mock used. cloned into each request thread as an Arc.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::behavior::Behavior;
use crate::config::{normalize_model, Config};
use crate::responses::{push_specs, ResponseSpec};

pub type Shared = Arc<State>;

pub struct State {
    pub captures: Mutex<Vec<Value>>,
    pub responses: Mutex<VecDeque<ResponseSpec>>,
    // the fallback used when the queue is empty; None means the built-in stream.
    pub default_response: Mutex<Option<ResponseSpec>>,
    pub behavior: Mutex<Behavior>,
    pub stalled_once: Mutex<bool>,
    pub models: Mutex<Vec<Value>>,
    pub log_file: Mutex<Option<File>>,
    pub log_stdout: bool,
    /// Raw s16le pcm served by `/v1/audio/speech` when `--speech-file` was given; None keeps the
    /// built-in positional ramp. Read once at boot so a request costs no io.
    pub speech_pcm: Option<Vec<u8>>,
}

/// The samples of a wav, or the bytes as-is when they are already raw pcm.
///
/// Only the minimum of RIFF is parsed: find `data`, take what follows. A fixture is produced by a
/// tts endpoint or espeak, both of which write canonical 16-bit mono, and guessing at anything
/// more elaborate would fail silently as noise rather than loudly as an error.
fn raw_pcm(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" {
        return bytes;
    }
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        if id == b"data" {
            let end = (at + 8 + len).min(bytes.len());
            return bytes[at + 8..end].to_vec();
        }
        at += 8 + len + (len & 1);
    }
    bytes
}

impl State {
    pub fn from_config(c: &Config) -> State {
        let log_file = c.log_file.as_ref().map(|p| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .unwrap_or_else(|e| panic!("open log file {}: {e}", p.display()))
        });
        State {
            captures: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
            default_response: Mutex::new(c.default_response.as_ref().map(ResponseSpec::from_value)),
            behavior: Mutex::new(c.behavior.clone()),
            stalled_once: Mutex::new(false),
            models: Mutex::new(c.models.clone()),
            log_file: Mutex::new(log_file),
            log_stdout: c.log_stdout,
            speech_pcm: c.speech_file.as_ref().map(|p| {
                let bytes = std::fs::read(p)
                    .unwrap_or_else(|e| panic!("read speech file {}: {e}", p.display()));
                raw_pcm(bytes)
            }),
        }
    }

    // the configured fallback, cloned for use without holding the lock; None
    // means the built-in stream.
    pub fn default_response(&self) -> Option<ResponseSpec> {
        self.default_response.lock().unwrap().clone()
    }

    pub fn default_response_value(&self) -> Value {
        json!({"default": self.default_response.lock().unwrap().as_ref().map(|s| s.to_value())})
    }

    pub fn set_default_response(&self, body: &Value) {
        *self.default_response.lock().unwrap() = Some(ResponseSpec::from_value(body));
    }

    pub fn clear_default_response(&self) {
        *self.default_response.lock().unwrap() = None;
    }

    pub fn reset(&self) {
        self.captures.lock().unwrap().clear();
        *self.stalled_once.lock().unwrap() = false;
    }

    pub fn captures_snapshot(&self) -> Vec<Value> {
        self.captures.lock().unwrap().clone()
    }

    pub fn capture_count(&self) -> usize {
        self.captures.lock().unwrap().len()
    }

    pub fn pending(&self) -> usize {
        self.responses.lock().unwrap().len()
    }

    pub fn responses_snapshot(&self) -> Vec<Value> {
        self.responses
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.to_value())
            .collect()
    }

    pub fn responses_append(&self, body: &Value) -> usize {
        let mut q = self.responses.lock().unwrap();
        push_specs(&mut q, body);
        q.len()
    }

    pub fn responses_replace(&self, body: &Value) -> usize {
        let mut q = self.responses.lock().unwrap();
        q.clear();
        push_specs(&mut q, body);
        q.len()
    }

    pub fn responses_clear(&self) {
        self.responses.lock().unwrap().clear();
    }

    pub fn behavior_value(&self) -> Value {
        let b = self.behavior.lock().unwrap();
        json!({
            "stall_first_with_tools": b.stall_first_with_tools,
            "stall_seconds": b.stall_seconds,
            "heartbeat_sentinel": b.heartbeat_sentinel,
            "consume_only_with_tools": b.consume_only_with_tools,
            "connect_delay_ms": b.connect_delay_ms,
            "validate_chat": b.validate_chat,
            "context_window": b.context_window,
            "chars_per_token": b.chars_per_token,
            "llamaswap": b.llamaswap,
            "empty_transcript": b.empty_transcript,
        })
    }

    pub fn behavior_merge(&self, body: &Value) {
        let mut b = self.behavior.lock().unwrap();
        if let Some(v) = body.get("stall_first_with_tools").and_then(|x| x.as_bool()) {
            b.stall_first_with_tools = v;
        }
        if let Some(v) = body.get("stall_seconds").and_then(|x| x.as_u64()) {
            b.stall_seconds = v;
        }
        if let Some(v) = body.get("heartbeat_sentinel").and_then(|x| x.as_str()) {
            b.heartbeat_sentinel = v.to_string();
        }
        if let Some(v) = body
            .get("consume_only_with_tools")
            .and_then(|x| x.as_bool())
        {
            b.consume_only_with_tools = v;
        }
        if let Some(v) = body.get("connect_delay_ms").and_then(|x| x.as_u64()) {
            b.connect_delay_ms = v;
        }
        if let Some(v) = body.get("validate_chat").and_then(|x| x.as_bool()) {
            b.validate_chat = v;
        }
        // an explicit null clears the window (back to unbounded); a number sets it.
        if let Some(v) = body.get("context_window") {
            b.context_window = if v.is_null() { None } else { v.as_u64() };
        }
        if let Some(v) = body.get("chars_per_token").and_then(|x| x.as_u64()) {
            b.chars_per_token = v;
        }
        if let Some(v) = body.get("empty_transcript").and_then(|x| x.as_bool()) {
            b.empty_transcript = v;
        }
        if let Some(v) = body.get("llamaswap").and_then(|x| x.as_bool()) {
            b.llamaswap = v;
        }
    }

    pub fn llamaswap_enabled(&self) -> bool {
        self.behavior.lock().unwrap().llamaswap
    }

    pub fn models_snapshot(&self) -> Vec<Value> {
        self.models.lock().unwrap().clone()
    }

    // accept ["a","b"], [{"id":"a", ...}], or {"models":[...]}; ignore anything
    // else. entries are normalized to full model objects so a caller can mirror
    // a real server's metadata verbatim.
    pub fn set_models(&self, body: &Value) {
        let list = match body {
            Value::Array(a) => a,
            _ => match body.get("models").and_then(|m| m.as_array()) {
                Some(a) => a,
                None => return,
            },
        };
        *self.models.lock().unwrap() = list.iter().filter_map(normalize_model).collect();
    }
}
