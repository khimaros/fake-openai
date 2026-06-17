// bind, announce the chosen base url on stdout, then serve one thread per
// request. tiny_http gives a blocking, thread-per-request model that maps onto
// the python ThreadingHTTPServer.

use std::io::Write;
use std::sync::Arc;
use std::thread;

use tiny_http::Server;

use crate::config::Config;
use crate::response::{parse_request, write_response};
use crate::responses;
use crate::router::Router;
use crate::state::{Shared, State};

pub fn serve(config: Config) {
    let state: Shared = Arc::new(State::from_config(&config));
    if let Some(path) = &config.responses_file {
        responses::load_file(&state, path);
    }
    if config.stdin {
        responses::spawn_stdin_reader(state.clone());
    }

    let server = Server::http((config.host.as_str(), config.port))
        .unwrap_or_else(|e| panic!("bind {}:{}: {e}", config.host, config.port));
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(config.port);

    // a single stable line a parent process reads to learn the url.
    println!("base_url=http://{}:{}/v1", config.host, port);
    let _ = std::io::stdout().flush();

    let router = Arc::new(Router::new());
    for mut request in server.incoming_requests() {
        let state = state.clone();
        let router = router.clone();
        thread::spawn(move || {
            let parsed = parse_request(&mut request);
            let resp = router.route(&state, &parsed);
            write_response(request, resp);
        });
    }
}
