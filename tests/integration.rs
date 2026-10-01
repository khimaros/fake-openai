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
const REGISTRY_VOICES_PATH: &str = "/v1/voices";
const FLAG_REQUIRE_VOICE_CONSENT: &str = "--require-voice-consent";
const DEFAULT_VOICE: &str = "alloy";
const IMAGES_PATH: &str = "/v1/images/generations";

// llama.cpp's NATIVE endpoints, which its own server serves at the root and
// llama-swap fronts at /upstream/<model>/. served only under --llamacpp.
const PROPS_PATH: &str = "/props";
const TOKENIZE_PATH: &str = "/tokenize";
const COMPLETION_PATH: &str = "/completion";
const FLAG_LLAMACPP: &str = "--llamacpp";
const FLAG_CHAT_TEMPLATE: &str = "--chat-template";
const LLAMA_PROBS_PATH: &str = "/__admin/probs";
// the byte-level vocabulary: 256 bytes plus the two specials a client reads
// from /props, so a token id is derivable from the text that produced it.
const VOCAB: u64 = 258;
const BOS_ID: u64 = 256;
const TEMPLATE: &str = "{% for m in messages %}{{ m.role }}\n{{ m.content }}\n{% endfor %}";

// a scoring request in the shape llama.cpp's /completion takes.
fn completion_body(prompt: &str, n_probs: u64) -> String {
    format!(
        "{{\"prompt\":\"{prompt}\",\"n_predict\":1,\"n_probs\":{n_probs},\
         \"cache_prompt\":true,\"post_sampling_probs\":false,\"temperature\":-1.0}}"
    )
}

// a token id array as numbers, for asserting on byte-level tokenization.
fn ids(v: &Value) -> Vec<u64> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.as_u64())
        .collect()
}

// the scored distribution out of a /completion response, in the order given.
fn entries(body: &str) -> Vec<Value> {
    json(body)["completion_probabilities"][0]["top_logprobs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn approx(v: f64, want: f64) -> bool {
    (v - want).abs() < 1e-6
}

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

    // send one request over a fresh connection; return (status, head, body). the body is
    // decoded LOSSILY -- the audio endpoints serve payloads that are not valid utf-8, and a
    // strict decode here would fail every test that merely posts to one. a test that inspects
    // audio bytes uses req_bytes instead.
    fn req_full(&self, method: &str, path: &str, body: &str) -> (u16, String, String) {
        let (status, head, bytes) = self.req_bytes(method, path, body);
        (status, head, String::from_utf8_lossy(&bytes).into_owned())
    }

    // as req_full, but the response body stays BYTES.
    fn req_bytes(&self, method: &str, path: &str, body: &str) -> (u16, String, Vec<u8>) {
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
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).unwrap();
        let split = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, head, resp[split + 4..].to_vec())
    }

    fn req(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let (status, _head, body) = self.req_full(method, path, body);
        (status, body)
    }

    fn post(&self, path: &str, body: &str) -> (u16, String) {
        self.req("POST", path, body)
    }

    // POST a body that is BYTES. audio is binary; building it as a string first mangles every
    // non-utf8 sample, which is the very thing the audio measurement has to see through.
    fn post_bytes(&self, path: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: multipart/form-data; \
             boundary=b\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr,
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).unwrap();
        let split = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        let headers = String::from_utf8_lossy(&resp[..split]).into_owned();
        let status = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, String::from_utf8_lossy(&resp[split + 4..]).into_owned())
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
fn programmed_reasoning_streams_reasoning_content() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"reasoning\":\"thinking\",\"content\":\"answer\"}");
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    // a reasoning delta (reasoning_content) precedes the visible content delta.
    assert!(body.contains("reasoning_content"), "no reasoning delta: {body}");
    assert!(body.contains("thinking"), "reasoning text missing: {body}");
    assert!(body.contains("answer"), "content missing: {body}");
    let rpos = body.find("reasoning_content").unwrap();
    let cpos = body.find("\"content\":\"answer\"").unwrap();
    assert!(rpos < cpos, "reasoning must stream before content: {body}");
}

#[test]
fn programmed_reasoning_non_stream_json() {
    let s = Server::spawn(&[]);
    s.post("/__admin/responses", "{\"reasoning\":\"thinking\",\"content\":\"answer\"}");
    let (_, body) = s.post(CHAT_PATH, &chat_body(false, false, false));
    let v = json(&body);
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "thinking");
    assert_eq!(v["choices"][0]["message"]["content"], "answer");
}

#[test]
fn programmed_reasoning_chunks_stream_one_delta_each() {
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"reasoning_chunks\":[\"think \",\"more \",\"now\"],\"chunks\":[\"hel\",\"lo\"]}",
    );
    let (status, body) = s.post(CHAT_PATH, &chat_body(false, false, true));
    assert_eq!(status, 200);
    // one reasoning_content delta per reasoning chunk (incremental thinking).
    assert_eq!(
        body.matches("reasoning_content").count(),
        3,
        "expected 3 reasoning deltas: {body}"
    );
    // and the non-stream concatenation joins them (a second spec for the new request).
    s.post(
        "/__admin/responses",
        "{\"reasoning_chunks\":[\"think \",\"more \",\"now\"],\"chunks\":[\"hel\",\"lo\"]}",
    );
    let (_, jbody) = s.post(CHAT_PATH, &chat_body(false, false, false));
    let v = json(&jbody);
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "think more now");
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
    // reads BYTES: the pcm payload is not valid utf-8 (see req_bytes).
    let (status, head, out) = s.req_bytes("POST", SPEECH_PATH, body);
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase().contains("content-type: audio/pcm"),
        "{head}"
    );
    assert!(!out.is_empty(), "pcm body empty");
    assert!(
        !out.windows(18).any(|w| w == b"speech.audio.delta"),
        "raw path returned sse"
    );
}

// the ramp the speech endpoint serves, mirrored here so the test states the contract rather
// than reading it out of the implementation.
fn expected_pcm(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// silence proves only that SOMETHING arrived. a positional payload proves the audio a consumer
// received is the audio that was sent -- in order, unshifted, untruncated.
#[test]
fn tts_speech_pcm_is_positional_not_silence() {
    let s = Server::spawn(&[]);
    let body =
        "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\"response_format\":\"pcm\"}";
    let (status, head, pcm) = s.req_bytes("POST", SPEECH_PATH, body);
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase().contains("content-type: audio/pcm"),
        "{head}"
    );
    assert!(!pcm.is_empty(), "pcm body empty");
    assert_eq!(pcm, expected_pcm(pcm.len()), "pcm is not the expected ramp");

    // the two hazards this exists to catch. the period is coprime with the 2-byte s16_le sample
    // width, so a single-byte shift cannot coincidentally re-align, and a truncated stream
    // cannot pass by being a prefix of silence.
    assert_ne!(pcm[1..], expected_pcm(pcm.len())[..pcm.len() - 1],
        "a one-byte shift must be detectable");
    assert_ne!(pcm.len(), 0);
    assert!(pcm.iter().any(|b| *b != 0), "payload is still silence");
}

