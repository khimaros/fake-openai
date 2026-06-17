// end-to-end tests: spawn the built binary, learn its url from stdout, and drive
// it over raw HTTP/1.1. the binary is consumed cross-language, so its own tests
// exercise it over the wire rather than calling internals.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_fake-openai");
const READ_TIMEOUT: Duration = Duration::from_secs(10);

// the built-in fallback stream returned when the queue is empty and no default
// response is configured.
const FALLBACK_SSE: &str = concat!(
    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
    "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
);

const CHAT_PATH: &str = "/v1/chat/completions";
const SPEECH_PATH: &str = "/v1/audio/speech";
const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
const VOICES_PATH: &str = "/v1/audio/voices";

// a streaming chat request body, optionally tools-bearing and/or a heartbeat.
fn chat_body(tools: bool, heartbeat: bool, stream: bool) -> String {
    let content = if heartbeat {
        "tick [heartbeat] tick"
    } else {
        "hello"
    };
    let tools_field = if tools {
        ",\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"x\"}}]"
    } else {
        ""
    };
    format!(
        "{{\"model\":\"fake-model\",\"messages\":[{{\"role\":\"user\",\"content\":\"{content}\"}}]{tools_field},\"stream\":{stream}}}"
    )
}

struct Server {
    child: Child,
    addr: String, // host:port
    _stdout: BufReader<ChildStdout>,
    stdin: Option<ChildStdin>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    // launch the binary and block until it announces base_url= on stdout.
    fn spawn(args: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fake-openai");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        let addr = loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read stdout") == 0 {
                panic!("fake-openai exited before announcing base_url");
            }
            if let Some(url) = line.trim().strip_prefix("base_url=") {
                break url
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap()
                    .to_string();
            }
        };
        Server {
            stdin: child.stdin.take(),
            child,
            addr,
            _stdout: reader,
        }
    }

    // send one request over a fresh connection; return (status, head, body).
    fn req_full(&self, method: &str, path: &str, body: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let raw = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        stream.write_all(raw.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, head.to_string(), body.to_string())
    }

    fn req(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let (status, _head, body) = self.req_full(method, path, body);
        (status, body)
    }

    fn post(&self, path: &str, body: &str) -> (u16, String) {
        self.req("POST", path, body)
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.req("GET", path, "")
    }

    fn captures(&self) -> Vec<Value> {
        let (_, body) = self.get("/__admin/captures");
        json(&body)["captures"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn pending(&self) -> usize {
        let (_, body) = self.get("/__admin/responses");
        json(&body)["responses"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
    }

    // block until the queue holds at least `want` specs (for the stdin channel,
    // which is processed asynchronously by a reader thread).
    fn wait_pending(&self, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.pending() < want {
            if Instant::now() > deadline {
                panic!("timeout waiting for {want} pending responses");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("bad json: {e}\n{s}"))
}

#[test]
fn fallback_sse_when_queue_empty() {
    let s = Server::spawn(&[]);
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    assert_eq!(body, FALLBACK_SSE);
}

#[test]
fn models_lists_default() {
    let s = Server::spawn(&[]);
    let (status, body) = s.get("/v1/models");
    assert_eq!(status, 200);
    let v = json(&body);
    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], "fake-model");
    assert_eq!(data[0]["object"], "model");
}

#[test]
fn models_configurable_and_introspectable() {
    let s = Server::spawn(&["--models", "alpha,beta"]);
    let (_, body) = s.get("/v1/models");
    let ids: Vec<String> = json(&body)["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["alpha", "beta"]);
    // single-model introspection.
    let (status, body) = s.get("/v1/models/alpha");
    assert_eq!(status, 200);
    assert_eq!(json(&body)["id"], "alpha");
    // unknown model id.
    let (status, _) = s.get("/v1/models/zzz");
    assert_eq!(status, 404);
}

#[test]
fn models_settable_via_admin() {
    let s = Server::spawn(&[]);
    let (status, _) = s.req("PUT", "/__admin/models", "[\"m1\",\"m2\"]");
    assert_eq!(status, 200);
    let (_, body) = s.get("/v1/models");
    assert_eq!(json(&body)["data"].as_array().unwrap().len(), 2);
}

#[test]
fn captures_record_contract() {
    let s = Server::spawn(&[]);
    s.post(CHAT_PATH, &chat_body(true, false, true));
    let caps = s.captures();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["path"], CHAT_PATH);
    assert!(caps[0]["headers"].is_object());
    assert!(caps[0]["body"]["tools"].is_array());
}

#[test]
fn reset_clears_captures_and_rearms_stall() {
    let s = Server::spawn(&["--stall-first-with-tools", "--stall-seconds", "1"]);
    s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(s.captures().len(), 1);
    let (status, _) = s.post("/__admin/reset", "");
    assert_eq!(status, 200);
    assert_eq!(s.captures().len(), 0);
    // stall latch is re-armed: the next tools request stalls again.
    let start = Instant::now();
    s.post(CHAT_PATH, &chat_body(true, false, true));
    assert!(start.elapsed() >= Duration::from_millis(900));
}

#[test]
fn programmed_response_via_admin_then_fallback() {
    let s = Server::spawn(&[]);
    let (status, _) = s.post("/__admin/responses", "{\"content\":\"hi there\"}");
    assert_eq!(status, 200);
    let (_, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(
        body.contains("hi there"),
        "programmed content missing: {body}"
    );
    // queue drained: the next request falls back to the fixed stream.
    let (_, body2) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(body2, FALLBACK_SSE);
}

#[test]
fn programmed_response_non_stream_json() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"content\":\"hi json\"}");
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, false));
    assert_eq!(status, 200);
    let v = json(&body);
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "hi json");
}

