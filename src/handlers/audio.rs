// audio endpoints for tts/stt mocking (pi-omni). speech returns deterministic
// pcm -- raw, or sse speech.audio.delta frames when the request asks for sse;
// transcriptions returns a fixed json transcript; voices lists a default voice.

use serde_json::{json, Value};

use crate::handlers::{b64, Handler};
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub const SPEECH_PATH: &str = "/v1/audio/speech";
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
pub const VOICES_PATH: &str = "/v1/audio/voices";

const CONTENT_TYPE_PCM: &str = "audio/pcm";
// divisible by 3 so the base64 of the streaming frame needs no padding.
const PCM_FRAME_LEN: usize = 1536;
// the ramp's period. PRIME, and so coprime with the 2-byte s16_le sample width: a stream that
// slipped by one byte cannot coincidentally re-align, which a power-of-two period would allow.
const PCM_RAMP_PERIOD: usize = 251;
// a deterministic pcm payload carrying POSITION, so a consumer can assert the audio it received
// is the audio that was sent -- in order, unshifted, untruncated. this was silence once, and
// silence cannot distinguish success from a byte-shifted stream, a dropped frame, or a
// truncated body: every one of those is still a run of zeros.
const PCM_FRAME: [u8; PCM_FRAME_LEN] = pcm_ramp();

const fn pcm_ramp() -> [u8; PCM_FRAME_LEN] {
    let mut out = [0u8; PCM_FRAME_LEN];
    let mut i = 0;
    while i < PCM_FRAME_LEN {
        out[i] = (i % PCM_RAMP_PERIOD) as u8;
        i += 1;
    }
    out
}
// the fixed transcript every stt request resolves to.
pub const STT_TRANSCRIPT: &str = "this is a test transcription";
// the default voice id reported by /v1/audio/voices.
pub const DEFAULT_VOICE: &str = "alloy";
// the voice id returned by a clone request that names no voice.
pub const CLONED_VOICE: &str = "cloned-voice";

pub struct AudioSpeech;

impl Handler for AudioSpeech {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == SPEECH_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/audio/speech"]
    }

    // sse when the client requests stream_format=sse, else a single raw blob.
    //
    // The payload is the positional ramp unless `--speech-file` supplied real audio. Both matter
    // and neither replaces the other: the ramp proves a stream arrived in order, unshifted and
    // untruncated, which speech cannot; speech proves what a real reply does to a client's jitter
    // buffer, resampler and playout, which a 32ms ramp cannot. A fixture streams in FRAMES rather
    // than one blob so the client sees delivery paced the way a real synthesizer paces it -- one
    // burst per sentence is what makes an under-sized jitter buffer audible.
    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let sse = req.body.get("stream_format").and_then(|v| v.as_str()) == Some("sse");
        let pcm: &[u8] = state.speech_pcm.as_deref().unwrap_or(&PCM_FRAME);
        if sse {
            let mut body = String::new();
            for frame in pcm.chunks(PCM_FRAME_LEN) {
                let audio = b64(frame);
                body.push_str(&format!(
                    "event: speech.audio.delta\ndata: {{\"audio\":\"{audio}\"}}\n\n"
                ));
            }
            body.push_str("event: speech.audio.done\ndata: {}\n\ndata: [DONE]\n\n");
            HttpResponse::sse(200, body.into_bytes())
        } else {
            HttpResponse::new(200, CONTENT_TYPE_PCM, pcm.to_vec())
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

    // the request is multipart; the mock ignores it and returns a fixed transcript -- or an
    // EMPTY one under `empty_transcript`, which is what a real transcriber returns for a clip
    // its vad found no speech in. that is an ordinary outcome, not an error, and a consumer
    // handling only "text" or "failure" wedges on it.
    fn handle(&self, state: &Shared, _req: &ParsedRequest) -> HttpResponse {
        let empty = state.behavior.lock().unwrap().empty_transcript;
        let text = if empty { "" } else { STT_TRANSCRIPT };
        HttpResponse::json(200, &json!({ "text": text }))
    }
}

pub struct AudioVoices;

impl Handler for AudioVoices {
    fn matches(&self, method: &str, path: &str) -> bool {
        (method == "GET" || method == "POST") && path == VOICES_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["GET /v1/audio/voices", "POST /v1/audio/voices"]
    }

    // GET lists voices; POST clones one from a multipart sample and returns the
    // created voice. the upload itself is ignored -- only the created id matters
    // to a client, which feeds it back as the `voice` of a later speech request.
    fn handle(&self, _state: &Shared, req: &ParsedRequest) -> HttpResponse {
        if req.method != "POST" {
            return HttpResponse::json(200, &json!({"voices": [DEFAULT_VOICE]}));
        }
        let name =
            multipart_field(&req.body, "name").unwrap_or_else(|| CLONED_VOICE.to_string());
        HttpResponse::json(
            200,
            &json!({"id": name, "name": name, "object": "voice", "created": 0}),
        )
    }
}

// pull one text field out of a multipart body, which parse_request leaves as a
// lossy string under `_raw`. the mock does not parse multipart properly; it only
// needs the caller-supplied voice name to echo back, so it scans for the part's
// disposition header and takes the value that follows.
fn multipart_field(body: &Value, field: &str) -> Option<String> {
    let text = body.get("_raw")?.as_str()?;
    let marker = format!("name=\"{field}\"");
    let start = text.find(&marker)? + marker.len();
    let value = text[start..].split("\r\n\r\n").nth(1)?;
    let value = value.split("\r\n").next()?.trim();
    match value.is_empty() {
        true => None,
        false => Some(value.to_string()),
    }
}
