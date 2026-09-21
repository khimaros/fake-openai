// configurable mock behaviors: the heartbeat predicate and the one-shot stall.
// ported from hcp-spec/testing/mock_openai.py.

use serde_json::Value;

pub const HEARTBEAT_SENTINEL: &str = "[heartbeat]";
pub const DEFAULT_STALL_SECONDS: u64 = 5;
// characters per token for the derived-usage estimate. openai-family tokenizers
// land near four characters per token for prose; a caller tunes it per content
// (dense code/base64 packs closer to two) with --chars-per-token.
pub const DEFAULT_CHARS_PER_TOKEN: u64 = 4;

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
    // when set, a transcription request whose audio is silent is rejected instead of answered
    // with the fixed transcript. off by default: every existing consumer relies on that
    // transcript, and a stub that answers anything is only a problem for a caller that wants to
    // prove its capture path works.
    pub reject_silent_audio: bool,
    // when set, speaking as a voice other than the preset is rejected unless the
    // request carries a `consent_attestation`, the way crispasr-style servers gate
    // cloned-voice synthesis. ENROLMENT IS NOT THE ONLY GATE there, and a client
    // that sends the field only at upload time clears the first and fails this
    // one. off by default: it is a vendor extension a plain openai client never
    // sends, and leaving it on would reject the traffic every other test here has.
    pub require_voice_consent: bool,
    // when set, the reported usage is derived from the request size and a request
    // whose derived prompt exceeds this window is rejected with a context-overflow
    // 400. this closes the feedback loop a harness's compaction depends on: a
    // compacted (smaller) request reports a smaller usage, which is what a
    // context-usage trigger reads. a fixed per-turn usage cannot model that.
    pub context_window: Option<u64>,
    pub chars_per_token: u64,
    // when set, serve llama-swap's non-standard surface: the
    // /upstream/<model>/v1/* routes a client falls back to, and model lookup by
    // an advertised meta.llamaswap alias. off by default so the mock answers as
    // a plain openai-compatible server unless a caller opts in.
    pub llamaswap: bool,
    // how long to hold between speech.audio.delta frames, in ms. 0 (the default) answers the whole
    // reply in one write, which is the fast, deterministic thing a bench usually wants.
    //
    // A REPLY THAT ARRIVES INSTANTLY CANNOT STARVE A CLIENT, and starving one is a behaviour worth
    // testing: the middle of a real reply arrives over a link, and a jitter buffer that only ever
    // gates the START of one dribbles the rest out in fragment-sized bursts. that defect is
    // unreachable from a mock that has already sent everything before the first sample is played.
    pub speech_frame_delay_ms: u64,
    // when set, transcriptions answer with an EMPTY transcript. a real transcriber does this
    // whenever it hears no speech (whisper's vad filtering a silent clip), and it is not an
    // error -- so a consumer that only handles "text" or "failure" hangs on it.
    pub empty_transcript: bool,
    // what transcriptions answer with; None keeps the built-in fixed string.
    //
    // SETTABLE AT RUNTIME because a scenario is a CONVERSATION: a harness that plays two different
    // utterances into a microphone and gets the same words back for both cannot script a flow, and
    // a mock that transcribes speech into something unrelated to it makes every recording of the
    // exchange nonsense to listen to.
    pub transcript: Option<String>,
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
            reject_silent_audio: false,
            require_voice_consent: false,
            context_window: None,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            llamaswap: false,
            empty_transcript: false,
            speech_frame_delay_ms: 0,
            transcript: None,
        }
    }
}

// estimate a chat request's prompt tokens from its serialized messages, at
// chars_per_token characters each. coarse, but it tracks how a request grows or
// shrinks turn to turn -- the only property the context-window loop needs.
pub fn estimate_prompt_tokens(body: &Value, chars_per_token: u64) -> u64 {
    let cpt = chars_per_token.max(1);
    let chars = body
        .get("messages")
        .map(|m| m.to_string().len() as u64)
        .unwrap_or(0);
    chars.div_ceil(cpt)
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
