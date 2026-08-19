# design

`fake-openai` is a single rust binary that mimics an openai-compatible api for
testing agent harnesses. a harness launches it as a subprocess, points its
provider base url at it, drives a scenario, and inspects what was captured.

it replaces the in-process python mock at `hcp-spec/testing/mock_openai.py` with
a language-agnostic process that is programmable (callers script the response
sequence) and observable (every request is captured and inspectable over http).

## dependencies

- `tiny_http`: blocking, thread-per-request http server. maps onto the python
  `ThreadingHTTPServer` the mock used, without hand-rolling header and SSE
  framing in std.
- `serde_json`: faithful `Value` round-trip of arbitrary request bodies and
  correct json/jsonl serialization. no `serde` derive, so no proc-macros.

nothing else: no async runtime, no http client (tests speak raw HTTP/1.1 over a
`TcpStream`), no arg-parser crate (flags are hand-parsed).

## request flow

```
tiny_http accept (one thread per request)
  -> read method, path, headers, body bytes
  -> body -> serde_json::Value (fallback {"_raw": <utf8-lossy>})
  -> path starts with /__admin? -> admin handler (NOT captured)
  -> else: capture {path, headers, body} into State + optional jsonl sink
  -> /upstream/<model>/v1/... ? -> strip prefix if llamaswap on, else 404
  -> router dispatch by (method, path):
       POST /v1/chat/completions   -> handlers::chat
       POST /v1/audio/speech       -> handlers::audio (tts)
       POST /v1/audio/transcriptions -> handlers::audio (stt)
       GET/POST /v1/audio/voices   -> handlers::audio (list / clone)
       POST /v1/images/generations -> handlers::images
       GET  /v1/models[/{id}]      -> handlers::models
       (no match)                  -> 404 with an endpoint listing
  -> non-admin: pay connect_delay_ms (slow-first-byte injection), then capture
  -> write response; ignore broken pipe
```

a response is either buffered (written in one shot, the common path) or streamed.
the streamed path takes raw control of the socket via `Request::into_writer`,
writes a `Connection: close` head, then writes each frame after sleeping its
delay, and closes on drop. only a stream that needs pacing (`chunk_delay_ms`),
truncation (`truncate`), or an idle hold (`hang_ms`) takes this path; an
unpaced/uncut stream stays buffered and byte-identical to the old single write.

## shared state

a single `Arc<State>` is cloned into each request thread. `State` holds one
mutex per concern (captures, response queue, behavior, stall latch, log sink),
mirroring the single lock the python mock used.

## endpoint extensibility

each endpoint family implements a `Handler` (`matches`, `handle`, and `routes`)
and is registered in an ordered list in `router.rs`. capture, logging, and admin
inspection are cross-cutting, applied before dispatch, so adding a family only
implements that family's request/response shape and the routes it advertises. a
request that matches nothing returns 404 whose body lists every handler's
`routes` plus the admin routes -- a discovery aid that stays accurate as families
are added (the models handler is scoped to `/v1/models`, so a bare `/v1` 404s
rather than masquerading as a model list). the `/__admin` namespace is matched
before `/v1` so control-plane paths never collide with api paths.

vendor extensions to the openai surface are gated on a behavior flag rather than
built into a handler's matcher, so they can be turned off wholesale: the router
rewrites (or rejects) llama-swap's `/upstream/<model>/v1/*` before dispatch,
which keeps one implementation per endpoint family and leaves the handlers
unaware of the gateway. they default off so the mock is a plain
openai-compatible server unless a caller opts in.

the audio family (`handlers::audio`) covers pi-omni's voice paths: speech returns
deterministic pcm (raw, or sse `speech.audio.delta` frames when the request asks
for `stream_format: "sse"`), transcriptions returns a fixed `{"text": ...}`, and
voices lists a default voice. responses are deterministic rather than queue-driven
since these paths are exercised for their bytes, not their content.

## programmable responses

the response queue is a FIFO of `ResponseSpec` values. each chat request pops
the next spec; an empty queue falls back to the default response, which is a
fixed minimal SSE stream (`ok` then `[DONE]`) unless overridden via
`--default-response` or `PUT /__admin/default`. so unconfigured callers see the
same behavior as the old python mock, while a test that wants every unscripted
request to return something specific sets the default once. a spec is either high-level (content, tool_calls, finish_reason, usage,
status, delay) which the binary renders as SSE or as a single `chat.completion`
json object depending on the request's `stream` flag, or a raw spec whose bytes
are returned verbatim. specs enter the queue from a startup file
(`--responses`), from stdin (`--stdin`, one json spec per line), or from the
admin api (`POST/PUT /__admin/responses`).

