// fake-openai: a standalone programmable openai-compatible mock for testing
// agent harnesses. see README.md and DESIGN.md.

mod admin;
mod behavior;
mod capture;
mod config;
mod handlers;
mod response;
mod responses;
mod router;
mod server;
mod state;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", config::usage());
        return;
    }
    match config::parse_args(args) {
        Ok(config) => server::serve(config),
        Err(e) => {
            eprintln!("fake-openai: {e}");
            std::process::exit(2);
        }
    }
}
