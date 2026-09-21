// audio endpoints for tts/stt mocking (pi-omni). speech returns deterministic
// pcm -- raw, or sse speech.audio.delta frames when the request asks for sse;
// transcriptions returns a fixed json transcript; voices lists a default voice.

use serde_json::{json, Value};

use crate::audio_probe;
use crate::handlers::{b64, Handler};
use crate::response::{Frame, HttpResponse, ParsedRequest, CONTENT_TYPE_SSE};
use crate::state::Shared;

pub const SPEECH_PATH: &str = "/v1/audio/speech";
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
pub const VOICES_PATH: &str = "/v1/audio/voices";
// crispasr-style servers keep the voice registry off the /audio/ prefix; POST
// lands here and only GET is aliased onto VOICES_PATH.
pub const REGISTRY_VOICES_PATH: &str = "/v1/voices";

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
// the container a registry upload is reported as when the part carries no
// filename to read an extension off.
const DEFAULT_VOICE_FORMAT: &str = "wav";

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
        if let Some(refusal) = consent_refusal(state, &req.body) {
            return refusal;
        }
        let sse = req.body.get("stream_format").and_then(|v| v.as_str()) == Some("sse");
        let input = req.body.get("input").and_then(|v| v.as_str()).unwrap_or("");
        let pcm: &[u8] = state.speech_for(input).unwrap_or(&PCM_FRAME);
        let pace = state.behavior.lock().unwrap().speech_frame_delay_ms;
        if sse && pace > 0 {
            // PACED, so the reply arrives over time the way one really does. each delta is its own
            // chunk held `pace` ms behind the last; a client whose jitter buffer only gates the
            // start of a reply runs dry in the middle of this one, which is the whole point.
            let mut frames: Vec<Frame> = pcm
                .chunks(PCM_FRAME_LEN)
                .map(|frame| Frame {
                    delay_ms: pace,
                    data: format!(
                        "event: speech.audio.delta\ndata: {{\"audio\":\"{}\"}}\n\n",
                        b64(frame)
                    )
                    .into_bytes(),
                })
                .collect();
            frames.push(Frame {
                delay_ms: pace,
                data: b"event: speech.audio.done\ndata: {}\n\ndata: [DONE]\n\n".to_vec(),
            });
            HttpResponse::streamed(200, CONTENT_TYPE_SSE, frames, 0)
        } else if sse {
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

// under `require_voice_consent`, refuse to speak as a cloned voice unless the
// request attests consent. a PRESET is nobody's likeness, so it is never gated;
// only a voice the caller named itself, which is what an enrolled clone is.
fn consent_refusal(state: &Shared, body: &Value) -> Option<HttpResponse> {
    if !state.behavior.lock().unwrap().require_voice_consent {
        return None;
    }
    let voice = body.get("voice").and_then(|v| v.as_str())?;
    if voice.is_empty() || voice == DEFAULT_VOICE {
        return None;
    }
    if body.get("consent_attestation").is_some() {
        return None;
    }
    let err = json!({"error": {
        "message": "voice cloning requires a 'consent_attestation' field in the request body",
        "type": "invalid_request_error",
        "param": "consent_attestation",
        "code": "consent_required",
    }});
    Some(HttpResponse::json(400, &err))
}

pub struct AudioTranscriptions;

impl Handler for AudioTranscriptions {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == TRANSCRIPTIONS_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/audio/transcriptions"]
    }

    // the request is multipart and the mock answers with the configured transcript -- or an EMPTY
    // one under `empty_transcript`, which is what a real transcriber returns for a clip its vad
    // found no speech in. that is an ordinary outcome, not an error, and a consumer handling only
    // "text" or "failure" wedges on it.
    //
    // UNDER `reject_silent_audio` THE REQUEST IS READ. answering a fixed string whatever arrives
    // makes this mock unable to tell a working capture path from a dead one, and a caller that
    // wants that proof gets it here rather than three layers downstream. the measurement is in
    // every capture regardless (see capture::record).
    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let b = state.behavior.lock().unwrap();
        if b.reject_silent_audio && audio_probe::is_silent(&req.raw) {
            let body = json!({"error": {
                "message": "the audio is silent -- nothing was said into this request",
                "type": "invalid_request_error",
                "code": null,
            }});
            return HttpResponse::json(400, &body);
        }
        let text = if b.empty_transcript {
            ""
        } else {
            b.transcript.as_deref().unwrap_or(STT_TRANSCRIPT)
        };
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

pub struct VoiceRegistry;

impl Handler for VoiceRegistry {
    fn matches(&self, method: &str, path: &str) -> bool {
        (method == "GET" || method == "POST") && path == REGISTRY_VOICES_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["GET /v1/voices", "POST /v1/voices"]
    }

    // the registry surface: GET lists the stored voices, POST enrols one from a
    // multipart sample. it answers 201 and describes the STORED FILE rather than
    // echoing an openai voice object, because that is what the servers carrying
    // this path do, and a client that reads `id` off the reply must find out
    // here that there isn't one.
    //
    // THE CONSENT GATE IS ENFORCED, not decorative. storing a recording as a
    // reusable voiceprint is the cloning step, so a caller that omits the
    // attestation gets the same 400 a real server gives it -- a mock that waved
    // the field through would let a client ship without ever sending it.
    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse {
        if req.method != "POST" {
            let mut voices = vec![json!({"name": DEFAULT_VOICE, "format": DEFAULT_VOICE_FORMAT})];
            for name in state.voice_names() {
                voices.push(json!({"name": name, "format": DEFAULT_VOICE_FORMAT}));
            }
            return HttpResponse::json(200, &json!({"voices": voices}));
        }
        if multipart_field(&req.body, "consent_attestation").is_none() {
            let body = json!({
                "error": "voice upload requires consent_attestation",
                "code": "consent_required",
            });
            return HttpResponse::json(400, &body);
        }
        let name = multipart_field(&req.body, "name").unwrap_or_else(|| CLONED_VOICE.to_string());
        let force = req.path.contains("force=true");
        if !state.enrol_voice(&name, force) {
            let body = json!({"error": {
                "message": format!("voice '{name}' already exists; add ?force=true to overwrite"),
                "type": "invalid_request_error",
            }});
            return HttpResponse::json(409, &body);
        }
        let format = multipart_filename(&req.body, "voice")
            .and_then(|f| f.rsplit_once('.').map(|(_, ext)| ext.to_string()))
            .unwrap_or_else(|| DEFAULT_VOICE_FORMAT.to_string());
        // the mock does not split the parts apart, so the whole body stands in
        // for the sample's size. a caller can assert it is non-zero, which is
        // the only thing the real field tells it either.
        let size_bytes = req.raw.len();
        HttpResponse::json(
            201,
            &json!({"name": name, "format": format, "size_bytes": size_bytes}),
        )
    }
}

// the `filename="..."` of a multipart file part, read off the same lossy `_raw`
// string multipart_field scans.
fn multipart_filename(body: &Value, field: &str) -> Option<String> {
    let text = body.get("_raw")?.as_str()?;
    let marker = format!("name=\"{field}\"");
    let rest = &text[text.find(&marker)? + marker.len()..];
    let header = rest.split("\r\n").next()?;
    let start = header.find("filename=\"")? + "filename=\"".len();
    Some(header[start..].split('"').next()?.to_string())
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
