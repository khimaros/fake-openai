// command-line configuration. flags are hand-parsed to avoid a parser
// dependency for this small surface.

use std::path::PathBuf;

use serde_json::Value;

use crate::behavior::Behavior;

pub const DEFAULT_HOST: &str = "127.0.0.1";
// the model name reported by /v1/models (default) and echoed in responses.
// override the /v1/models listing with --models.
pub const DEFAULT_MODEL: &str = "fake-model";

const FLAG_HOST: &str = "--host";
const FLAG_PORT: &str = "--port";
const FLAG_RESPONSES: &str = "--responses";
const FLAG_DEFAULT_RESPONSE: &str = "--default-response";
const FLAG_STDIN: &str = "--stdin";
const FLAG_LOG_FILE: &str = "--log-file";
const FLAG_LOG_STDOUT: &str = "--log-stdout";
const FLAG_MODELS: &str = "--models";
const FLAG_STALL_TOOLS: &str = "--stall-first-with-tools";
const FLAG_STALL_SECONDS: &str = "--stall-seconds";
const FLAG_HEARTBEAT: &str = "--heartbeat-sentinel";
const FLAG_CONSUME_TOOLS: &str = "--consume-only-with-tools";
const FLAG_CONNECT_DELAY: &str = "--connect-delay-ms";
const FLAG_VALIDATE_CHAT: &str = "--validate-chat";

pub struct Config {
    pub host: String,
    pub port: u16,
    pub responses_file: Option<PathBuf>,
    // overrides the built-in fallback returned when the queue is empty.
    pub default_response: Option<Value>,
    pub stdin: bool,
    pub log_file: Option<PathBuf>,
    pub log_stdout: bool,
    pub models: Vec<String>,
    pub behavior: Behavior,
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
            models: vec![DEFAULT_MODEL.to_string()],
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

fn parse_models(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
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
    u.push_str("  --help                    show this help\n\n");
    u.push_str("on startup prints: base_url=http://<host>:<port>/v1\n");
    u
}
