// llama.cpp's NATIVE vocabulary and the scoring specs its /completion answers.
//
// a decision harness reads a probability distribution off one token position and
// generates no text, so what it needs from a mock is a distribution the test
// CHOSE. real weights cannot be told that, which is why every consumer of this
// family tests the client rather than a model.
//
// the vocabulary is byte-level on purpose: id == byte, so an id is DERIVABLE from
// the text rather than looked up. a client resolves its option labels by calling
// /tokenize and scores whatever ids come back, and the ids this file reports can
// never disagree with them -- no fixture to keep in sync with a tokenizer that
// belongs to somebody else. the cost is recorded in the roadmap: 258 pieces hold
// every ascii letter and nothing outside it, so a client that labels options with
// non-ascii single characters cannot be exercised here.

use serde_json::{json, Value};

use crate::response::HttpResponse;

pub const VOCAB: u32 = 258;
pub const BOS_ID: u32 = 256;
pub const EOS_ID: u32 = 257;
pub const BOS_TOKEN: &str = "<s>";
pub const EOS_TOKEN: &str = "</s>";

// the answer when nothing is scripted: all the mass on the first option label.
// a caller that has not programmed anything is asking "does the pipeline work",
// and this clears a client's option-mass floor while reading unambiguously.
pub const DEFAULT_LABEL: u32 = b'A' as u32;

// what /completion returns when the request omits n_probs. llama.cpp answers
// without probabilities at all in that case; a mock that did the same would let a
// client's own default silently govern every test written against it.
pub const DEFAULT_N_PROBS: u32 = 64;

// zero as a logprob: ln(f64::MIN_POSITIVE), the smallest value json carries.
// padding a distribution to the requested width needs a logprob that is not
// json's null and not a lie of exactly zero probability.
pub const ZERO_LOGPROB: f64 = -708.3965;

/// the token ids of a string, one per byte, with the opening token prefixed when
/// the caller asked for specials.
pub fn token_ids(text: &str, add_special: bool) -> Vec<u32> {
    let mut ids: Vec<u32> = text.as_bytes().iter().map(|b| *b as u32).collect();
    if add_special {
        ids.insert(0, BOS_ID);
    }
    ids
}

/// the text a token id means, which a client reads back to match an entry to the
/// label it asked about.
pub fn token_text(id: u32) -> String {
    match id {
        BOS_ID => BOS_TOKEN.to_string(),
        EOS_ID => EOS_TOKEN.to_string(),
        b if b < 256 => char::from(b as u8).to_string(),
        _ => format!("<{id}>"),
    }
}

/// a scripted probability key as a token id: a number is an id, a single
/// character is its byte. anything else is a caller naming something this
/// vocabulary does not have, and saying so beats scoring a different token.
pub fn resolve_key(key: &str) -> Result<u32, String> {
    if let Ok(id) = key.parse::<u32>() {
        if id < VOCAB {
            return Ok(id);
        }
        return Err(format!("token id {key} is outside the {VOCAB}-piece vocabulary"));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii() => Ok(c as u32),
        _ => Err(format!("token {key:?} is not one ascii character or a token id")),
    }
}

// one programmed scoring answer: the distribution to report, plus the timing and
// fault fields a client's own error paths read.
//
// probabilities are reported EXACTLY as scripted and are not renormalised: a
// caller scripting 0.4 and 0.6 across two of fifty-two labels is testing what its
// client does with a distribution that has mass somewhere else, which is the
// option-mass health check and not a rounding error to tidy away.
#[derive(Clone)]
pub struct ScoreSpec {
    probs: Vec<(String, f64)>,
    cache_n: u64,
    error: Option<String>,
    status: u16,
    delay_ms: u64,
}

impl ScoreSpec {
    /// the fallback answer: one label holding everything. spelled out rather
    /// than derived, because the zero value of `status` is 0, which is not an
    /// http status at all.
    pub fn default_answer() -> ScoreSpec {
        ScoreSpec {
            probs: vec![(DEFAULT_LABEL.to_string(), 1.0)],
            cache_n: 0,
            error: None,
            status: 200,
            delay_ms: 0,
        }
    }

