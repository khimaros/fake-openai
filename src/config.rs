// command-line configuration. flags are hand-parsed to avoid a parser
// dependency for this small surface.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::behavior::Behavior;

pub const DEFAULT_HOST: &str = "127.0.0.1";
// the model name reported by /v1/models (default) and echoed in responses.
// override the /v1/models listing with --models.
pub const DEFAULT_MODEL: &str = "fake-model";
const DEFAULT_OWNER: &str = "fake-openai";

const FLAG_HOST: &str = "--host";
const FLAG_PORT: &str = "--port";
const FLAG_RESPONSES: &str = "--responses";
const FLAG_DEFAULT_RESPONSE: &str = "--default-response";
const FLAG_STDIN: &str = "--stdin";
const FLAG_LOG_FILE: &str = "--log-file";
const FLAG_LOG_STDOUT: &str = "--log-stdout";
const FLAG_MODELS: &str = "--models";
const FLAG_MODELS_JSON: &str = "--models-json";
const FLAG_STALL_TOOLS: &str = "--stall-first-with-tools";
const FLAG_STALL_SECONDS: &str = "--stall-seconds";
const FLAG_HEARTBEAT: &str = "--heartbeat-sentinel";
const FLAG_CONSUME_TOOLS: &str = "--consume-only-with-tools";
const FLAG_CONNECT_DELAY: &str = "--connect-delay-ms";
const FLAG_VALIDATE_CHAT: &str = "--validate-chat";
const FLAG_CONTEXT_WINDOW: &str = "--context-window";
const FLAG_CHARS_PER_TOKEN: &str = "--chars-per-token";
const FLAG_LLAMASWAP: &str = "--llamaswap";
const FLAG_EMPTY_TRANSCRIPT: &str = "--empty-transcript";
const FLAG_SPEECH_FILE: &str = "--speech-file";

pub struct Config {
    pub host: String,
    pub port: u16,
    pub responses_file: Option<PathBuf>,
    // overrides the built-in fallback returned when the queue is empty.
    pub default_response: Option<Value>,
    pub stdin: bool,
    pub log_file: Option<PathBuf>,
    pub log_stdout: bool,
    pub models: Vec<Value>,
    pub behavior: Behavior,
    /// A wav (or raw s16le pcm) served by `/v1/audio/speech` INSTEAD of the built-in ramp.
    ///
    /// The ramp stays the default and must: it carries POSITION, so a consumer can prove the audio
    /// it received is the audio that was sent, unshifted and untruncated. What it cannot do is
    /// sound like a voice, and a test that wants to know what a real reply does to a real client's
    /// audio path -- jitter buffer, resampler, playout -- needs speech, end to end, with nothing
    /// injected into the client. Point this at a fixture for that.
    pub speech_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            host: DEFAULT_HOST.to_string(),
            port: 0,
            responses_file: None,
            default_response: None,
            stdin: false,
            log_file: None,
            log_stdout: false,
            speech_file: None,
            models: parse_models(DEFAULT_MODEL),
            behavior: Behavior::default(),
        }
    }
}

pub fn parse_args(args: Vec<String>) -> Result<Config, String> {
    let mut c = Config::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match key.as_str() {
            FLAG_HOST => c.host = take(&mut it, inline, FLAG_HOST)?,
            FLAG_PORT => c.port = parse_num(&take(&mut it, inline, FLAG_PORT)?, FLAG_PORT)?,
            FLAG_RESPONSES => {
                c.responses_file = Some(take(&mut it, inline, FLAG_RESPONSES)?.into())
            }
            FLAG_DEFAULT_RESPONSE => {
                let raw = take(&mut it, inline, FLAG_DEFAULT_RESPONSE)?;
                c.default_response = Some(parse_json(&raw, FLAG_DEFAULT_RESPONSE)?);
            }
            FLAG_STDIN => c.stdin = true,
            FLAG_LOG_FILE => c.log_file = Some(take(&mut it, inline, FLAG_LOG_FILE)?.into()),
            FLAG_LOG_STDOUT => c.log_stdout = true,
            FLAG_MODELS => c.models = parse_models(&take(&mut it, inline, FLAG_MODELS)?),
            FLAG_MODELS_JSON => {
                let raw = take(&mut it, inline, FLAG_MODELS_JSON)?;
                let parsed = parse_json(&raw, FLAG_MODELS_JSON)?;
                let list = parsed
                    .as_array()
                    .ok_or_else(|| format!("{FLAG_MODELS_JSON}: expected a json array"))?;
                c.models = list.iter().filter_map(normalize_model).collect();
            }
            FLAG_STALL_TOOLS => c.behavior.stall_first_with_tools = true,
            FLAG_STALL_SECONDS => {
                c.behavior.stall_seconds = parse_num(
                    &take(&mut it, inline, FLAG_STALL_SECONDS)?,
                    FLAG_STALL_SECONDS,
                )?
            }
            FLAG_HEARTBEAT => {
                c.behavior.heartbeat_sentinel = take(&mut it, inline, FLAG_HEARTBEAT)?
            }
            FLAG_CONSUME_TOOLS => c.behavior.consume_only_with_tools = true,
            FLAG_CONNECT_DELAY => {
                c.behavior.connect_delay_ms = parse_num(
                    &take(&mut it, inline, FLAG_CONNECT_DELAY)?,
                    FLAG_CONNECT_DELAY,
                )?
            }
            FLAG_VALIDATE_CHAT => c.behavior.validate_chat = true,
            FLAG_CONTEXT_WINDOW => {
                c.behavior.context_window = Some(parse_num(
                    &take(&mut it, inline, FLAG_CONTEXT_WINDOW)?,
                    FLAG_CONTEXT_WINDOW,
                )?)
            }
            FLAG_CHARS_PER_TOKEN => {
                c.behavior.chars_per_token = parse_num(
                    &take(&mut it, inline, FLAG_CHARS_PER_TOKEN)?,
                    FLAG_CHARS_PER_TOKEN,
                )?
            }
            FLAG_LLAMASWAP => c.behavior.llamaswap = true,
            FLAG_EMPTY_TRANSCRIPT => c.behavior.empty_transcript = true,
            FLAG_SPEECH_FILE => {
                c.speech_file = Some(take(&mut it, inline, FLAG_SPEECH_FILE)?.into())
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(c)
}

fn take(
    it: &mut std::vec::IntoIter<String>,
    inline: Option<String>,
    flag: &str,
) -> Result<String, String> {
    inline
        .or_else(|| it.next())
        .ok_or_else(|| format!("{flag}: requires a value"))
}

fn parse_num<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("{flag}: invalid number '{s}'"))
}