// both transports must deliver the SAME audio: a consumer that decodes sse and one that reads
// the raw body should be indistinguishable downstream.
#[test]
fn tts_speech_sse_audio_matches_the_raw_body() {
    let s = Server::spawn(&[]);
    let raw_body =
        "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\"response_format\":\"pcm\"}";
    let (_, _, raw) = s.req_bytes("POST", SPEECH_PATH, raw_body);

    let sse_body = "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\
                    \"response_format\":\"pcm\",\"stream_format\":\"sse\"}";
    let (_, _, out) = s.req_full("POST", SPEECH_PATH, sse_body);
    let line = out
        .lines()
        .find(|l| l.starts_with("data: {\"audio\""))
        .expect("audio data line");
    let encoded = json(line.trim_start_matches("data: "))["audio"]
        .as_str()
        .expect("audio field")
        .to_string();
    assert_eq!(decode_b64(&encoded), raw, "sse audio differs from raw pcm");
}

// a minimal base64 decoder: the test suite carries no dependencies, and asserting on the
// DECODED bytes is the whole point -- comparing the encoded string would not catch a payload
// that encodes cleanly but decodes to the wrong samples.
fn decode_b64(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = ALPHABET.iter().position(|a| *a == c).expect("base64 char") as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
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
fn tts_speech_sse_can_be_paced_so_a_reply_arrives_over_time() {
    // A REPLY THAT ARRIVES INSTANTLY CANNOT STARVE A CLIENT, and the default answers the whole
    // thing in one write. that is the right default -- fast and deterministic -- but it makes one
    // real behaviour unreachable: a jitter buffer that gates only the START of a reply is correct
    // for every reply that is already in hand before the first sample plays. the middle of a real
    // reply arrives over a link, and pacing is what lets a bench ask about it.
    let s = Server::spawn(&[]);
    let body = "{\"input\":\"hi\",\"model\":\"tts-1\",\"voice\":\"alloy\",\
                \"response_format\":\"pcm\",\"stream_format\":\"sse\"}";

    let at = Instant::now();
    let (status, _, unpaced) = s.req_full("POST", SPEECH_PATH, body);
    let quick = at.elapsed();
    assert_eq!(status, 200);

    let (status, _) = s.post("/__admin/behavior", "{\"speech_frame_delay_ms\":250}");
    assert_eq!(status, 200);
    let at = Instant::now();
    let (status, head, paced) = s.req_full("POST", SPEECH_PATH, body);
    let slow = at.elapsed();

    assert_eq!(status, 200);
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    assert!(
        slow >= Duration::from_millis(400),
        "the frames were not held apart: paced took {slow:?}, unpaced {quick:?}"
    );
    // SAME STREAM, JUST SLOWER. pacing that changed the audio would make every measurement taken
    // through it a measurement of the mock. compared by DATA LINE rather than by raw body: a
    // paced response is chunked transfer, so its bytes carry frame sizes the buffered one has not.
    let audio = |s: &str| {
        s.lines()
            .filter(|l| l.starts_with("data: {\"audio\""))
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        audio(&paced),
        audio(&unpaced),
        "a paced reply must carry the same audio as an unpaced one"
    );
    assert!(!audio(&paced).is_empty(), "no audio at all: {paced}");
    assert!(
        paced.contains("[DONE]"),
        "a paced stream must still terminate: {paced}"
    );
}

// write a raw pcm fixture whose bytes name it, so a test can tell WHICH one came back.
fn pcm_fixture(name: &str, fill: u8) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("fake-openai-{name}-{fill}.pcm"));
    std::fs::write(&path, vec![fill; 64]).expect("write fixture");
    path
}

// ONE FIXTURE FOR EVERY UTTERANCE MAKES A MULTI-BLOCK TURN UNREADABLE. an agent that speaks three
// times over one turn plays the same clip three times, and nothing in a recording of it
// distinguishes "said something new" from "replayed the last thing". the fixture is chosen by the
// TEXT so the choice is deterministic and order-independent -- a round-robin would re-voice the
// whole scenario the moment one extra sentence appeared anywhere ahead of it.
#[test]
fn tts_serves_the_fixture_that_matches_the_text() {
    let first = pcm_fixture("first", 0x11);
    let second = pcm_fixture("second", 0x22);
    let fallback = pcm_fixture("fallback", 0x33);
    let s = Server::spawn(&[
        "--speech-for",
        &format!("checking={}", first.display()),
        "--speech-for",
        &format!("almost there={}", second.display()),
        "--speech-file",
        &fallback.display().to_string(),
    ]);
    let say = |text: &str| {
        let body = format!("{{\"input\":\"{text}\",\"response_format\":\"pcm\"}}");
        let (status, _, pcm) = s.req_bytes("POST", SPEECH_PATH, &body);
        assert_eq!(status, 200);
        pcm
    };
    assert_eq!(say("Checking.")[0], 0x11, "the first fixture");
    assert_eq!(say("Almost there.")[0], 0x22, "the second");
    // ORDER MUST NOT MATTER: asking again gives the same voice, not the next one in a rotation.
    assert_eq!(say("Checking.")[0], 0x11, "the same text is the same audio");
    // matching is case-insensitive, because the text comes from a model and its casing is not a
    // contract. anything unmatched falls back to --speech-file rather than to silence.
    assert_eq!(say("CHECKING, one moment.")[0], 0x11);
    assert_eq!(say("something else entirely")[0], 0x33, "the fallback");

    // and with no variants configured at all, --speech-file is still the whole answer.
    let plain = Server::spawn(&["--speech-file", &fallback.display().to_string()]);
    let (_, _, pcm) = plain.req_bytes(
        "POST",
        SPEECH_PATH,
        "{\"input\":\"checking\",\"response_format\":\"pcm\"}",
    );
    assert_eq!(pcm[0], 0x33);
}

// A SCENARIO IS A CONVERSATION. a harness that plays two different utterances into a microphone
// and is told the same words both times cannot script a flow at all -- and a recording of the
// exchange, where the transcript is what the agent answers, becomes nonsense to listen to.
#[test]
fn stt_transcript_is_configurable_and_settable_at_runtime() {
    let s = Server::spawn(&["--transcript", "the build is failing again"]);
    let (status, body) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(status, 200);
    assert_eq!(json(&body)["text"], "the build is failing again");

    // the next utterance is a different one, and the mock has to be able to say so mid-run.
    let (status, _) = s.post("/__admin/behavior", "{\"transcript\":\"thanks, that worked\"}");
    assert_eq!(status, 200);
    let (_, body) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(json(&body)["text"], "thanks, that worked");

    // an explicit null goes back to the built-in, so a scenario can hand the mock back unchanged.
    s.post("/__admin/behavior", "{\"transcript\":null}");
    let (_, body) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(json(&body)["text"], "this is a test transcription");

    // an empty clip is still an empty transcript, whatever text is configured: what the
    // transcriber HEARD wins over what it would have said.
    s.post(
        "/__admin/behavior",
        "{\"transcript\":\"hello\",\"empty_transcript\":true}",
    );
    let (_, body) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(json(&body)["text"], "");
}