#[test]
fn programmed_raw_body_verbatim() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"raw_body\":\"custom-bytes\",\"content_type\":\"text/plain\"}",
    );
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    assert_eq!(body, "custom-bytes");
}

#[test]
fn default_response_overridden_by_flag() {
    let s = Server::spawn(&["--default-response", "{\"content\":\"DFLT\"}"]);
    // empty queue -> the configured default, not the built-in stream.
    let (_, a) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(a.contains("DFLT"), "{a}");
    // a programmed spec still takes precedence over the default.
    s.post("/__admin/responses", "{\"content\":\"queued\"}");
    let (_, b) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(b.contains("queued"), "{b}");
    // queue drained -> back to the configured default.
    let (_, c) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(c.contains("DFLT"), "{c}");
}

#[test]
fn default_response_settable_via_admin() {
    let s = Server::spawn(&[]);
    let (status, _) = s.req("PUT", "/__admin/default", "{\"content\":\"admin-default\"}");
    assert_eq!(status, 200);
    let (_, a) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(a.contains("admin-default"), "{a}");
    // delete -> back to the built-in fallback stream.
    s.req("DELETE", "/__admin/default", "");
    let (_, b) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(b, FALLBACK_SSE);
}

#[test]
fn responses_from_file_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("responses.json");
    std::fs::write(&path, "[{\"content\":\"first\"},{\"content\":\"second\"}]").unwrap();
    let s = Server::spawn(&["--responses", path.to_str().unwrap()]);
    let (_, a) = s.post(CHAT_PATH, &chat_body(false, false, true));
    let (_, b) = s.post(CHAT_PATH, &chat_body(false, false, true));
    let (_, c) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(a.contains("first"), "{a}");
    assert!(b.contains("second"), "{b}");
    assert_eq!(c, FALLBACK_SSE);
}

#[test]
fn responses_from_stdin_in_order() {
    let mut s = Server::spawn(&["--stdin"]);
    {
        let stdin = s.stdin.as_mut().expect("stdin");
        stdin.write_all(b"{\"content\":\"one\"}\n").unwrap();
        stdin.write_all(b"{\"content\":\"two\"}\n").unwrap();
        stdin.flush().unwrap();
    }
    s.wait_pending(2);
    let (_, a) = s.post(CHAT_PATH, &chat_body(false, false, true));
    let (_, b) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(a.contains("one"), "{a}");
    assert!(b.contains("two"), "{b}");
}