    pub fn from_value(v: &Value) -> Result<ScoreSpec, String> {
        let mut probs = Vec::new();
        match v.get("probs") {
            Some(Value::Object(m)) => {
                for (key, p) in m {
                    let p = p
                        .as_f64()
                        .ok_or_else(|| format!("probs[{key}] is not a number"))?;
                    resolve_key(key)?;
                    probs.push((key.clone(), p));
                }
            }
            // a spec with no distribution is a fault or a timing spec, and an
            // empty distribution is a legitimate thing to be asked for.
            None | Some(Value::Null) => {}
            Some(_) => return Err("probs must be an object of token to probability".to_string()),
        }
        let (error, status) = fault(v);
        Ok(ScoreSpec {
            probs,
            cache_n: v.get("cache_n").and_then(|x| x.as_u64()).unwrap_or(0),
            error,
            status,
            delay_ms: v
                .get("delay_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
        })
    }

    pub fn to_value(&self) -> Value {
        let probs: serde_json::Map<String, Value> = self
            .probs
            .iter()
            .map(|(k, p)| (k.clone(), json!(p)))
            .collect();
        let mut m = serde_json::Map::new();
        m.insert("probs".to_string(), Value::Object(probs));
        m.insert("cache_n".to_string(), json!(self.cache_n));
        m.insert("status".to_string(), json!(self.status));
        m.insert("delay_ms".to_string(), json!(self.delay_ms));
        if let Some(e) = &self.error {
            m.insert("error".to_string(), json!(e));
        }
        Value::Object(m)
    }

    /// render one scoring response for a prompt of `prompt_n` tokens.
    pub fn render(&self, prompt_n: u64, n_probs: u32, model: &str) -> HttpResponse {
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        if let Some(message) = &self.error {
            // llama.cpp answers a native failure with a bare string, not openai's
            // error object, and a client that shows an operator the reason reads
            // `error` directly.
            return HttpResponse::json(self.status, &json!({"error": message}));
        }

        let width = n_probs.min(VOCAB) as usize;
        let mut entries = self.distribution();
        // highest probability first, and by id on a tie so two runs of the same
        // spec return byte-identical bytes.
        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0)));
        entries.truncate(width);
        // pad to the requested width with the rest of the vocabulary, ascending by
        // id. a real server returns the top n of a whole vocabulary, and a client
        // reads a SHORT list as "that was all of it": answering n_probs=1 with two
        // entries would delete the distinction between "outside the top k" and
        // "probability zero" that its whole widening ladder is built on.
        if entries.len() < width {
            let used: std::collections::HashSet<u32> =
                entries.iter().map(|(id, _)| *id).collect();
            entries.extend(
                (0..VOCAB)
                    .filter(|id| !used.contains(id))
                    .take(width - entries.len())
                    .map(|id| (id, 0.0)),
            );
        }

        let top = entries[0].0;
        let top_logprobs: Vec<Value> = entries
            .iter()
            .map(|(id, p)| {
                json!({"id": id,
                       "prob": p,
                       "logprob": if *p > 0.0 { p.ln() } else { ZERO_LOGPROB },
                       "token": token_text(*id)})
            })
            .collect();
        HttpResponse::json(
            self.status,
            &json!({
                "id": 0,
                "object": "text_completion",
                "created": 0,
                "model": model,
                "timings": {"prompt_n": prompt_n,
                            "cache_n": self.cache_n,
                            "predicted_ms": 0,
                            "predicted_per_token_ms": 0},
                "usage": {"prompt_tokens": prompt_n,
                          "completion_tokens": 1,
                          "total_tokens": prompt_n + 1},
                "choices": [{"index": 0,
                             "text": token_text(top),
                             "stop_type": "eos",
                             "logprobs": null}],
                // one entry per scored position. a decision is one position, so
                // one entry, and a client indexes [0].
                "completion_probabilities": [{"content": false, "top_logprobs": top_logprobs}],
            }),
        )
    }

    fn distribution(&self) -> Vec<(u32, f64)> {
        if self.probs.is_empty() {
            return vec![(DEFAULT_LABEL, 1.0)];
        }
        self.probs
            .iter()
            .map(|(key, p)| (resolve_key(key).unwrap_or(DEFAULT_LABEL), *p))
            .collect()
    }
}

// the server's pooling settings an embedding client can meet. `none` returns a
// hidden state per position and `last` the final position's alone.
pub const POOLING_NONE: &str = "none";
pub const POOLING_LAST: &str = "last";
pub const POOLINGS: &[&str] = &[POOLING_NONE, POOLING_LAST];

// how wide a hidden state is when nobody scripted one. small, because a test
// that cares about the width scripts a vector, and that vector sets it.
pub const DEFAULT_EMBEDDING_WIDTH: usize = 8;

// the value of `embd_normalize` that declines normalisation. llama.cpp's default
// is the euclidean norm, which a pooled vector gets unless the request says so.
pub const RAW_NORMALIZE: i64 = -1;

// llama-server's own words when the openai route is asked on a server pooling
// `none`, which is how a client learns to read the native route instead.
pub const POOLING_NONE_REFUSAL: &str =
    "Pooling type 'none' is not OAI compatible. Please use a different pooling type";

/// the token ids of a prompt as an embedding request carries it: text is one
/// id per byte, an array of ids is itself. None when it is neither.
pub fn prompt_ids(prompt: Option<&Value>) -> Option<Vec<u32>> {
    match prompt? {
        Value::String(text) => Some(token_ids(text, false)),
        Value::Array(ids) => ids.iter().map(|id| id.as_u64().map(|n| n as u32)).collect(),
        _ => None,
    }
}

