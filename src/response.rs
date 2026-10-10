// http message types shared across the server: the parsed inbound request, the
// outbound response, and the fixed fallback stream.

use std::io::{self, Write};
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
            // a client that hangs up mid-stream is normal in tests; ignore the error.
            let _ = write_streamed(req, resp.status, &resp.content_type, frames, hang_ms);
        }
    }
}

// a paced body, written by hand as chunked transfer: the head at once, then each frame as its
// own chunk after its lead delay, FLUSHED, then an optional idle hold before the terminator.
//
// NOT THROUGH `Request::respond`, which this used to do with a reader that slept between
// frames. tiny_http copies a chunked body through an encoder that holds 8k before it writes,
// so every delay was honoured and then the whole stream went out in one burst at the end: the
// total time was right and the pacing a client was meant to observe did not exist. the head is
// flushed FIRST and whole, because a strict client (undici) waits on headers before it will
// read a body at all.
fn write_streamed(
    req: Request,
    status: u16,
    content_type: &str,
    frames: Vec<Frame>,
    hang_ms: u64,
) -> io::Result<()> {
    let mut out = req.into_writer();
    let reason = StatusCode(status).default_reason_phrase();
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )?;
    out.flush()?;
    for frame in frames {
        if frame.delay_ms > 0 {
            thread::sleep(Duration::from_millis(frame.delay_ms));
        }
        if frame.data.is_empty() {
            // a zero-length chunk is the terminator, and would end the stream early.
            continue;
        }
        write!(out, "{:x}\r\n", frame.data.len())?;
        out.write_all(&frame.data)?;
        out.write_all(b"\r\n")?;
        out.flush()?;
    }
    // frames exhausted: hold once (idle), then end the stream.
    if hang_ms > 0 {
        thread::sleep(Duration::from_millis(hang_ms));
    }
    out.write_all(b"0\r\n\r\n")?;
    out.flush()
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}
