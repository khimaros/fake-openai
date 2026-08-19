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
        let models = state.models_snapshot();
        if let Some(id) = path.strip_prefix(&format!("{MODELS_PATH}/")) {
            // with llamaswap on, an advertised alias also resolves to the model
            // behind it, mirroring a gateway that accepts either name.
            let aliases = state.llamaswap_enabled();
            if let Some(model) = models
                .iter()
                .find(|m| model_names(m, aliases).any(|n| n == id))
            {
                return HttpResponse::json(200, model);
            }
            return HttpResponse::json(404, &json!({"error": format!("model not found: {id}")}));
        }
        HttpResponse::json(200, &json!({"object": "list", "data": models}))
    }
}

// every name a model answers to: its id, plus any advertised llama-swap aliases
// when that extension is enabled.
pub fn model_names(model: &Value, aliases: bool) -> impl Iterator<Item = &str> {
    let id = model.get("id").and_then(|v| v.as_str());
    let alias_list = match aliases {
        true => model
            .pointer("/meta/llamaswap/aliases")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
        false => &[],
    };
    id.into_iter()
        .chain(alias_list.iter().filter_map(|v| v.as_str()))
}