/// a vector scaled to unit euclidean length, as a pooled embedding is unless the
/// request declines.
pub fn unit(v: &[f64]) -> Vec<f64> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

// one programmed embedding answer: the hidden state at the LAST position, plus
// the timing and fault fields a client's own error paths read.
//
// only the last position is scripted because that is the one a decision head is
// trained to read. every other position is derived from its token id, the way an
// id is derived from its text: a client that reads spans of positions can check
// what it got against what it sent, with no fixture to keep in step.
#[derive(Clone)]
pub struct EmbeddingSpec {
    vector: Option<Vec<f64>>,
    error: Option<String>,
    status: u16,
    delay_ms: u64,
}

impl EmbeddingSpec {
    /// the fallback answer: every position derived, none scripted.
    pub fn default_answer() -> EmbeddingSpec {
        EmbeddingSpec {
            vector: None,
            error: None,
            status: 200,
            delay_ms: 0,
        }
    }

    pub fn from_value(v: &Value) -> Result<EmbeddingSpec, String> {
        let vector = match v.get("vector") {
            Some(Value::Array(a)) if !a.is_empty() => Some(
                a.iter()
                    .map(|x| x.as_f64().ok_or("vector holds something that is not a number"))
                    .collect::<Result<Vec<f64>, _>>()?,
            ),
            // a spec with no vector is a fault or a timing spec
            None | Some(Value::Null) => None,
            Some(_) => return Err("vector must be a non-empty array of numbers".to_string()),
        };
        let (error, status) = fault(v);
        Ok(EmbeddingSpec {
            vector,
            error,
            status,
            delay_ms: v.get("delay_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }

    pub fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("vector".to_string(), json!(self.vector));
        m.insert("status".to_string(), json!(self.status));
        m.insert("delay_ms".to_string(), json!(self.delay_ms));
        if let Some(e) = &self.error {
            m.insert("error".to_string(), json!(e));
        }
        Value::Object(m)
    }

    /// the scripted failure, if this spec is one, after its delay.
    pub fn failure(&self) -> Option<HttpResponse> {
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        self.error
            .as_ref()
            .map(|message| HttpResponse::json(self.status, &json!({"error": message})))
    }

    /// a hidden state per token: its id repeated across the width, with the
    /// scripted vector in place of the last one.
    pub fn rows(&self, tokens: &[u32]) -> Vec<Vec<f64>> {
        let width = self.vector.as_ref().map_or(DEFAULT_EMBEDDING_WIDTH, |v| v.len());
        let mut rows: Vec<Vec<f64>> = tokens.iter().map(|id| vec![*id as f64; width]).collect();
        if let (Some(last), Some(vector)) = (rows.last_mut(), &self.vector) {
            *last = vector.clone();
        }
        rows
    }
}

// the fault fields a scripted answer may carry: an error message, and the status
// it is answered with. shared by every spec this family queues.
fn fault(v: &Value) -> (Option<String>, u16) {
    let error = match v.get("error") {
        Some(Value::String(e)) => Some(e.clone()),
        Some(Value::Object(_)) => Some(
            v["error"]
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error")
                .to_string(),
        ),
        Some(_) => Some("upstream error".to_string()),
        None => None,
    };
    let status = v.get("status").and_then(|x| x.as_u64()).unwrap_or(200) as u16;
    // an error without an explicit status is still an error, and answering it
    // with a 200 would hide the very thing the spec exists to inject.
    let status = if error.is_some() && status == 200 {
        500
    } else {
        status
    };
    (error, status)
}

/// append embedding specs from one object or an array of them. all or nothing:
/// a refused spec leaves the queue as it was.
pub fn push_embedding_specs(
    q: &mut std::collections::VecDeque<EmbeddingSpec>,
    body: &Value,
) -> Result<usize, String> {
    let list: Vec<&Value> = match body {
        Value::Array(a) => a.iter().collect(),
        _ => vec![body],
    };
    let specs = list
        .into_iter()
        .map(EmbeddingSpec::from_value)
        .collect::<Result<Vec<_>, _>>()?;
    q.extend(specs);
    Ok(q.len())
}

/// append specs from one object or an array of them, as the chat queue does.
pub fn push_score_specs(q: &mut std::collections::VecDeque<ScoreSpec>, body: &Value) -> Result<usize, String> {
    let list: Vec<&Value> = match body {
        Value::Array(a) => a.iter().collect(),
        _ => vec![body],
    };
    let mut errors = Vec::new();
    for v in list {
        match ScoreSpec::from_value(v) {
            Ok(spec) => q.push_back(spec),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(q.len())
    } else {
        Err(errors.join("; "))
    }
}