#[test]
fn stall_skips_heartbeat_then_stalls_once() {
    let s = Server::spawn(&["--stall-first-with-tools", "--stall-seconds", "1"]);

    // a tools-bearing heartbeat is exempt: not stalled, latch stays armed.
    let start = Instant::now();
    s.post(CHAT_PATH, &chat_body(true, true, true));
    assert!(
        start.elapsed() < Duration::from_millis(900),
        "heartbeat was stalled"
    );

    // first real tools request stalls.
    let start = Instant::now();
    s.post(CHAT_PATH, &chat_body(true, false, true));
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "first tools req not stalled"
    );

    // latch consumed: subsequent tools requests are not stalled.
    let start = Instant::now();
    s.post(CHAT_PATH, &chat_body(true, false, true));
    assert!(
        start.elapsed() < Duration::from_millis(900),
        "stall was not one-shot"
    );
}

#[test]
fn heartbeat_detected_in_list_content() {
    let s = Server::spawn(&["--stall-first-with-tools", "--stall-seconds", "1"]);
    // content as a list of parts with a text field containing the sentinel.
    let body = "{\"model\":\"fake-model\",\"messages\":[{\"role\":\"user\",\"content\":\
                [{\"type\":\"text\",\"text\":\"please [heartbeat] now\"}]}],\
                \"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"x\"}}],\"stream\":true}";
    let start = Instant::now();
    s.post(CHAT_PATH, body);
    assert!(
        start.elapsed() < Duration::from_millis(900),
        "list-content heartbeat was stalled"
    );
}

#[test]
fn behavior_admin_get_and_set() {
    let s = Server::spawn(&[]);
    let (_, body) = s.get("/__admin/behavior");
    assert_eq!(json(&body)["stall_first_with_tools"], false);
    let (status, _) = s.post("/__admin/behavior", "{\"stall_first_with_tools\":true}");
    assert_eq!(status, 200);
    let (_, body) = s.get("/__admin/behavior");
    assert_eq!(json(&body)["stall_first_with_tools"], true);
}

#[test]
fn jsonl_log_file_records_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("captures.jsonl");
    let s = Server::spawn(&["--log-file", path.to_str().unwrap()]);
    s.post(CHAT_PATH, &chat_body(false, false, true));
    // give the server a beat to flush the line, then read it.
    std::thread::sleep(Duration::from_millis(100));
    let logged = std::fs::read_to_string(&path).unwrap();
    let line = logged.lines().next().expect("one jsonl line");
    assert_eq!(json(line)["path"], CHAT_PATH);
}

#[test]
fn admin_requests_are_not_captured() {
    let s = Server::spawn(&[]);
    s.get("/__admin/health");
    s.get("/__admin/captures");
    assert_eq!(s.captures().len(), 0);
}

#[test]
fn consume_only_with_tools_preserves_queue_for_non_tools() {
    let s = Server::spawn(&["--consume-only-with-tools"]);
    s.post("/__admin/responses", "{\"content\":\"tool-turn\"}");
    // a non-tools chat request must not consume the queue; it gets the fallback.
    let (_, a) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(a, FALLBACK_SSE, "non-tools request consumed the queue");
    assert_eq!(
        s.pending(),
        1,
        "queue should be intact after a non-tools request"
    );
    // a tools-bearing heartbeat is exempt as well.
    let (_, b) = s.post(CHAT_PATH, &chat_body(true, true, true));
    assert_eq!(b, FALLBACK_SSE, "heartbeat consumed the queue");
    assert_eq!(s.pending(), 1);
    // a real tools-bearing request consumes the spec.
    let (_, c) = s.post(CHAT_PATH, &chat_body(true, false, true));
    assert!(c.contains("tool-turn"), "{c}");
    assert_eq!(s.pending(), 0);
}

#[test]
fn tts_speech_raw_pcm() {
    let s = Server::spawn(&[]);
    let body =
        "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\"response_format\":\"pcm\"}";
    let (status, head, out) = s.req_full("POST", SPEECH_PATH, body);
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase().contains("content-type: audio/pcm"),
        "{head}"
    );
    assert!(!out.is_empty(), "pcm body empty");
    assert!(!out.contains("speech.audio.delta"), "raw path returned sse");
}

#[test]
fn tts_speech_sse_frames() {
    let s = Server::spawn(&[]);
    let body = "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\
                \"response_format\":\"pcm\",\"stream_format\":\"sse\"}";
    let (status, head, out) = s.req_full("POST", SPEECH_PATH, body);
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    assert!(
        out.contains("event: speech.audio.delta"),
        "missing delta event: {out}"
    );
    // the delta carries a non-empty base64 audio field the client decodes.
    let line = out
        .lines()
        .find(|l| l.starts_with("data: {\"audio\""))
        .expect("audio data line");
    let v = json(line.trim_start_matches("data: "));
    assert!(
        v["audio"].as_str().is_some_and(|a| !a.is_empty()),
        "empty audio: {out}"
    );
}

