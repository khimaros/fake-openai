// GET endpoints for provider introspection. /v1/models lists the configured
// models; /v1/models/{id} returns one or 404. unrelated GETs fall through to the
// router's 404, so a bare /v1 does not masquerade as a model list.

use serde_json::{json, Value};

use crate::handlers::Handler;
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

const MODELS_PATH: &str = "/v1/models";

pub struct Models;

impl Handler for Models {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "GET" && (path == MODELS_PATH || path.starts_with(&format!("{MODELS_PATH}/")))
    }

    fn routes(&self) -> &'static [&'static str] {
        &["GET /v1/models", "GET /v1/models/{id}"]
    }

    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let path = req.path.split('?').next().unwrap_or(&req.path);
        let ids = state.models_snapshot();
        if let Some(id) = path.strip_prefix(&format!("{MODELS_PATH}/")) {
            if ids.iter().any(|m| m == id) {
                return HttpResponse::json(200, &model_obj(id));
            }
            return HttpResponse::json(404, &json!({"error": format!("model not found: {id}")}));
        }
        let data: Vec<Value> = ids.iter().map(|id| model_obj(id)).collect();
        HttpResponse::json(200, &json!({"object": "list", "data": data}))
    }
}

fn model_obj(id: &str) -> Value {
    json!({"id": id, "object": "model", "created": 0, "owned_by": "fake-openai"})
}
