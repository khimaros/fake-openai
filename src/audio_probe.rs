// measure the audio a transcription request carries, so a caller cannot commit SILENCE and get a
// transcript back.
//
// the transcriptions handler used to ignore the request entirely and answer a fixed string. that
// is fine as a stub and ruinous as a test double: a downstream harness lost months to it, because
// its emulator's microphone was delivering zeros and every spoken scenario still passed. a mock
// that answers anything cannot tell a working capture path from a dead one.
//
// the measurement is REPORTED, not enforced, unless `--reject-silent-audio` is set -- every
// existing consumer relies on the fixed transcript.

use serde_json::{json, Value};

// samples below this are indistinguishable from a dead capture. an idle emulator microphone
// measures a peak of 8 out of 32767; real speech peaks in the thousands.
const SILENT_PEAK: i64 = 64;

// the multipart part that carries the audio, and the header that ends its section.
const FILE_MARKER: &[u8] = b"name=\"file\"";
const HEADER_END: &[u8] = b"\r\n\r\n";

/// the s16_le payload of the multipart `file` field, or None when there is no such field.
fn file_bytes(raw: &[u8]) -> Option<&[u8]> {
    let at = find(raw, FILE_MARKER)?;
    let start = at + find(&raw[at..], HEADER_END)? + HEADER_END.len();
    // the trailing boundary belongs to the envelope, not the audio.
    let end = find(&raw[start..], b"\r\n--").map_or(raw.len(), |o| start + o);
    (end > start).then(|| &raw[start..end])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// `{bytes, samples, peak, rms, silent}` for a request carrying audio, or null for one that is
/// not. interpreted as s16_le, which is what every consumer of this mock sends.
pub fn measure(raw: &[u8]) -> Value {
    let Some(audio) = file_bytes(raw) else {
        return Value::Null;
    };
    let samples: Vec<i16> = audio
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    if samples.is_empty() {
        return Value::Null;
    }
    let peak = samples.iter().map(|s| (*s as i64).abs()).max().unwrap_or(0);
    let energy: f64 = samples.iter().map(|s| (*s as f64).powi(2)).sum();
    let rms = (energy / samples.len() as f64).sqrt();
    json!({
        "bytes": audio.len(),
        "samples": samples.len(),
        "peak": peak,
        "rms": (rms * 1000.0).round() / 1000.0,
        "silent": peak <= SILENT_PEAK,
    })
}

/// did this request carry audio that is indistinguishable from a dead capture?
pub fn is_silent(raw: &[u8]) -> bool {
    measure(raw)
        .get("silent")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