#[test]
fn stt_transcription_json() {
    let s = Server::spawn(&[]);
    // the real client sends multipart; the mock ignores the body and replies json.
    let (status, body) = s.post(TRANSCRIPTIONS_PATH, "multipart-bytes-here");
    assert_eq!(status, 200);
    assert!(
        json(&body)["text"].as_str().is_some_and(|t| !t.is_empty()),
        "{body}"
    );
}

#[test]
fn audio_voices_list() {
    let s = Server::spawn(&[]);
    let (status, body) = s.get(VOICES_PATH);
    assert_eq!(status, 200);
    assert!(body.contains("alloy"), "{body}");
}

#[test]
fn audio_requests_are_captured() {
    let s = Server::spawn(&[]);
    s.post(SPEECH_PATH, "{\"input\":\"hi\"}");
    s.post(TRANSCRIPTIONS_PATH, "blob");
    let caps = s.captures();
    let paths: Vec<&str> = caps.iter().filter_map(|c| c["path"].as_str()).collect();
    assert!(
        paths.contains(&SPEECH_PATH),
        "speech not captured: {paths:?}"
    );
    assert!(
        paths.contains(&TRANSCRIPTIONS_PATH),
        "stt not captured: {paths:?}"
    );
}

#[test]
fn unknown_get_returns_404_with_endpoints() {
    let s = Server::spawn(&[]);
    // a bare /v1 GET must not list models; it 404s with a discovery listing.
    let (status, body) = s.get("/v1");
    assert_eq!(status, 404);
    let v = json(&body);
    let endpoints = v["endpoints"].as_array().expect("endpoints array");
    assert!(
        endpoints
            .iter()
            .any(|e| e.as_str() == Some("GET /v1/models")),
        "endpoints should advertise /v1/models: {body}"
    );
    // the real models path still works.
    let (status, _) = s.get("/v1/models");
    assert_eq!(status, 200);
}

// ---- v0.3: multi-chunk streaming, fault injection, timing, validation ----

impl Server {
    // POST carrying an Authorization: Bearer header, for the validation tests.
    fn post_auth(&self, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer test\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        stream.write_all(raw.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, body.to_string())
    }
}

#[test]
fn chunks_render_one_delta_each_streaming() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"chunks\":[\"foo\",\"bar\"]}");
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    // two separate content deltas, in order, then the terminating frames.
    let foo = body.find("\"content\":\"foo\"").expect("foo delta");
    let bar = body.find("\"content\":\"bar\"").expect("bar delta");
    assert!(foo < bar, "deltas out of order: {body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.contains("data: [DONE]"), "{body}");
}

#[test]
fn chunks_join_in_non_stream_message() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"chunks\":[\"foo\",\"bar\"]}");
    let (_, body) = s.post(CHAT_PATH, &chat_body(false, false, false));
    assert_eq!(json(&body)["choices"][0]["message"]["content"], "foobar");
}

#[test]
fn error_spec_returns_openai_error_body() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"error\":{\"message\":\"boom\"},\"status\":429}",
    );
    // even a streaming request gets a json error: the failure precedes the stream.
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 429);
    let v = json(&body);
    assert_eq!(v["error"]["message"], "boom");
    assert_eq!(v["error"]["type"], "invalid_request_error");
}

#[test]
fn error_string_shorthand_defaults_to_400() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"error\":\"nope\"}");
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, false));
    assert_eq!(status, 400);
    assert_eq!(json(&body)["error"]["message"], "nope");
}

#[test]
fn truncate_omits_finish_and_done() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"content\":\"partial\",\"truncate\":true}",
    );
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    assert!(body.contains("partial"), "{body}");
    assert!(
        !body.contains("[DONE]"),
        "truncated stream sent [DONE]: {body}"
    );
    assert!(!body.contains("\"finish_reason\":\"stop\""), "{body}");
}

