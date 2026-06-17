// dispatch a parsed request: admin first (never captured), then capture every
// other request and route it to the first matching handler.

use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::admin::{self, ADMIN_PREFIX};
use crate::capture;
use crate::handlers::{
    audio::{AudioSpeech, AudioTranscriptions, AudioVoices},
    chat::ChatCompletions,
    models::Models,
    Handler,
};
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub struct Router {
    handlers: Vec<Box<dyn Handler + Send + Sync>>,
}

impl Router {
    pub fn new() -> Router {
        // order matters: specific paths before the /v1/models matcher.
        Router {
            handlers: vec![
                Box::new(ChatCompletions),
                Box::new(AudioSpeech),
                Box::new(AudioTranscriptions),
                Box::new(AudioVoices),
                Box::new(Models),
            ],
        }
    }

    pub fn route(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let path = req.path.split('?').next().unwrap_or(&req.path).to_string();
        if path.starts_with(ADMIN_PREFIX) {
            return admin::handle(state, req, &path);
        }
        // connection-level latency: pay it before touching the request, so a
        // client sees a slow first byte. admin (above) is exempt.
        let connect_delay = state.behavior.lock().unwrap().connect_delay_ms;
        if connect_delay > 0 {
            thread::sleep(Duration::from_millis(connect_delay));
        }
        capture::record(state, req);
        for handler in &self.handlers {
            if handler.matches(&req.method, &path) {
                return handler.handle(state, req);
            }
        }
        self.not_found(&req.method, &path)
    }

    // a 404 that advertises the served endpoints, so an unknown path is a
    // discovery aid rather than a dead end.
    fn not_found(&self, method: &str, path: &str) -> HttpResponse {
        let mut endpoints: Vec<&str> = self
            .handlers
            .iter()
            .flat_map(|h| h.routes())
            .copied()
            .collect();
        endpoints.extend_from_slice(admin::ROUTES);
        HttpResponse::json(
            404,
            &json!({"error": format!("not found: {method} {path}"), "endpoints": endpoints}),
        )
    }
}
