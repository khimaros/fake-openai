// llama.cpp's native endpoints, served under --llamacpp.
//
// its own server puts these at the root and llama-swap fronts them at
// /upstream/<model>/<path> with no /v1, which is why the router's gateway rewrite
// has two shapes. the family is gated on a behavior flag rather than on its
// matchers, so the same binary can prove a client works against a plain openai
// server and against this one.

use serde_json::{json, Value};

use crate::handlers::Handler;
use crate::llama::{self, EmbeddingSpec, ScoreSpec};
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub struct Props;
pub struct Tokenize;
pub struct Completion;
pub struct Embedding;

// llama-server's two native embedding routes, and the openai one. the native
// pair answer a matrix of rows; the openai one a single vector and a usage.
const NATIVE_EMBEDDING_PATHS: &[&str] = &["/embedding", "/embeddings"];
const OAI_EMBEDDINGS_PATH: &str = "/v1/embeddings";

impl Handler for Embedding {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && (NATIVE_EMBEDDING_PATHS.contains(&path) || path == OAI_EMBEDDINGS_PATH)
    }

    // an embedding server is a llama-server started with a pooling, so the
    // family needs both: without one it is the scoring server it always was.
    fn enabled(&self, state: &Shared) -> bool {
        state.llamacpp_enabled() && state.pooling().is_some()
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /embedding", "POST /embeddings", "POST /v1/embeddings"]
    }

    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let oai = req.path.split('?').next().unwrap_or("").ends_with(OAI_EMBEDDINGS_PATH);
        let pooled = state.pooling().as_deref() == Some(llama::POOLING_LAST);
        if oai && !pooled {
            // refused before the queue is read, so the spec waits for the
            // request that can use it.
            return HttpResponse::json(
                400,
                &json!({"error": {"code": 400, "type": "invalid_request_error",
                                  "message": llama::POOLING_NONE_REFUSAL}}),
            );
        }
        let prompt = req.body.get(if oai { "input" } else { "content" });
        let tokens = match llama::prompt_ids(prompt) {
            Some(ids) if !ids.is_empty() => ids,
            Some(_) => return native_error(400, "the prompt is empty: no position to return"),
            None => return native_error(400, "missing required parameter: content or input"),
        };
        let spec = state
            .embeddings_pop()
            .or_else(|| state.default_embedding())
            .unwrap_or_else(EmbeddingSpec::default_answer);
        if let Some(failure) = spec.failure() {
            return failure;
        }
        let mut rows = spec.rows(&tokens);
        if !pooled {
            return HttpResponse::json(200, &json!([{"index": 0, "embedding": rows}]));
        }
        let last = rows.pop().unwrap_or_default();
        let raw = req.body.get("embd_normalize").and_then(|n| n.as_i64())
            == Some(llama::RAW_NORMALIZE);
        let vector = if raw { last } else { llama::unit(&last) };
        if !oai {
            return HttpResponse::json(200, &json!([{"index": 0, "embedding": [vector]}]));
        }
        HttpResponse::json(
            200,
            &json!({"object": "list",
                    "model": served_model(state),
                    "data": [{"index": 0, "object": "embedding", "embedding": vector}],
                    "usage": {"prompt_tokens": tokens.len(), "total_tokens": tokens.len()}}),
        )
    }
}

// a native failure is a bare {"error": "..."} string, unlike openai's object.
fn native_error(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, &json!({"error": message}))
}

impl Handler for Props {
    fn matches(&self, method: &str, path: &str) -> bool {
        matches!(method, "GET" | "POST") && path == "/props"
    }

    fn enabled(&self, state: &Shared) -> bool {
        state.llamacpp_enabled()
    }

    fn routes(&self) -> &'static [&'static str] {
        &["GET /props"]
    }

    // a decision client derives its prompt affixes from `chat_template`, so this
    // is the one field a caller cannot afford to have invented for it.
    fn handle(&self, state: &Shared, _req: &ParsedRequest) -> HttpResponse {
        HttpResponse::json(
            200,
            &json!({"status": "ok",
                    "model_path": "fake.gguf",
                    "model_alias": served_model(state),
                    "vocab_type": 2,
                    "n_vocab": llama::VOCAB,
                    "n_ctx": 4096,
                    "n_ctx_train": 4096,
                    "evals": false,
                    "params": false,
                    "add_bos_token": false,
                    "bos_token": llama::BOS_TOKEN,
                    "eos_token": llama::EOS_TOKEN,
                    "utf8_extra_tokens": [],
                    "template_kwargs": {},
                    "chat_template": state.chat_template()}),
        )
    }
}

impl Handler for Tokenize {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == "/tokenize"
    }

    fn enabled(&self, state: &Shared) -> bool {
        state.llamacpp_enabled()
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /tokenize"]
    }

    fn handle(&self, _state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let Some(content) = req.body.get("content").and_then(|c| c.as_str()) else {
            return native_error(400, "missing required parameter: content");
        };
        let add_special = req
            .body
            .get("add_special")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);
        let tokens = llama::token_ids(content, add_special);
        // the shape llama.cpp answers with. parse_special is accepted and ignored:
        // with no special tokens in the byte vocabulary there is nothing to parse,
        // and a client asking for it is asking for chat markers to survive, which
        // they do here because every byte round-trips.
        HttpResponse::json(
            200,
            &json!({"tokens": tokens,
                    "chars": content.chars().count(),
                    "tokens_n": tokens.len()}),
        )
    }
}

impl Handler for Completion {
    fn matches(&self, method: &str, path: &str) -> bool {
        // exact, because "/completion" is a substring of the openai route
        // "/v1/chat/completions", which is a different family answering a
        // different question.
        method == "POST" && path == "/completion"
    }

    fn enabled(&self, state: &Shared) -> bool {
        state.llamacpp_enabled()
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /completion"]
    }

    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        // the scoring queue is separate from the chat queue because one generated
        // turn carries many scoring calls, and interleaving them would make either
        // sequence unreadable.
        let spec = state
            .probs_pop()
            .or_else(|| state.default_probs())
            .unwrap_or_else(ScoreSpec::default_answer);
        let n_probs = req
            .body
            .get("n_probs")
            .and_then(|x| x.as_u64())
            .unwrap_or(llama::DEFAULT_N_PROBS as u64) as u32;
        spec.render(prompt_len(&req.body), n_probs, &served_model(state))
    }
}

// how long the prompt was, in the units it was sent: bytes for text, entries for
// token ids. derived rather than fixed so a client's prefix reuse and cache
// accounting have something true to assert on.
fn prompt_len(body: &Value) -> u64 {
    match body.get("prompt") {
        Some(Value::String(text)) => text.len() as u64,
        Some(Value::Array(ids)) => ids.len() as u64,
        _ => 0,
    }
}

fn served_model(state: &Shared) -> String {
    state
        .models_snapshot()
        .first()
        .and_then(|m| m.get("id").and_then(|id| id.as_str()))
        .unwrap_or(crate::config::DEFAULT_MODEL)
        .to_string()
}