// a transcriber that hears no speech returns an EMPTY string, not an error. consumers get this
// wrong (hmux's voice face said nothing back and wedged its client in "transcribing"), so the
// mock has to be able to produce it.
#[test]
fn stt_can_return_an_empty_transcript() {
    let s = Server::spawn(&["--empty-transcript"]);
    let (status, body) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(status, 200);
    assert_eq!(json(&body)["text"], "", "expected an empty transcript: {body}");

    // and the default is still a real transcript, so existing consumers are unaffected.
    let plain = Server::spawn(&[]);
    let (_, body) = plain.post(TRANSCRIPTIONS_PATH, "blob");
    assert!(!json(&body)["text"].as_str().unwrap().is_empty());
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

/// THE LEDGER MUST NOT LAG THE TRAFFIC. a capture records what the client ASKED, so it is written
/// when the request arrives -- `connect_delay_ms` describes how slowly this server ANSWERS, and has
/// no business backdating the question.
///
/// recording it after the sleep made every request invisible for the whole delay, which turns any
/// "act, then count captures" harness into a race against an artificial timer. hmux's audio bench
/// read `0 of 6` blocks synthesized for a turn this server had already been asked to synthesize
/// four times, and the same check passed or failed between runs of identical client code.
#[test]
fn a_capture_is_recorded_before_the_connect_delay_is_paid() {
    let s = Server::spawn(&["--connect-delay-ms", "1500"]);
    let addr = s.addr.clone();
    let body = chat_body(false, false, true);
    // this request cannot answer for 1500ms, so it is still in flight for the whole assertion.
    let caller = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(&addr).expect("connect");
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let head = format!(
            "POST {CHAT_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
    });
    // WELL INSIDE THE DELAY: late enough that the request has certainly been read, early enough
    // that no response can have been sent -- so a capture found here was written on arrival.
    std::thread::sleep(Duration::from_millis(400));
    let seen = s.captures();
    assert_eq!(
        seen.len(),
        1,
        "the request is in flight and unrecorded -- the ledger is racing the connect delay: {seen:?}"
    );
    assert_eq!(seen[0]["path"], CHAT_PATH);
    // and it is stamped, so a consumer can correlate against its own log rather than infer from
    // order alone.
    assert!(
        seen[0]["at_ms"].as_u64().unwrap_or(0) > 0,
        "capture carries no at_ms: {:?}",
        seen[0]
    );
    caller.join().expect("caller thread");
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

// --- context-window: derived usage + overflow at the window ---------------

// a non-streaming chat body whose single user message carries `content`.
fn sized_chat_body(content: &str) -> String {
    serde_json::json!({
        "model": "fake-model",
        "messages": [{"role": "user", "content": content}],
        "stream": false,
    })
    .to_string()
}

// the prompt tokens the behavior derives for `content`: the serialized messages
// array length over chars_per_token, rounded up. computed with the same
// serde_json the binary uses, so it matches byte for byte.
fn derived_prompt(content: &str, chars_per_token: u64) -> u64 {
    let messages = serde_json::json!([{"role": "user", "content": content}]);
    (messages.to_string().len() as u64).div_ceil(chars_per_token)
}

#[test]
fn context_window_reports_usage_derived_from_request() {
    // a window far above any request here: nothing overflows, so this isolates
    // the usage derivation.
    let s = Server::spawn(&["--context-window", "1000000"]);
    s.post("/__admin/responses", "{\"content\":\"ok\"}");
    let content = "x".repeat(400);
    let (status, body) = s.post(CHAT_PATH, &sized_chat_body(&content));
    assert_eq!(status, 200, "{body}");
    let usage = &json(&body)["usage"];
    let expected = derived_prompt(&content, 4);
    assert_eq!(usage["prompt_tokens"], expected, "{body}");
    assert_eq!(
        usage["total_tokens"].as_u64().unwrap(),
        expected + usage["completion_tokens"].as_u64().unwrap(),
    );
}

#[test]
fn context_window_smaller_request_reports_smaller_usage() {
    // the property a compaction loop depends on: shrinking the request shrinks
    // the reported usage, so a usage-based trigger sees the reduction.
    let s = Server::spawn(&["--context-window", "1000000"]);
    s.post("/__admin/responses", "[{\"content\":\"a\"},{\"content\":\"b\"}]");
    let (_, big) = s.post(CHAT_PATH, &sized_chat_body(&"x".repeat(4000)));
    let (_, small) = s.post(CHAT_PATH, &sized_chat_body("x"));
    let big_prompt = json(&big)["usage"]["prompt_tokens"].as_u64().unwrap();
    let small_prompt = json(&small)["usage"]["prompt_tokens"].as_u64().unwrap();
    assert!(big_prompt > small_prompt, "big {big_prompt} small {small_prompt}");
}

#[test]
fn context_window_rejects_oversized_request_without_consuming() {
    // a tiny window: any real message overflows it. the rejection is a 400
    // context_length_exceeded and the queued spec is left intact for the retry.
    let s = Server::spawn(&["--context-window", "5"]);
    s.post("/__admin/responses", "{\"content\":\"unconsumed\"}");
    assert_eq!(s.pending(), 1);
    let (status, body) = s.post(CHAT_PATH, &sized_chat_body(&"x".repeat(100)));
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "context_length_exceeded");
    assert_eq!(s.pending(), 1, "an overflowing request must not consume a spec");
}

