// record every non-admin request as {path, headers, body} -- the contract that
// downstream harnesses inspect -- and optionally mirror it to a jsonl sink.

use std::io::Write;

use serde_json::{json, Value};

use crate::response::ParsedRequest;
use crate::state::Shared;

pub fn record(state: &Shared, req: &ParsedRequest) {
    let entry = json!({"path": req.path, "headers": req.headers, "body": req.body});
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
