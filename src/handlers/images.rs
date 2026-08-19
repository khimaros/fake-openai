// image generation. returns a deterministic png so a client that decodes and
// writes the payload gets real image bytes, not a placeholder string.

use serde_json::{json, Value};

use crate::handlers::{b64, Handler};
use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub const IMAGES_PATH: &str = "/v1/images/generations";

// a 1x1 black png. the smallest payload that still parses as an image, so a
// client's decode/save/display path is exercised for real.
const PNG_1X1: [u8; 69] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0x60, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x01, 0xf6, 0x17, 0x38, 0x55, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

pub struct ImagesGenerations;

impl Handler for ImagesGenerations {
    fn matches(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == IMAGES_PATH
    }

    fn routes(&self) -> &'static [&'static str] {
        &["POST /v1/images/generations"]
    }

    // honors `n` (default 1) and the request's response_format; a url request
    // gets a data: url, since the mock has nowhere to host a file.
    fn handle(&self, _state: &Shared, req: &ParsedRequest) -> HttpResponse {
        let n = req.body.get("n").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        let as_url = req.body.get("response_format").and_then(|v| v.as_str()) == Some("url");
        let encoded = b64(&PNG_1X1);
        let data: Vec<Value> = (0..n)
            .map(|_| match as_url {
                true => json!({"url": format!("data:image/png;base64,{encoded}")}),
                false => json!({"b64_json": encoded}),
            })
            .collect();
        HttpResponse::json(200, &json!({"created": 0, "data": data}))
    }
}