#[test]
fn context_window_serves_request_under_the_window() {
    let s = Server::spawn(&["--context-window", "1000000"]);
    s.post("/__admin/responses", "{\"content\":\"served\"}");
    let (status, body) = s.post(CHAT_PATH, &sized_chat_body("small"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["choices"][0]["message"]["content"], "served");
    assert_eq!(s.pending(), 0);
}

#[test]
fn chars_per_token_scales_the_derivation() {
    // halving chars-per-token doubles the derived prompt for the same request.
    let s = Server::spawn(&["--context-window", "1000000", "--chars-per-token", "2"]);
    s.post("/__admin/responses", "{\"content\":\"ok\"}");
    let content = "x".repeat(400);
    let (_, body) = s.post(CHAT_PATH, &sized_chat_body(&content));
    assert_eq!(json(&body)["usage"]["prompt_tokens"], derived_prompt(&content, 2));
}

#[test]
fn context_window_off_by_default() {
    // no flag: the spec's own usage is reported verbatim and nothing is rejected.
    let s = Server::spawn(&[]);
    s.post(
        "/__admin/responses",
        "{\"content\":\"ok\",\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}",
    );
    let (status, body) = s.post(CHAT_PATH, &sized_chat_body(&"x".repeat(10000)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["usage"]["prompt_tokens"], 7);
}

#[test]
fn context_window_settable_and_clearable_via_admin() {
    let s = Server::spawn(&[]);
    s.post("/__admin/behavior", "{\"context_window\":123,\"chars_per_token\":2}");
    let (_, body) = s.get("/__admin/behavior");
    assert_eq!(json(&body)["context_window"], 123);
    assert_eq!(json(&body)["chars_per_token"], 2);
    // an explicit null clears the window back to unbounded.
    s.post("/__admin/behavior", "{\"context_window\":null}");
    let (_, body) = s.get("/__admin/behavior");
    assert!(json(&body)["context_window"].is_null(), "{body}");
}

// a model list carrying llama-swap metadata: modality tags and an alias.
const LLAMASWAP_MODELS: &str = concat!(
    "[{\"id\":\"qwen3.6-27b:Q8_0\",\"meta\":{\"llamaswap\":",
    "{\"aliases\":[\"llm-1\"],\"modsi\":\"text,image\",\"modso\":\"text\"}}},",
    "{\"id\":\"plain-model\"}]",
);

#[test]
fn models_keep_caller_supplied_metadata() {
    let s = Server::spawn(&[]);
    assert_eq!(s.req("PUT", "/__admin/models", LLAMASWAP_MODELS).0, 200);
    let (_, body) = s.get("/v1/models");
    let data = json(&body)["data"].clone();
    assert_eq!(data[0]["meta"]["llamaswap"]["modso"], "text");
    assert_eq!(data[0]["meta"]["llamaswap"]["aliases"][0], "llm-1");
    // the standard openai fields are filled in for both shapes.
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[1]["id"], "plain-model");
    assert_eq!(data[1]["object"], "model");
}

#[test]
fn models_from_json_flag() {
    let s = Server::spawn(&["--models-json", LLAMASWAP_MODELS]);
    let (_, body) = s.get("/v1/models");
    assert_eq!(json(&body)["data"][0]["id"], "qwen3.6-27b:Q8_0");
}

#[test]
fn alias_lookup_requires_llamaswap() {
    let s = Server::spawn(&[]);
    s.req("PUT", "/__admin/models", LLAMASWAP_MODELS);
    // off by default: only the real id resolves.
    assert_eq!(s.get("/v1/models/qwen3.6-27b:Q8_0").0, 200);
    assert_eq!(s.get("/v1/models/llm-1").0, 404);

    s.post("/__admin/behavior", "{\"llamaswap\":true}");
    let (status, body) = s.get("/v1/models/llm-1");
    assert_eq!(status, 200);
    assert_eq!(json(&body)["id"], "qwen3.6-27b:Q8_0");
}

#[test]
fn upstream_routes_require_llamaswap() {
    let s = Server::spawn(&[]);
    let path = "/upstream/fake-model/v1/chat/completions";
    assert_eq!(s.post(path, &chat_body(false, false, true)).0, 404);

    s.post("/__admin/behavior", "{\"llamaswap\":true}");
    let (status, body) = s.post(path, &chat_body(false, false, true));
    assert_eq!(status, 200);
    assert_eq!(body, FALLBACK_SSE);
}

#[test]
fn upstream_flag_enables_every_endpoint_family() {
    let s = Server::spawn(&["--llamaswap"]);
    assert_eq!(s.post("/upstream/m/v1/audio/speech", "{}").0, 200);
    assert_eq!(s.post("/upstream/m/v1/images/generations", "{}").0, 200);
    assert_eq!(s.get("/upstream/m/v1/audio/voices").0, 200);
}

#[test]
fn images_return_decodable_png() {
    let s = Server::spawn(&[]);
    let (status, body) = s.post(IMAGES_PATH, "{\"prompt\":\"a cat\"}");
    assert_eq!(status, 200);
    let data = json(&body)["data"].clone();
    assert_eq!(data.as_array().unwrap().len(), 1);
    let b64 = data[0]["b64_json"].as_str().unwrap().to_string();
    let bytes = b64_decode(&b64);
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a png header");
    assert!(bytes.ends_with(b"IEND\xae\x42\x60\x82"), "truncated png");
}

#[test]
fn images_honor_n_and_url_format() {
    let s = Server::spawn(&[]);
    let (_, body) = s.post(IMAGES_PATH, "{\"prompt\":\"x\",\"n\":3}");
    assert_eq!(json(&body)["data"].as_array().unwrap().len(), 3);

    let (_, body) = s.post(IMAGES_PATH, "{\"prompt\":\"x\",\"response_format\":\"url\"}");
    let url = json(&body)["data"][0]["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
}

#[test]
fn voice_clone_echoes_the_requested_name() {
    let s = Server::spawn(&[]);
    let body = concat!(
        "--X\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nnarrator\r\n",
        "--X\r\nContent-Disposition: form-data; name=\"audio_sample\"; filename=\"a.wav\"\r\n\r\n",
        "RIFFdata\r\n--X--\r\n",
    );
    let (status, resp) = s.post(VOICES_PATH, body);
    assert_eq!(status, 200);
    assert_eq!(json(&resp)["id"], "narrator");
    assert_eq!(json(&resp)["object"], "voice");
}

#[test]
fn voice_clone_falls_back_to_a_default_name() {
    let s = Server::spawn(&[]);
    let (_, resp) = s.post(VOICES_PATH, "--X\r\nno fields here\r\n--X--\r\n");
    assert_eq!(json(&resp)["id"], "cloned-voice");
}

#[test]
fn speech_with_a_cloned_voice_is_gated_on_consent() {
    // ENROLMENT IS NOT THE ONLY GATE. crispasr also refuses to SPEAK as a cloned
    // voice without an attestation, and a client that only sends the field at
    // upload time passes the first gate and fails the second.
    let s = Server::spawn(&[FLAG_REQUIRE_VOICE_CONSENT]);
    let (status, body) = s.post(SPEECH_PATH, r#"{"input":"hi","voice":"narrator"}"#);
    assert_eq!(status, 400);
    assert_eq!(json(&body)["error"]["code"], "consent_required");
}

#[test]
fn speech_with_a_cloned_voice_passes_with_consent() {
    let s = Server::spawn(&[FLAG_REQUIRE_VOICE_CONSENT]);
    let body = r#"{"input":"hi","voice":"narrator","consent_attestation":"i hold the rights"}"#;
    let (status, _) = s.post(SPEECH_PATH, body);
    assert_eq!(status, 200);
}

#[test]
fn speech_with_a_preset_voice_is_never_gated() {
    // the gate is about CLONES. a preset is nobody's likeness, so requiring an
    // attestation for one would make the mock reject requests a real server takes.
    let s = Server::spawn(&[FLAG_REQUIRE_VOICE_CONSENT]);
    let body = format!(r#"{{"input":"hi","voice":"{DEFAULT_VOICE}"}}"#);
    assert_eq!(s.post(SPEECH_PATH, &body).0, 200);
    assert_eq!(s.post(SPEECH_PATH, r#"{"input":"hi"}"#).0, 200);
}

#[test]
fn speech_consent_gate_is_off_by_default() {
    // it is a crispasr extension, not openai. left on by default the mock would
    // reject plain openai traffic that every other consumer here sends.
    let s = Server::spawn(&[]);
    assert_eq!(s.post(SPEECH_PATH, r#"{"input":"hi","voice":"narrator"}"#).0, 200);
}

#[test]
fn voice_registry_lists_voices() {
    let s = Server::spawn(&[]);
    let (status, body) = s.get(REGISTRY_VOICES_PATH);
    assert_eq!(status, 200);
    assert!(body.contains("alloy"), "{body}");
}

// a well-formed enrolment for `name`, attestation included.
fn voice_upload_body(name: &str) -> String {
    format!(
        "--X\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n\
         --X\r\nContent-Disposition: form-data; name=\"consent_attestation\"\r\n\r\ni hold the rights\r\n\
         --X\r\nContent-Disposition: form-data; name=\"voice\"; filename=\"a.wav\"\r\n\r\n\
         RIFFdata\r\n--X--\r\n"
    )
}

#[test]
fn voice_registry_upload_returns_the_stored_voice() {
    // crispasr-style servers enrol a clone under POST /v1/voices and answer 201
    // describing the STORED FILE, not the openai-shaped voice object the
    // /v1/audio/voices path returns. a client that assumes one shape on both
    // paths reads a null id off the other, so the mock keeps them distinct.
    let s = Server::spawn(&[]);
    let (status, resp) = s.post(REGISTRY_VOICES_PATH, &voice_upload_body("narrator"));
    assert_eq!(status, 201);
    assert_eq!(json(&resp)["name"], "narrator");
    assert_eq!(json(&resp)["format"], "wav");
}

#[test]
fn voice_registry_rejects_a_duplicate_name() {
    // THE REGISTRY REMEMBERS. a stateless mock answers 201 forever and hides the
    // one failure a workflow re-run actually hits: the name it chose last time
    // is still taken. the reply names `force` because the client has to find it.
    let s = Server::spawn(&[]);
    assert_eq!(s.post(REGISTRY_VOICES_PATH, &voice_upload_body("narrator")).0, 201);
    let (status, resp) = s.post(REGISTRY_VOICES_PATH, &voice_upload_body("narrator"));
    assert_eq!(status, 409);
    assert!(resp.contains("force"), "{resp}");
}

#[test]
fn voice_registry_force_overwrites_an_existing_voice() {
    let s = Server::spawn(&[]);
    assert_eq!(s.post(REGISTRY_VOICES_PATH, &voice_upload_body("narrator")).0, 201);
    let path = format!("{REGISTRY_VOICES_PATH}?force=true");
    let (status, resp) = s.post(&path, &voice_upload_body("narrator"));
    assert_eq!(status, 201);
    assert_eq!(json(&resp)["name"], "narrator");
}

#[test]
fn voice_registry_lists_what_was_enrolled() {
    let s = Server::spawn(&[]);
    s.post(REGISTRY_VOICES_PATH, &voice_upload_body("narrator"));
    let (status, body) = s.get(REGISTRY_VOICES_PATH);
    assert_eq!(status, 200);
    assert!(body.contains("narrator"), "{body}");
    assert!(body.contains(DEFAULT_VOICE), "{body}");
}

#[test]
fn voice_registry_upload_requires_consent() {
    // THE ATTESTATION IS THE GATE. a client that forgets the field must fail
    // here, where the test names the reason, rather than against a real server.
    let s = Server::spawn(&[]);
    let body = concat!(
        "--X\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nnarrator\r\n",
        "--X\r\nContent-Disposition: form-data; name=\"voice\"; filename=\"a.wav\"\r\n\r\n",
        "RIFFdata\r\n--X--\r\n",
    );
    let (status, resp) = s.post(REGISTRY_VOICES_PATH, body);
    assert_eq!(status, 400);
    assert_eq!(json(&resp)["code"], "consent_required");
}

// standard base64 decode, to prove the served image is real png bytes.
fn b64_decode(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|b| *b != b'=') {
        let v = ALPHABET.iter().position(|a| *a == c).expect("bad base64") as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

// --- v0.7: prove the audio was real -------------------------------------------------

// a multipart body carrying `bytes` as the `file` field, shaped like the real client's.
fn multipart_audio(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
Content-Type: audio/wav\r\n\r\n",
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--b--\r\n");
    body
}

// s16_le samples as bytes: `n` frames of `amplitude`, alternating sign so it is not dc.
fn pcm(n: usize, amplitude: i16) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = if i % 2 == 0 { amplitude } else { -amplitude };
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[test]
fn transcription_captures_measure_the_audio() {
    // THE DEFECT THIS CLOSES. the handler ignored the request entirely and answered a fixed
    // transcript, so a caller that committed pure silence got a transcript and a green test.
    // a downstream harness lost months to exactly that: its emulator microphone delivered zeros
    // and every spoken scenario still passed.
    let s = Server::spawn(&[]);
    s.post_bytes(TRANSCRIPTIONS_PATH, &multipart_audio(&pcm(512, 9000)));
    let caps = s.captures();
    let audio = caps
        .iter()
        .find(|c| c["path"].as_str() == Some(TRANSCRIPTIONS_PATH))
        .map(|c| c["audio"].clone())
        .expect("a transcription capture");
    assert!(audio.is_object(), "no audio measurement: {audio}");
    assert!(audio["samples"].as_u64().unwrap_or(0) > 0, "no samples: {audio}");
    assert!(audio["peak"].as_u64().unwrap_or(0) > 1000, "not loud: {audio}");
    assert_eq!(audio["silent"].as_bool(), Some(false), "loud audio read as silent");
}

#[test]
fn silence_is_reported_as_silent() {
    let s = Server::spawn(&[]);
    s.post_bytes(TRANSCRIPTIONS_PATH, &multipart_audio(&pcm(512, 0)));
    let caps = s.captures();
    let audio = caps
        .iter()
        .find(|c| c["path"].as_str() == Some(TRANSCRIPTIONS_PATH))
        .map(|c| c["audio"].clone())
        .expect("a transcription capture");
    assert_eq!(audio["silent"].as_bool(), Some(true), "silence read as sound: {audio}");
    assert_eq!(audio["peak"].as_u64(), Some(0), "silence has a peak: {audio}");
}

#[test]
fn silence_still_transcribes_by_default() {
    // REQUIREMENT 5 MUST NOT REGRESS: every existing consumer relies on the fixed transcript,
    // so measuring the audio changes what is REPORTED, never what is answered.
    let s = Server::spawn(&[]);
    let (status, body) = s.post_bytes(TRANSCRIPTIONS_PATH, &multipart_audio(&pcm(512, 0)));
    assert_eq!(status, 200);
    assert_eq!(json(&body)["text"], "this is a test transcription");
}

#[test]
fn reject_silent_audio_refuses_silence() {
    let s = Server::spawn(&["--reject-silent-audio"]);
    let (status, body) = s.post_bytes(TRANSCRIPTIONS_PATH, &multipart_audio(&pcm(512, 0)));
    assert_eq!(status, 400, "silence was accepted: {body}");
    assert!(
        json(&body)["error"]["message"].as_str().unwrap_or("").contains("silent"),
        "the error does not say why: {body}"
    );
}

#[test]
fn reject_silent_audio_still_accepts_speech() {
    let s = Server::spawn(&["--reject-silent-audio"]);
    let (status, body) = s.post_bytes(TRANSCRIPTIONS_PATH, &multipart_audio(&pcm(512, 9000)));
    assert_eq!(status, 200, "real audio was rejected: {body}");
    assert_eq!(json(&body)["text"], "this is a test transcription");
}

#[test]
fn a_body_with_no_audio_is_not_measured() {
    // the older tests post the string "blob"; that carries no file field and must stay working.
    let s = Server::spawn(&[]);
    let (status, _) = s.post(TRANSCRIPTIONS_PATH, "blob");
    assert_eq!(status, 200);
    let caps = s.captures();
    let cap = caps
        .iter()
        .find(|c| c["path"].as_str() == Some(TRANSCRIPTIONS_PATH))
        .expect("a capture");
    assert!(cap["audio"].is_null(), "measured audio that was not there: {cap}");
}

// llama.cpp's native scoring surface (--llamacpp). a decision harness reads a
// probability distribution off one token position instead of generating text, so
// what it needs from a mock is a distribution the test CHOSE. real weights cannot
// be told that, which is why every consumer of this family is a test of the
// client rather than of a model.

#[test]
fn llama_endpoints_stay_closed_until_enabled() {
    let s = Server::spawn(&[]);
    let (status, body) = s.get(PROPS_PATH);
    assert_eq!(status, 404, "props answered without --llamacpp");
    let endpoints = json(&body)["endpoints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !endpoints
            .iter()
            .any(|e| e.as_str().unwrap_or("").contains("props")),
        "the listing advertises a disabled family: {body}"
    );
    for path in [TOKENIZE_PATH, COMPLETION_PATH] {
        assert_eq!(
            s.post(path, "{}").0,
            404,
            "{path} answered without --llamacpp"
        );
    }
}

#[test]
fn llama_props_report_a_template_and_a_vocab() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_CHAT_TEMPLATE, TEMPLATE]);
    let (status, body) = s.get(PROPS_PATH);
    assert_eq!(status, 200);
    let v = json(&body);
    // a client derives its prompt affixes from the template, so the string it
    // gets back has to be the one that was configured.
    assert_eq!(v["chat_template"].as_str(), Some(TEMPLATE));
    assert_eq!(v["n_vocab"].as_u64(), Some(VOCAB));
    assert!(v["bos_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(v["eos_token"].as_str().is_some_and(|t| !t.is_empty()));
}

#[test]
fn llama_tokenize_is_byte_level() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    let (status, body) = s.post(
        TOKENIZE_PATH,
        &serde_json::json!({"content": "A B"}).to_string(),
    );
    assert_eq!(status, 200);
    assert_eq!(ids(&json(&body)["tokens"]), vec![65, 32, 66]);

    // ids come out of here and go back in as the labels to score, so the two
    // ends of a session have to agree without anyone writing them down.
    let (_, body) = s.post(
        TOKENIZE_PATH,
        &serde_json::json!({"content": "A", "add_special": true}).to_string(),
    );
    assert_eq!(ids(&json(&body)["tokens"]), vec![BOS_ID, 65]);
}

#[test]
fn llama_completion_without_a_spec_answers_the_first_label() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    let (status, body) = s.post(COMPLETION_PATH, &completion_body("hello", 64));
    assert_eq!(status, 200);
    let e = entries(&body);
    // padded out to the requested width, the way a real server returns the top n
    // of a whole vocabulary. a short list is how a client learns it saw the
    // entire distribution, so padding is not decoration.
    assert_eq!(e.len(), 64, "answered {} of 64: {body}", e.len());
    assert_eq!(e[0]["id"].as_u64(), Some(65), "default answer: {body}");
    assert_eq!(e[0]["token"].as_str(), Some("A"));
    assert!(approx(e[0]["logprob"].as_f64().unwrap(), 1.0_f64.ln()));
    // the greedy token is the top of the distribution, as it is on the real one
    assert_eq!(json(&body)["choices"][0]["text"].as_str(), Some("A"));
}

#[test]
fn llama_completion_pops_the_programmed_distributions() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"A": 0.25, "B": 0.75}}).to_string(),
    );
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"A": 0.9, "B": 0.1}}).to_string(),
    );

    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q1", 64));
    let e = entries(&body);
    assert_eq!(e[0]["id"].as_u64(), Some(66), "B should be on top: {body}");
    assert!(approx(e[0]["logprob"].as_f64().unwrap(), 0.75_f64.ln()));

    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q2", 64));
    let e = entries(&body);
    assert_eq!(e[0]["id"].as_u64(), Some(65));
    assert!(approx(e[0]["logprob"].as_f64().unwrap(), 0.9_f64.ln()));
    // probabilities are reported as well as log probabilities, since the whole
    // point of scripting them is asserting on them.
    assert!(approx(e[0]["prob"].as_f64().unwrap(), 0.9));

    // and the queue is empty again: the default, not a repeat of the last spec.
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q3", 64));
    assert!(approx(entries(&body)[0]["prob"].as_f64().unwrap(), 1.0));
}

