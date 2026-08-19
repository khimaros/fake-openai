# requirements

product requirements for `fake-openai`. these must not regress in a release.

1. a single static rust binary with minimal external dependencies, launchable
   as a subprocess by any harness regardless of the harness's own language.
2. serve an openai-compatible `POST /v1/chat/completions` endpoint, supporting
   both streaming (SSE) and non-streaming (single JSON) responses.
3. serve provider introspection: `GET /v1/models` lists a configurable model
   list (default a single `fake-model`) and `GET /v1/models/{id}` returns one
   model; any other GET returns the model list.
4. let callers program the exact sequence of responses returned, via three
   channels that all feed one FIFO queue: a file at startup, stdin in realtime,
   and the http admin api. when the queue is empty, fall back to a default
   response that is the fixed `ok` stream unless overridden by
   `--default-response` or the admin api.
5. capture every request as `{path, headers, body}` and expose it for inspection
   via the admin api and an optional jsonl log (file and/or stdout).
6. support a configurable `stall_first_with_tools` behavior that delays the first
   tools-bearing, non-heartbeat request, with a configurable `[heartbeat]`
   sentinel that exempts heartbeat requests.
7. expose an http admin control plane under `/__admin` for inspecting captures,
   programming responses, configuring behavior, and resetting state. admin
   requests are never captured.
8. select a free port when asked (`--port 0`) and announce the chosen base url on
   stdout as a single stable `base_url=...` line so a parent process can read it.
9. route requests through an endpoint-extensible dispatcher so new endpoint
   families (e.g. audio speech/transcription) are additive.
10. preserve behavioral parity with `hcp-spec/testing/mock_openai.py` for the
    chat-completions fallback stream and the stall/heartbeat semantics. the
    models endpoint is a superset: a configurable list instead of always empty.
11. inject chat-completions faults on demand: an `error` spec returns an
    openai-style error body with a chosen status, and a `truncate` spec ends a
    stream with no finish frame or `[DONE]` (a simulated aborted upstream).
12. control response timing: per-response `chunk_delay_ms` paces streamed deltas
    and `hang_ms` holds the socket open before closing; a global
    `connect_delay_ms` delays every non-admin request before any bytes. these
    exercise a client's slow-first-byte, idle, and read-timeout handling. a
    listening socket cannot simulate a tcp connect timeout, and the binary does
    not claim to.
13. optionally validate chat requests: a `validate_chat` behavior (default off,
    so the permissive default never regresses) rejects a request missing a
    `model`, missing `messages`, or lacking a bearer Authorization header with an
    openai-style 400/401.
14. optionally simulate a context window: a `context_window` behavior (default
    off, so fixed per-spec usage never regresses) derives the reported prompt/
    total usage from the request's serialized `messages` at a configurable
    `chars_per_token` (default 4), and rejects a request whose derived prompt
    exceeds the window with a `context_length_exceeded` 400 before the queue is
    consumed. this closes the feedback loop a harness's own compaction depends on.
