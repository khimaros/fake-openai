# contributing

## product requirements

always acknowledge product requirements in [REQUIREMENTS.md](REQUIREMENTS.md).
it is never okay to regress on these in a release.

## development workflow

the roadmap lives in [ROADMAP.md](ROADMAP.md). before starting a task, add it to
the roadmap; mark it done when complete.

write end-to-end integration tests before the implementation, and have them
reviewed before implementing. update [DESIGN.md](DESIGN.md) after architectural
changes and [README.md](README.md) after user-visible changes.

never mutate version control. cutting releases and committing is the user's job.

## build and test

- `make` builds the binary.
- `make precommit` runs lint, tests, and build; it must pass before committing.

deterministic builds use `mise` (see [mise.toml](mise.toml)).

## style

ascii only. comments and command-line output are lowercase, except acronyms or
emphasis. comments explain "why", not "what"; avoid changelog-style comments and
dead code. keep functions under 50 lines and the code DRY. define magical
constants as globals at the top of the file that uses them. keep external
dependencies to a minimum.

## adding an endpoint

each endpoint family is a small `Handler` in `src/handlers/`. to add one:

1. create `src/handlers/<name>.rs` implementing the `Handler` trait
   (`matches` + `handle`).
2. register it in the router's handler list in `src/router.rs`.

request capture, jsonl logging, and admin inspection apply automatically, so a
new endpoint only implements its own request/response shape.
