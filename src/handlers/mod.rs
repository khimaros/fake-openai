// endpoint families. each Handler owns its paths; capture, logging, and admin
// inspection are cross-cutting and applied by the router before dispatch.

use crate::response::{HttpResponse, ParsedRequest};
use crate::state::Shared;

pub mod audio;
pub mod chat;
pub mod models;

// matched by (method, path); path has any query string stripped.
pub trait Handler {
    fn matches(&self, method: &str, path: &str) -> bool;
    fn handle(&self, state: &Shared, req: &ParsedRequest) -> HttpResponse;
    // the routes this handler serves, advertised in the 404 endpoint listing.
    fn routes(&self) -> &'static [&'static str];
}