#[test]
fn llama_completion_truncates_to_the_requested_width() {
    // n_probs is what a client walks to tell "outside the top k" from "zero",
    // so answering n_probs=1 with the whole scripted distribution would erase
    // the one failure mode the ladder exists to catch.
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"A": 0.4, "B": 0.6}}).to_string(),
    );
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q", 1));
    let e = entries(&body);
    assert_eq!(e.len(), 1, "n_probs=1 returned {}: {body}", e.len());
    assert_eq!(e[0]["id"].as_u64(), Some(66));
}

#[test]
fn llama_completion_reports_the_prompt_it_was_given() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("hello there", 8));
    assert_eq!(json(&body)["timings"]["prompt_n"].as_u64(), Some(11));
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("hello there you", 8));
    assert_eq!(
        json(&body)["timings"]["prompt_n"].as_u64(),
        Some(15),
        "a longer prompt has to report a longer prompt_n"
    );

    // a prompt sent as token ids is counted in ids, which is the other shape a
    // client sends.
    let (_, body) = s.post(
        COMPLETION_PATH,
        &serde_json::json!({"prompt": [65, 66, 67], "n_probs": 8}).to_string(),
    );
    assert_eq!(json(&body)["timings"]["prompt_n"].as_u64(), Some(3));

    // a scripted cache_n is reported as-is: prefix reuse is a property the
    // client asserts on, and a fixed zero cannot tell hot from cold.
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"A": 1.0}, "cache_n": 30}).to_string(),
    );
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("hello there you", 8));
    assert_eq!(json(&body)["timings"]["cache_n"].as_u64(), Some(30));
}