`--consume-only-with-tools` narrows queue consumption to tools-bearing,
non-heartbeat chat requests (the same predicate the stall keys on). title and
heartbeat requests interleaved by an agent loop then get the fallback without
popping a spec, so a programmed sequence stays aligned to the tool-call turns --
needed to replay scenarios where an agent issues a tool call, the tool result
comes back, and only those two turns should draw scripted responses.

## fault injection, timing, and validation

beyond a clean response, a spec can model a failing or slow upstream so a harness
can test its own error and timeout handling:

- `error` renders an openai-style `{"error": {...}}` body (a bare `status` of 200
  becomes 400). this precedes any stream, so even a `stream:true` request gets the
  json error -- matching how a real endpoint rejects before emitting tokens.
- `chunks` renders one content delta per element instead of one combined delta,
  so a client's incremental delta handling is exercised; non-stream joins them.
- `truncate` ends a stream after its content frames with no finish frame or
  `[DONE]`, then closes the socket: a simulated aborted upstream.
- `chunk_delay_ms` sleeps between streamed deltas; `hang_ms` holds the socket open
  after the last frame before closing. the global `connect_delay_ms` behavior
  delays every non-admin request before any bytes. together these drive a
  client's slow-first-byte, idle, and read-timeout paths. a tcp connect timeout is
  out of reach -- a listening socket completes the handshake immediately -- so for
  that case a harness should point at a black-hole address instead.

the `validate_chat` behavior (default off) gates chat requests: a missing `model`
or `messages`, or a missing bearer Authorization header, is rejected with an
openai-style 400/401 before the queue is consumed, so a rejected request leaves
the programmed sequence intact. left off, the endpoint stays permissive.

the `context_window` behavior (default off) models a bounded server context. when
set, the chat handler derives a prompt-token count from the request's serialized
`messages` (at `chars_per_token` characters each, default 4) and, before consuming
a spec, rejects any request over the window with a `context_length_exceeded` 400 --
the llama-server overflow shape, leaving the queue intact for the retry. an
in-window request is served normally but its rendered `usage` is overridden with
the derived prompt (keeping the spec's completion count). the point is the
feedback loop a fixed per-turn usage cannot model: a harness that compacts its own
context keys off the usage a server reports, so a compacted (smaller) request must
report a smaller usage. deriving usage from the request is what makes shrinking the
request shrink the reported usage, and thus what lets a compaction/overflow cycle
be tested end to end.

## introspection

`GET /v1/models` reports a configurable model list (default `fake-model`) so a
provider that probes for models during setup succeeds; `GET /v1/models/{id}`
returns one model object for clients that fetch a specific model. the list is set
with `--models` or `PUT /__admin/models`. this is a superset of the python mock,
which always returned an empty list.

## capture and observability

every non-admin request is recorded as `{path, headers, body}` -- the contract
existing consumers rely on (`body.tools`, heartbeat detection in message
content). captures are readable via `GET /__admin/captures` and, when enabled,
appended as jsonl to a file (`--log-file`) and/or stdout (`--log-stdout`).

## stall and heartbeat

`should_stall` delays the first chat request that carries a non-empty `tools`
list and is not a heartbeat, once per run (a latch reset by
`POST /__admin/reset`). this keeps a harness process alive long enough for a
timer-driven heartbeat request to fire. `is_heartbeat` returns true when any
message content (a string, or a list of parts with a `text` field) contains the
configurable `[heartbeat]` sentinel. both are configurable via cli flags and the
admin api.

## startup announcement

with `--port 0` the os assigns a free port. on startup the binary prints a
single stable line, `base_url=http://<host>:<port>/v1`, so a parent process can
read the url from stdout before driving the server.

## python test client

`clients/python/fakeopenai.py` is a stdlib-only client and process manager that
wraps the startup announcement and the admin api: a `FakeOpenAI` context manager
spawns the binary on a free port, parses `base_url=`, derives the admin url, and
terminates the subprocess on exit; methods cover the admin surface (captures,
program/replace/default/models, behavior, reset) and a module-level
`is_heartbeat_request` mirrors the binary's heartbeat predicate. it is the single
source of spawn/admin glue for every consumer's e2e test, which removes the
copy-pasted helpers those tests used to carry. consumers reference it by the same
`../fake-openai/` sibling path they use for the binary; the client resolves the
binary relative to its own location (so a consumer needs neither the path nor
`FAKE_OPENAI_BIN`). `clients/python/test_fakeopenai.py` spawns the real binary
and exercises the client end to end, doubling as a conformance check that runs in
precommit via `make test-python`.

writing every e2e test in python -- a language other than the rust/ts apps under
test -- is deliberate: an in-process stub in the app's own language can pass even
when the real wiring (config loading, the process entrypoint, the http/ws server)
is broken, whereas a separate process driving the app over its real socket
cannot.
