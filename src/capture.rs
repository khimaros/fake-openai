// record every non-admin request as {path, headers, body} -- the contract that
// downstream harnesses inspect -- and optionally mirror it to a jsonl sink.

use std::io::Write;

use serde_json::{json, Value};

use crate::audio_probe;
use crate::handlers::audio::TRANSCRIPTIONS_PATH;
use crate::response::ParsedRequest;
use crate::state::Shared;

pub fn record(state: &Shared, req: &ParsedRequest) {
    // AND WHAT THE AUDIO WAS, for a request that carries some. `body` is lossy for binary, so a
    // consumer cannot tell silence from speech by reading it -- which is how a dead microphone
    // upstream stayed invisible while every spoken test passed. null for everything else.
    let audio = if req.path == TRANSCRIPTIONS_PATH {
        audio_probe::measure(&req.raw)
    } else {
        Value::Null
    };
    // AND WHEN IT ARRIVED. without this a consumer can only infer time from ORDER, which is enough
    // to say what happened and never enough to say when -- so correlating this ledger against a
    // client's own timestamped log meant counting entries and hoping. unix millis rather than a
    // formatted stamp: every consumer is comparing it to something else, and arithmetic on an
    // integer needs no parser and carries no timezone.
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entry = json!({
        "path": req.path,
        "headers": req.headers,
        "body": req.body,
        "audio": audio,
        "at_ms": at_ms,
    });
    state.captures.lock().unwrap().push(entry.clone());
    log(state, &entry);
}

fn log(state: &Shared, entry: &Value) {
    if !state.log_stdout && state.log_file.lock().unwrap().is_none() {
        return;
    }
    let line = entry.to_string();
    if state.log_stdout {
        println!("{line}");
    }
    if let Some(file) = state.log_file.lock().unwrap().as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}