#[test]
fn llama_completion_serves_a_failing_upstream() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"status": 503, "error": "model not loaded"}).to_string(),
    );
    let (status, body) = s.post(COMPLETION_PATH, &completion_body("q", 8));
    assert_eq!(status, 503, "the fault was not served: {body}");
    // llama.cpp's native errors are a bare string, not openai's error object, and
    // a client that tells an operator what happened reads it directly.
    assert_eq!(json(&body)["error"].as_str(), Some("model not loaded"));
}

#[test]
fn llama_requests_are_captured() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    s.post(COMPLETION_PATH, &completion_body("what queue", 8));
    let caps = s.captures();
    let cap = caps
        .iter()
        .rev()
        .find(|c| c["path"].as_str() == Some(COMPLETION_PATH))
        .expect("a capture");
    // the prompt a client assembled is the fact worth asserting on, and the
    // scoring answer says nothing about whether it was built correctly.
    assert_eq!(cap["body"]["prompt"].as_str(), Some("what queue"));
}

#[test]
fn llama_native_routes_behind_the_gateway_prefix() {
    // llama-swap fronts the native endpoints at /upstream/<model>/<path> with no
    // /v1, which is a different rewrite from the openai routes it already does.
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    let (status, _) = s.get("/upstream/qwen3.5-9b:Q8_0/props");
    assert_eq!(
        status, 404,
        "the gateway prefix answered without --llamaswap"
    );

    let s = Server::spawn(&[FLAG_LLAMACPP, "--llamaswap"]);
    assert_eq!(s.get("/upstream/qwen3.5-9b:Q8_0/props").0, 200);
    let (status, body) = s.post(
        "/upstream/qwen3.5-9b:Q8_0/completion",
        &completion_body("q", 8),
    );
    assert_eq!(status, 200, "scoring behind the prefix: {body}");
}

