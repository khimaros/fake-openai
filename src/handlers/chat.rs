// POST /v1/chat/completions: capture has already happened; stall if configured,
// then pop the next programmed response or fall back to the fixed stream.

use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::behavior;
use crate::handlers::Handler;
use crate::response::{HttpResponse, ParsedRequest, SSE_RESPONSE};
use crate::state::Shared;

pub struct ChatCompletions;

impl Handler for ChatCompletions {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path.contains("chat/completions")
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/chat/completions"]
    }

    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        // reject malformed/unauthenticated requests before consuming the queue,
        // so a rejected request leaves the programmed sequence untouched.
        let validate = state.behavior.lock().unwrap().validate_chat;
        if validate {
            if let Some((status, message)) =
                behavior::validate_chat_request(&req.headers, &req.body)
            {
                let body = json!({"error": {"message": message, "type": "invalid_request_error", "code": null}});
                return HttpResponse::json(status, &body);
            }
        }
        maybe_stall(state, req);
        // pop the next queued spec when this request is allowed to consume one,
        // else the configured default, else the built-in stream.
        let consume = {
            let b = state.behavior.lock().unwrap();
            behavior::should_consume(&b, &req.path, &req.body)
        };
        let spec = consume
            .then(|| state.responses.lock().unwrap().pop_front())
            .flatten()
            .or_else(|| state.default_response());
        match spec {
            Some(spec) => {
                if spec.delay_ms() > 0 {
                    thread::sleep(Duration::from_millis(spec.delay_ms()));
                }
                let stream = req
                    .body
                    .get("stream")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(true);
                spec.render(stream)
            }
            None => HttpResponse::sse(200, SSE_RESPONSE.as_bytes().to_vec()),
        }
    }
}

fn maybe_stall(state: &Shared, req: &ParsedRequest) {
    let seconds = {
        let b = state.behavior.lock().unwrap();
        let mut latch = state.stalled_once.lock().unwrap();
        behavior::should_stall(&b, &mut latch, &req.path, &req.body).then_some(b.stall_seconds)
    };
    if let Some(s) = seconds {
        thread::sleep(Duration::from_secs(s));
    }
}
