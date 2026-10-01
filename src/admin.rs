// the /__admin control plane: inspect captures, program responses, configure
// behavior and models, and reset state. admin requests are never captured.

use serde_json::json;

use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub const ADMIN_PREFIX: &str = "/__admin";

// the control-plane routes, advertised in the 404 endpoint listing.
pub const ROUTES: &[&str] = &[
    "GET /__admin/health",
    "GET /__admin/captures",
    "POST /__admin/reset",
    "GET|POST|PUT|DELETE /__admin/responses",
    "GET|PUT|DELETE /__admin/default",
    "GET|POST|PUT|DELETE /__admin/probs",
    "GET|PUT|DELETE /__admin/default_probs",
    "GET|POST /__admin/behavior",
    "GET|PUT /__admin/models",
];

pub fn handle(state: &Shared, req: &ParsedRequest, path: &str) -> HttpResponse {
    let sub = &path[ADMIN_PREFIX.len()..];
    match (req.method.as_str(), sub) {
        ("GET", "/health") => HttpResponse::json(
            200,
            &json!({"ok": true, "captures": state.capture_count(), "pending_responses": state.pending(),
                     "pending_probs": state.probs_pending()}),
        ),
        ("GET", "/captures") => {
            HttpResponse::json(200, &json!({"captures": state.captures_snapshot()}))
        }
        ("POST", "/reset") => {
            state.reset();
            ok()
        }
        ("GET", "/responses") => {
            HttpResponse::json(200, &json!({"responses": state.responses_snapshot()}))
        }
        ("POST", "/responses") => ok_pending(state.responses_append(&req.body)),
        ("PUT", "/responses") => ok_pending(state.responses_replace(&req.body)),
        ("DELETE", "/responses") => {
            state.responses_clear();
            ok()
        }
        // the scoring queue, programmatic peer of /__admin/responses. a spec that
        // names a token this vocabulary does not have is refused rather than
        // quietly answered with a different token, since the whole value of the
        // family is that the distribution is the one that was asked for.
        ("GET", "/probs") => HttpResponse::json(200, &json!({"probs": state.probs_snapshot()})),
        ("POST", "/probs") => match state.probs_append(&req.body) {
            Ok(n) => ok_pending(n),
            Err(e) => bad_request(e),
        },
        ("PUT", "/probs") => match state.probs_replace(&req.body) {
            Ok(n) => ok_pending(n),
            Err(e) => bad_request(e),
        },
        ("DELETE", "/probs") => {
            state.probs_clear();
            ok()
        }
        ("GET", "/default_probs") => HttpResponse::json(200, &state.default_probs_value()),
        ("PUT", "/default_probs") => match state.set_default_probs(&req.body) {
            Ok(()) => ok(),
            Err(e) => bad_request(e),
        },
        ("DELETE", "/default_probs") => {
            state.clear_default_probs();
            ok()
        }
        ("GET", "/default") => HttpResponse::json(200, &state.default_response_value()),
        ("PUT", "/default") => {
            state.set_default_response(&req.body);
            ok()
        }
        ("DELETE", "/default") => {
            state.clear_default_response();
            ok()
        }
        ("GET", "/behavior") => HttpResponse::json(200, &state.behavior_value()),
        ("POST", "/behavior") => {
            state.behavior_merge(&req.body);
            HttpResponse::json(200, &state.behavior_value())
        }
        ("GET", "/models") => HttpResponse::json(200, &json!({"models": state.models_snapshot()})),
        ("PUT", "/models") => {
            state.set_models(&req.body);
            ok()
        }
        _ => HttpResponse::json(
            404,
            &json!({"error": format!("unknown admin endpoint: {} {sub}", req.method)}),
        ),
    }
}

fn ok() -> HttpResponse {
    HttpResponse::json(200, &json!({"ok": true}))
}

fn bad_request(message: String) -> HttpResponse {
    HttpResponse::json(400, &json!({"ok": false, "error": message}))
}

fn ok_pending(n: usize) -> HttpResponse {
    HttpResponse::json(200, &json!({"ok": true, "pending": n}))
}