#[test]
fn chunk_delay_paces_streamed_deltas() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"chunks\":[\"a\",\"b\",\"c\"],\"chunk_delay_ms\":200}",
    );
    let start = Instant::now();
    let (_, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    // two inter-delta gaps of 200ms between three chunks.
    assert!(
        start.elapsed() >= Duration::from_millis(380),
        "not paced: {:?}",
        start.elapsed()
    );
    assert!(body.contains("\"content\":\"c\""), "{body}");
}

#[test]
fn hang_holds_connection_then_closes() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"content\":\"x\",\"hang_ms\":400,\"truncate\":true}",
    );
    let start = Instant::now();
    let (_, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(
        start.elapsed() >= Duration::from_millis(380),
        "did not hang: {:?}",
        start.elapsed()
    );
    assert!(body.contains('x'), "{body}");
    assert!(!body.contains("[DONE]"), "{body}");
}

#[test]
fn connect_delay_applies_to_requests_not_admin() {
    let s = Server::spawn(&["--connect-delay-ms", "300"]);
    // admin is the control plane and must never be delayed.
    let start = Instant::now();
    s.get("/__admin/health");
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "admin was delayed"
    );
    // a chat request pays the connect delay.
    let start = Instant::now();
    s.post(CHAT_PATH, &chat_body(false, false, true));
    assert!(
        start.elapsed() >= Duration::from_millis(280),
        "no connect delay: {:?}",
        start.elapsed()
    );
}

#[test]
fn connect_delay_settable_via_admin_behavior() {
    let s = Server::spawn(&[]);
    s.post("/__admin/behavior", "{\"connect_delay_ms\":250}");
    let (_, body) = s.get("/__admin/behavior");
    assert_eq!(json(&body)["connect_delay_ms"], 250);
}

#[test]
fn validate_chat_rejects_missing_model() {
    let s = Server::spawn(&["--validate-chat"]);
    let body = "{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"stream\":false}";
    let (status, out) = s.post_auth(CHAT_PATH, body);
    assert_eq!(status, 400);
    assert_eq!(json(&out)["error"]["type"], "invalid_request_error");
}

#[test]
fn validate_chat_rejects_missing_messages() {
    let s = Server::spawn(&["--validate-chat"]);
    let (status, _) = s.post_auth(CHAT_PATH, "{\"model\":\"fake-model\",\"stream\":false}");
    assert_eq!(status, 400);
}

#[test]
fn validate_chat_requires_bearer_auth() {
    let s = Server::spawn(&["--validate-chat"]);
    // the default helper sends no Authorization header.
    let (status, out) = s.post(CHAT_PATH, &chat_body(false, false, false));
    assert_eq!(status, 401);
    assert_eq!(json(&out)["error"]["type"], "invalid_request_error");
}

#[test]
fn validate_chat_accepts_well_formed_request() {
    let s = Server::spawn(&["--validate-chat"]);
    s.post("/__admin/responses", "{\"content\":\"ok-valid\"}");
    let (status, out) = s.post_auth(CHAT_PATH, &chat_body(false, false, false));
    assert_eq!(status, 200, "{out}");
    assert_eq!(json(&out)["choices"][0]["message"]["content"], "ok-valid");
}

#[test]
fn validation_off_by_default() {
    let s = Server::spawn(&[]);
    // no auth, no model, no messages: still accepted under the permissive default.
    let (status, _) = s.post(CHAT_PATH, "{\"stream\":false}");
    assert_eq!(status, 200);
}

#[test]
fn tool_calls_render_in_stream_and_json() {
    let s = Server::spawn(&[]);
    let tc = "{\"tool_calls\":[{\"id\":\"c1\",\"type\":\"function\",\"index\":0,\
              \"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}],\"finish_reason\":\"tool_calls\"}";
    // streaming: the delta carries the tool_calls and the finish reason.
    s.post("/__admin/responses", tc);
    let (_, stream) = s.post(CHAT_PATH, &chat_body(true, false, true));
    assert!(stream.contains("tool_calls"), "{stream}");
    assert!(
        stream.contains("\"finish_reason\":\"tool_calls\""),
        "{stream}"
    );
    // non-streaming: the message carries the tool_calls.
    s.post("/__admin/responses", tc);
    let (_, body) = s.post(CHAT_PATH, &chat_body(true, false, false));
    let v = json(&body);
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
}
