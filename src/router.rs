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
    images::ImagesGenerations,
    models::Models,
    Handler,
};
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

const UPSTREAM_PREFIX: &str = "/upstream/";

pub struct Router {
    handlers: Vec<Box<dyn Handler + Send + Sync>>,
}

// "/upstream/<model>/v1/chat/completions" -> "/v1/chat/completions". None when
// the path is not an upstream route.
fn strip_upstream_prefix(path: &str) -> Option<String> {
    let rest = path.strip_prefix(UPSTREAM_PREFIX)?;
    let (_model, tail) = rest.split_once("/v1/")?;
    Some(format!("/v1/{tail}"))
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
                Box::new(ImagesGenerations),
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
        // llama-swap fronts each model at /upstream/<model>/v1/..., which clients
        // fall back to when the plain path fails. strip the prefix and serve it
        // with the same handlers, so both routes reach one implementation. with
        // the extension off the whole namespace 404s, so a caller can prove its
        // client works against a plain openai server.
        let path = if path.starts_with(UPSTREAM_PREFIX) {
            match state.llamaswap_enabled() {
                true => strip_upstream_prefix(&path).unwrap_or(path),
                false => return self.not_found(&req.method, &path),
            }
        } else {
            path
        };
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
