// configurable mock behaviors: the heartbeat predicate and the one-shot stall.
// ported from hcp-spec/testing/mock_openai.py.

use serde_json::Value;

pub const HEARTBEAT_SENTINEL: &str = "[heartbeat]";
pub const DEFAULT_STALL_SECONDS: u64 = 5;

#[derive(Clone)]
pub struct Behavior {
    pub stall_first_with_tools: bool,
    pub stall_seconds: u64,
    pub heartbeat_sentinel: String,
    // when set, only tools-bearing non-heartbeat chat requests pop the queue;
    // everything else gets the fallback. lets callers replay a sequence keyed to
    // the tool-call turns while ignoring title/heartbeat traffic in between.
    pub consume_only_with_tools: bool,
    // a fixed delay applied to every non-admin request before any response
    // bytes, to exercise a client's slow-first-byte / connect timeout handling.
    pub connect_delay_ms: u64,
    // when set, malformed or unauthenticated chat requests are rejected with an
    // openai-style 400/401 instead of being answered permissively.
    pub validate_chat: bool,
}

impl Default for Behavior {
    fn default() -> Behavior {
        Behavior {
            stall_first_with_tools: false,
            stall_seconds: DEFAULT_STALL_SECONDS,
            heartbeat_sentinel: HEARTBEAT_SENTINEL.to_string(),
            consume_only_with_tools: false,
            connect_delay_ms: 0,
            validate_chat: false,
        }
    }
}

// when validate_chat is on, reject a malformed or unauthenticated chat request.
// returns (status, message) on rejection, or None when the request is acceptable.
pub fn validate_chat_request(headers: &Value, body: &Value) -> Option<(u16, &'static str)> {
    if !bearer_present(headers) {
        return Some((401, "missing or malformed Authorization bearer token"));
    }
    if non_empty_str(body, "model").is_none() {
        return Some((400, "missing required parameter: model"));
    }
    if body
        .get("messages")
        .and_then(|m| m.as_array())
        .filter(|a| !a.is_empty())
        .is_none()
    {
        return Some((400, "missing required parameter: messages"));
    }
    None
}

// true when an Authorization header carries a bearer token (case-insensitive).
fn bearer_present(headers: &Value) -> bool {
    headers.as_object().is_some_and(|m| {
        m.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("authorization")
                && v.as_str()
                    .is_some_and(|s| s.trim_start().starts_with("Bearer "))
        })
    })
}

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
}

// true when any message content (a string, or a list of parts with a "text"
// field) contains the sentinel. heartbeats fire on a timer inside the harness
// and must be distinguishable from build/title requests.
pub fn is_heartbeat(body: &Value, sentinel: &str) -> bool {
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return false,
    };
    for m in messages {
        match m.get("content") {
            Some(Value::String(s)) if s.contains(sentinel) => return true,
            Some(Value::Array(parts)) => {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        if t.contains(sentinel) {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

// true when the request carries a non-empty tools array.
pub fn has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty())
}

// a tools-bearing, non-heartbeat chat request: the "real build turn" both the
// stall and the consume-only-with-tools gate key on.
fn is_tool_turn(b: &Behavior, path: &str, body: &Value) -> bool {
    path.contains("chat/completions")
        && has_tools(body)
        && !is_heartbeat(body, &b.heartbeat_sentinel)
}

// stall the first tool turn once. the latch is reset by /__admin/reset. keeps a
// harness alive long enough for a timer-driven heartbeat tick to fire.
pub fn should_stall(b: &Behavior, latch: &mut bool, path: &str, body: &Value) -> bool {
    if !b.stall_first_with_tools || *latch {
        return false;
    }
    if is_tool_turn(b, path, body) {
        *latch = true;
        return true;
    }
    false
}

// whether this request may pop a programmed response. with consume_only_with_tools
// off, every chat request consumes; with it on, only tool turns do.
pub fn should_consume(b: &Behavior, path: &str, body: &Value) -> bool {
    !b.consume_only_with_tools || is_tool_turn(b, path, body)
}
