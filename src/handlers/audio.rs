// audio endpoints for tts/stt mocking (pi-omni). speech returns deterministic
// pcm -- raw, or sse speech.audio.delta frames when the request asks for sse;
// transcriptions returns a fixed json transcript; voices lists a default voice.

use serde_json::json;

use crate::handlers::Handler;
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub const SPEECH_PATH: &str = "/v1/audio/speech";
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
pub const VOICES_PATH: &str = "/v1/audio/voices";

const CONTENT_TYPE_PCM: &str = "audio/pcm";
// a small deterministic pcm payload (silence). the tts client only needs
// non-empty s16_le bytes to exercise the audio path; divisible by 3 so the
// base64 of the streaming frame needs no padding.
const PCM_FRAME: [u8; 1536] = [0u8; 1536];
// the fixed transcript every stt request resolves to.
pub const STT_TRANSCRIPT: &str = "this is a test transcription";
// the default voice id reported by /v1/audio/voices.
pub const DEFAULT_VOICE: &str = "alloy";

pub struct AudioSpeech;

impl Handler for AudioSpeech {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == SPEECH_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/audio/speech"]
    }

    // sse when the client requests stream_format=sse, else a single raw blob.
    fn handle(&self, _state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let sse = req.body.get("stream_format").and_then(|v| v.as_str()) == Some("sse");
        if sse {
            let audio = b64(&PCM_FRAME);
            let body = format!(
                "event: speech.audio.delta\ndata: {{\"audio\":\"{audio}\"}}\n\n\
                 event: speech.audio.done\ndata: {{}}\n\ndata: [DONE]\n\n"
            );
            HttpResponse::sse(200, body.into_bytes())
        } else {
            HttpResponse::new(200, CONTENT_TYPE_PCM, PCM_FRAME.to_vec())
        }
    }
}

pub struct AudioTranscriptions;

impl Handler for AudioTranscriptions {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == TRANSCRIPTIONS_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/audio/transcriptions"]
    }

    // the request is multipart; the mock ignores it and returns a fixed transcript.
    fn handle(&self, _state: &Shared, _req: &ParsedRequest) -> HttpResponse {
        HttpResponse::json(200, &json!({"text": STT_TRANSCRIPT}))
    }
}

pub struct AudioVoices;

impl Handler for AudioVoices {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "GET" && path == VOICES_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["GET /v1/audio/voices"]
    }

    fn handle(&self, _state: &Shared, _req: &ParsedRequest) -> HttpResponse {
        HttpResponse::json(200, &json!({"voices": [DEFAULT_VOICE]}))
    }
}

// minimal standard base64 encoder; embeds pcm bytes in the sse audio frames the
// tts client decodes, without pulling in a crate for it.
fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