#[test]
fn a_default_scoring_answer_replies_to_every_read() {
    // a client that widens n_probs scores the same question more than once, so a
    // queue of one spec is gone after the first read. the default is how a caller
    // says "answer this way until told otherwise".
    let s = Server::spawn(&[
        FLAG_LLAMACPP,
        "--default-probs",
        r#"{"probs":{"A":0.1,"B":0.9}}"#,
    ]);
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q1", 64));
    assert_eq!(entries(&body)[0]["id"].as_u64(), Some(66), "the default did not answer: {body}");
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q2", 64));
    assert_eq!(
        entries(&body)[0]["id"].as_u64(),
        Some(66),
        "the default answered once and then stopped"
    );

    // a programmed spec still comes first, and the default resumes after it.
    s.post(LLAMA_PROBS_PATH, &serde_json::json!({"probs": {"A": 1.0}}).to_string());
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q3", 64));
    assert_eq!(entries(&body)[0]["id"].as_u64(), Some(65));
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q4", 64));
    assert_eq!(entries(&body)[0]["id"].as_u64(), Some(66));

    // inspectable and clearable, like the chat fallback.
    let (_, body) = s.get("/__admin/default_probs");
    assert!(body.contains("0.9"), "the default is not reported: {body}");
    s.req("DELETE", "/__admin/default_probs", "");
    let (_, body) = s.post(COMPLETION_PATH, &completion_body("q5", 64));
    assert_eq!(
        entries(&body)[0]["id"].as_u64(),
        Some(65),
        "clearing the default did not restore the built-in answer: {body}"
    );
}

#[test]
fn a_scoring_spec_naming_an_unknown_token_is_refused() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    let (status, body) = s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"not a token": 1.0}}).to_string(),
    );
    assert_eq!(status, 400, "a bad spec was accepted: {body}");
    // and it is stored nowhere. answering a named token with a different one is
    // worse than refusing, since the whole point is that the distribution is the
    // one the caller asked for.
    let (_, body) = s.get(LLAMA_PROBS_PATH);
    assert_eq!(
        json(&body)["probs"].as_array().map(|a| a.len()),
        Some(0),
        "the refused spec was kept: {body}"
    );
}

#[test]
fn the_scoring_queue_is_separate_from_the_chat_queue() {
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    s.post(
        LLAMA_PROBS_PATH,
        &serde_json::json!({"probs": {"A": 1.0}}).to_string(),
    );
    let (_, body) = s.get(LLAMA_PROBS_PATH);
    assert_eq!(
        json(&body)["probs"].as_array().map(|a| a.len()),
        Some(1),
        "the scoring queue is not inspectable: {body}"
    );
    assert_eq!(s.pending(), 0, "a scoring spec hit the chat queue");

    // a generated turn and a scored turn are different requests, and a decision
    // run issues many of the second per the first.
    s.post(CHAT_PATH, &chat_body(false, false, false));
    let (status, body) = s.post(COMPLETION_PATH, &completion_body("q", 8));
    assert_eq!(status, 200);
    assert!(
        approx(entries(&body)[0]["prob"].as_f64().unwrap(), 1.0),
        "the chat request consumed the scoring spec: {body}"
    );
}

// llama.cpp as an EMBEDDING server (--llamacpp --pooling <none|last>). a model
// that answers through a trained decision head is not read by its logits: the
// client fetches final hidden states and applies the head itself. so what it
// needs from a mock is the vector the test CHOSE at the position its head reads,
// and, for a head that reads every position, a row per token it sent.

const EMBEDDING_PATH: &str = "/embedding";
const OAI_EMBEDDINGS_PATH: &str = "/v1/embeddings";
const FLAG_POOLING: &str = "--pooling";
const LLAMA_EMBEDDINGS_PATH: &str = "/__admin/embeddings";
const LLAMA_DEFAULT_EMBEDDING_PATH: &str = "/__admin/default_embedding";
// the width of a hidden state nobody scripted
const EMBEDDING_WIDTH: usize = 8;

// the rows of a native embedding reply: one per position.
fn rows(body: &str) -> Vec<Vec<f64>> {
    json(body)[0]["embedding"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(floats)
        .collect()
}

fn floats(v: &Value) -> Vec<f64> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|x| x.as_f64())
        .collect()
}

fn embed(s: &Server, path: &str, body: Value) -> (u16, String) {
    s.post(path, &body.to_string())
}

#[test]
fn embeddings_stay_closed_until_a_pooling_is_chosen() {
    // a llama-server started without --embeddings refuses these, and a mock
    // that answered anyway could not show a client pointed at the wrong server.
    let s = Server::spawn(&[FLAG_LLAMACPP]);
    for path in [EMBEDDING_PATH, "/embeddings", OAI_EMBEDDINGS_PATH] {
        let (status, body) = embed(&s, path, serde_json::json!({"content": "A", "input": "A"}));
        assert_eq!(status, 404, "{path} answered without --pooling: {body}");
        // the admin routes that program the family are always listed; the
        // routes a client would call are not
        assert!(
            !json(&body)["endpoints"].to_string().contains("POST /embedding")
                && !json(&body)["endpoints"].to_string().contains("POST /v1/embeddings"),
            "the listing advertises a disabled family: {body}"
        );
    }
}

#[test]
fn pooling_none_returns_a_state_per_token_derived_from_its_id() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    let (status, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(status, 200, "{body}");
    // a row per token, and each row is its token id repeated: derivable from the
    // prompt, like the ids themselves, so a client's spans can be checked
    // against the rows without a fixture.
    assert_eq!(
        rows(&body),
        vec![vec![65.0; EMBEDDING_WIDTH], vec![66.0; EMBEDDING_WIDTH]]
    );

    // a prompt of token ids, which is how a client that tokenized piece by piece
    // sends it. no token is added: the rows are the ids that were sent.
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": [7, 300, 9]}));
    assert_eq!(
        rows(&body).iter().map(|r| r[0]).collect::<Vec<f64>>(),
        vec![7.0, 300.0, 9.0]
    );
    // llama-server serves the same reply on the plural route.
    let (status, body) = embed(&s, "/embeddings", serde_json::json!({"content": "A"}));
    assert_eq!((status, rows(&body).len()), (200, 1));
}

