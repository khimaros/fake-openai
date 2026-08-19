// endpoint families. each Handler owns its paths; capture, logging, and admin
// inspection are cross-cutting and applied by the router before dispatch.

use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub mod audio;
pub mod chat;
pub mod images;
pub mod models;

// matched by (method, path); path has any query string stripped.
pub trait Handler {
    fn matches(&self, method: &str, path: &str) -> bool;
    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse;
    // the routes this handler serves, advertised in the 404 endpoint listing.
    fn routes(&self) -> &'static [&'static str];
}

// minimal standard base64 encoder; embeds binary payloads (pcm frames, png
// bytes) in the json and sse bodies clients decode, without pulling in a crate.
pub fn b64(bytes: &[u8]) -> String {
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