fn parse_json(s: &str, flag: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| format!("{flag}: invalid json: {e}"))
}

fn parse_models(s: &str) -> Vec<Value> {
    s.split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .filter_map(|x| normalize_model(&Value::String(x.to_string())))
        .collect()
}

// a bare string becomes a minimal model object; an object carrying an `id` is
// kept verbatim so callers can mirror what a real server reports (llama-swap's
// `meta.llamaswap` modality and alias fields, say), with the standard openai
// fields filled in where absent.
pub fn normalize_model(v: &Value) -> Option<Value> {
    let mut model = match v {
        Value::String(s) => json!({"id": s}),
        Value::Object(_) => v.clone(),
        _ => return None,
    };
    let map = model.as_object_mut()?;
    if !map.get("id").is_some_and(|id| id.is_string()) {
        return None;
    }
    map.entry("object").or_insert(json!("model"));
    map.entry("created").or_insert(json!(0));
    map.entry("owned_by").or_insert(json!(DEFAULT_OWNER));
    Some(model)
}

pub fn usage() -> String {
    let mut u = String::new();
    u.push_str("fake-openai - standalone programmable openai-compatible mock\n\n");
    u.push_str("usage: fake-openai [flags]\n\n");
    u.push_str("flags:\n");
    u.push_str("  --host <addr>             bind host (default 127.0.0.1)\n");
    u.push_str("  --port <n>                bind port, 0 = free port (default 0)\n");
    u.push_str("  --responses <path>        load response specs (json array/object or jsonl)\n");
    u.push_str(
        "  --default-response <json> response when the queue is empty (default: ok stream)\n",
    );
    u.push_str("  --stdin                   read response specs as jsonl from stdin\n");
    u.push_str(
        "  --models <a,b,...>        model ids reported by /v1/models (default fake-model)\n",
    );
    u.push_str(
        "  --models-json <json>      model objects for /v1/models, with metadata (array)\n",
    );
    u.push_str("  --log-file <path>         append captured requests as jsonl\n");
    u.push_str("  --log-stdout              emit captured requests as jsonl to stdout\n");
    u.push_str("  --stall-first-with-tools  stall the first tools-bearing request\n");
    u.push_str("  --stall-seconds <n>       stall duration in seconds (default 5)\n");
    u.push_str(
        "  --heartbeat-sentinel <s>  sentinel that exempts heartbeats (default [heartbeat])\n",
    );
    u.push_str("  --consume-only-with-tools only tools-bearing chat requests pop the queue\n");
    u.push_str("  --connect-delay-ms <n>    delay every non-admin request before any bytes\n");
    u.push_str("  --validate-chat           reject malformed/unauthenticated chat requests\n");
    u.push_str(
        "  --context-window <n>      derive usage from request size; 400 when it exceeds n\n",
    );
    u.push_str(
        "  --chars-per-token <n>     characters per token for the derived usage (default 4)\n",
    );
    u.push_str(
        "  --llamaswap               serve llama-swap's /upstream/<model>/v1/* and aliases\n",
    );
    u.push_str(
        "  --empty-transcript        transcriptions answer {\"text\": \"\"} (a silent clip)\n",
    );
    u.push_str(
        "  --speech-file <path>      /v1/audio/speech serves this wav/pcm instead of the ramp,\n\
         \x20                         so a client receives real SPEECH over the real path\n",
    );
    u.push_str("  --help                    show this help\n\n");
    u.push_str("on startup prints: base_url=http://<host>:<port>/v1\n");
    u
}