#[test]
fn a_scripted_vector_is_the_last_position() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [1.5, -2.0, 0.25]}));
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    // the other positions take the scripted vector's width, so a reply is one
    // rectangular matrix as it is on the real server.
    assert_eq!(rows(&body), vec![vec![65.0; 3], vec![1.5, -2.0, 0.25]]);

    // the queue held one spec, so the next request is the derived state again.
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body)[1], vec![66.0; EMBEDDING_WIDTH]);
}

#[test]
fn pooling_none_is_refused_on_the_openai_route() {
    // llama-server's own refusal, which is how a client learns it has to read
    // the native route. the spec it did not read stays queued.
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [3.0]}));
    let (status, body) = embed(&s, OAI_EMBEDDINGS_PATH, serde_json::json!({"input": "AB"}));
    assert_eq!(status, 400, "{body}");
    assert!(
        json(&body)["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Pooling type 'none' is not OAI compatible")),
        "{body}"
    );
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body)[1], vec![3.0]);
}

#[test]
fn pooling_last_returns_one_vector_and_the_token_count() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "last"]);
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [3.0, 4.0]}));
    let (status, body) = embed(
        &s,
        OAI_EMBEDDINGS_PATH,
        serde_json::json!({"input": "hello", "embd_normalize": -1}),
    );
    assert_eq!(status, 200, "{body}");
    let v = json(&body);
    assert_eq!(floats(&v["data"][0]["embedding"]), vec![3.0, 4.0]);
    // the count the pooled reply would otherwise hide, in the units the prompt
    // was sent in: one token per byte.
    assert_eq!(v["usage"]["prompt_tokens"].as_u64(), Some(5));
    assert_eq!(v["usage"]["total_tokens"].as_u64(), Some(5));

    // the native route answers the same vector as a matrix of one row.
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [3.0, 4.0]}));
    let (_, body) = embed(
        &s,
        EMBEDDING_PATH,
        serde_json::json!({"content": "hello", "embd_normalize": -1}),
    );
    assert_eq!(rows(&body), vec![vec![3.0, 4.0]]);
}

#[test]
fn a_pooled_vector_is_normalised_unless_the_request_declines() {
    // llama-server's default is the euclidean norm. a head is trained on the raw
    // state, so a client that forgets to decline gets a vector that points the
    // right way and has the wrong length, and this is how a test sees that.
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "last"]);
    embed(&s, LLAMA_DEFAULT_EMBEDDING_PATH, serde_json::json!({"vector": [3.0, 4.0]}));
    s.req("PUT", LLAMA_DEFAULT_EMBEDDING_PATH, r#"{"vector":[3.0,4.0]}"#);
    let (_, body) = embed(&s, OAI_EMBEDDINGS_PATH, serde_json::json!({"input": "hello"}));
    let unit = floats(&json(&body)["data"][0]["embedding"]);
    assert!(approx(unit[0], 0.6) && approx(unit[1], 0.8), "{body}");
    let (_, body) = embed(
        &s,
        OAI_EMBEDDINGS_PATH,
        serde_json::json!({"input": "hello", "embd_normalize": -1}),
    );
    assert_eq!(floats(&json(&body)["data"][0]["embedding"]), vec![3.0, 4.0]);
}

#[test]
fn the_default_embedding_answers_until_it_is_cleared() {
    // a client verifies a head with several requests before it serves one, so
    // "answer this way until told otherwise" needs saying once.
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    assert_eq!(
        s.req("PUT", LLAMA_DEFAULT_EMBEDDING_PATH, r#"{"vector":[9.0,9.5]}"#).0,
        200
    );
    for _ in 0..2 {
        let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
        assert_eq!(rows(&body)[1], vec![9.0, 9.5]);
    }
    // a queued spec still comes first, and the default resumes after it.
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [1.0, 2.0]}));
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body)[1], vec![1.0, 2.0]);
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body)[1], vec![9.0, 9.5]);

    let (_, body) = s.get(LLAMA_DEFAULT_EMBEDDING_PATH);
    assert!(body.contains("9.5"), "the default is not reported: {body}");
    s.req("DELETE", LLAMA_DEFAULT_EMBEDDING_PATH, "");
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body)[1], vec![66.0; EMBEDDING_WIDTH]);
}

#[test]
fn an_embedding_spec_that_is_not_a_vector_is_refused() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    for bad in [r#"{"vector":"wide"}"#, r#"{"vector":[]}"#, r#"{"vector":[1,"two"]}"#] {
        let (status, body) = s.post(LLAMA_EMBEDDINGS_PATH, bad);
        assert_eq!(status, 400, "a bad spec was accepted: {bad} -> {body}");
    }
    let (_, body) = s.get(LLAMA_EMBEDDINGS_PATH);
    assert_eq!(
        json(&body)["embeddings"].as_array().map(|a| a.len()),
        Some(0),
        "a refused spec was kept: {body}"
    );
}

#[test]
fn an_embedding_spec_can_inject_a_failure() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "last"]);
    embed(
        &s,
        LLAMA_EMBEDDINGS_PATH,
        serde_json::json!({"error": "model is loading", "status": 503}),
    );
    let (status, body) = embed(&s, OAI_EMBEDDINGS_PATH, serde_json::json!({"input": "A"}));
    assert_eq!(status, 503);
    assert_eq!(json(&body)["error"].as_str(), Some("model is loading"));
}

#[test]
fn an_embedding_request_without_a_prompt_is_refused() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "none"]);
    let (status, _) = embed(&s, EMBEDDING_PATH, serde_json::json!({"nothing": true}));
    assert_eq!(status, 400);
    let (status, _) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": ""}));
    assert_eq!(status, 400, "an empty prompt has no position to return");
}

#[test]
fn embeddings_are_reachable_behind_llama_swap_and_reset_with_the_rest() {
    let s = Server::spawn(&[FLAG_LLAMACPP, FLAG_POOLING, "last", "--llamaswap"]);
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [2.0, 0.0]}));
    let (status, body) = embed(
        &s,
        "/upstream/fake-model/v1/embeddings",
        serde_json::json!({"input": "A", "embd_normalize": -1}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(floats(&json(&body)["data"][0]["embedding"]), vec![2.0, 0.0]);
    let (status, body) = embed(
        &s,
        "/upstream/fake-model/embedding",
        serde_json::json!({"content": "A", "embd_normalize": -1}),
    );
    assert_eq!((status, rows(&body).len()), (200, 1), "{body}");

    // a reset empties this queue with the others, and the pooling is a
    // behavior a caller can change without a restart.
    embed(&s, LLAMA_EMBEDDINGS_PATH, serde_json::json!({"vector": [5.0]}));
    s.post("/__admin/reset", "{}");
    let (_, body) = s.get(LLAMA_EMBEDDINGS_PATH);
    assert_eq!(json(&body)["embeddings"].as_array().map(|a| a.len()), Some(0));
    s.post("/__admin/behavior", r#"{"pooling":"none"}"#);
    let (_, body) = embed(&s, EMBEDDING_PATH, serde_json::json!({"content": "AB"}));
    assert_eq!(rows(&body).len(), 2);
}

