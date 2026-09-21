// http message types shared across the server: the parsed inbound request, the
// outbound response, and the fixed fallback stream.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tiny_http::{Header, Request, Response, StatusCode};

// fallback chat-completions stream returned when the queue is empty and no
// default response is configured: one "ok" delta, then [DONE]. the model name
// must match config::DEFAULT_MODEL.
pub const SSE_RESPONSE: &str = concat!(
    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
    "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
);

pub const CONTENT_TYPE_SSE: &str = "text/event-stream";
pub const CONTENT_TYPE_JSON: &str = "application/json";

// an inbound request after parsing. path retains any query string (capture
// parity); headers as a json object; body parsed as json (or {"_raw": ...}).
pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub headers: Value,
    pub body: Value,
    // THE BYTES AS RECEIVED. `body` is lossy for anything that is not utf8 -- audio arrives as
    // binary and every invalid sample becomes U+FFFD -- so a consumer that needs to MEASURE what
    // was sent cannot use it. kept alongside rather than replacing it: `body` is the captured
    // contract and must not change shape.
    pub raw: Vec<u8>,
}

pub fn parse_request(req: &mut Request) -> ParsedRequest {
    let method = req.method().as_str().to_string();
    let path = req.url().to_string();
    let mut headers = serde_json::Map::new();
    for h in req.headers() {
        headers.insert(
            h.field.as_str().as_str().to_string(),
            Value::String(h.value.as_str().to_string()),
        );
    }
    let mut buf = Vec::new();
    let _ = req.as_reader().read_to_end(&mut buf);
    let body = serde_json::from_slice(&buf)
        .unwrap_or_else(|_| json!({"_raw": String::from_utf8_lossy(&buf)}));
    ParsedRequest {
        method,
        path,
        headers: Value::Object(headers),
        body,
        raw: buf,
    }
}

// one frame of a streamed response: sleep delay_ms, then write data.
pub struct Frame {
    pub delay_ms: u64,
    pub data: Vec<u8>,
}

// the outbound body: either a buffered blob written in one shot, or a sequence
// of timed frames streamed as chunked transfer (see write_response) so a client
// observes real inter-delta pacing, an early truncation, or an idle hang.
pub enum Body {
    Buffered(Vec<u8>),
    Streamed { frames: Vec<Frame>, hang_ms: u64 },
}

// an outbound response built independently of tiny_http, then written at the
// server boundary. keeps handlers easy to reason about.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Body,
}

impl HttpResponse {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> HttpResponse {
        HttpResponse {
            status,
            content_type: content_type.to_string(),
            body: Body::Buffered(body),
        }
    }

    pub fn json(status: u16, value: &Value) -> HttpResponse {
        HttpResponse::new(status, CONTENT_TYPE_JSON, value.to_string().into_bytes())
    }

    pub fn sse(status: u16, body: Vec<u8>) -> HttpResponse {
        HttpResponse::new(status, CONTENT_TYPE_SSE, body)
    }

    pub fn streamed(
        status: u16,
        content_type: &str,
        frames: Vec<Frame>,
        hang_ms: u64,
    ) -> HttpResponse {
        HttpResponse {
            status,
            content_type: content_type.to_string(),
            body: Body::Streamed { frames, hang_ms },
        }
    }
}

pub fn write_response(req: Request, resp: HttpResponse) {
    match resp.body {
        Body::Buffered(bytes) => {
            let is_sse = resp.content_type == CONTENT_TYPE_SSE;
            let mut out = Response::from_data(bytes).with_status_code(resp.status);
            out.add_header(header("Content-Type", &resp.content_type));
            if is_sse {
                out.add_header(header("Cache-Control", "no-cache"));
            }
            // a client that hangs up mid-write is normal in tests; ignore the error.
            let _ = req.respond(out);
        }
        Body::Streamed { frames, hang_ms } => {
            // chunked transfer (data_length = None) lets tiny_http write the headers
            // up front, frame each read() as its own chunk, and own connection
            // teardown. FrameReader paces the reads, so a client observes real
            // inter-delta timing and a stream that ends without [DONE] when the spec
            // truncates or hangs. writing the head by hand instead starves a strict
            // client (e.g. undici) of headers and stalls it.
            let headers = vec![
                header("Content-Type", &resp.content_type),
                header("Cache-Control", "no-cache"),
            ];
            let reader = FrameReader::new(frames, hang_ms);
            let _ = req.respond(Response::new(
                StatusCode(resp.status),
                headers,
                reader,
                None,
                None,
            ));
        }
    }
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}

// a Read that emits each frame's bytes after sleeping the frame's lead delay,
// then optionally holds (hang_ms) before signaling end of stream. tiny_http drives
// it via io::copy through a chunked encoder, so each frame becomes one paced chunk.
struct FrameReader {
    frames: VecDeque<Frame>,
    current: Vec<u8>,
    pos: usize,
    hang_ms: u64,
    hung: bool,
}

impl FrameReader {
    fn new(frames: Vec<Frame>, hang_ms: u64) -> FrameReader {
        FrameReader {
            frames: frames.into(),
            current: Vec::new(),
            pos: 0,
            hang_ms,
            hung: false,
        }
    }
}

impl Read for FrameReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // drain whatever remains of the current frame first.
        if self.pos < self.current.len() {
            let n = (self.current.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        // advance to the next frame, sleeping its lead delay.
        if let Some(frame) = self.frames.pop_front() {
            if frame.delay_ms > 0 {
                thread::sleep(Duration::from_millis(frame.delay_ms));
            }
            self.current = frame.data;
            self.pos = 0;
            return self.read(buf);
        }
        // frames exhausted: hold once (idle), then end the stream.
        if self.hang_ms > 0 && !self.hung {
            self.hung = true;
            thread::sleep(Duration::from_millis(self.hang_ms));
        }
        Ok(0)
    }
}
