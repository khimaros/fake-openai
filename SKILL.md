---
name: fake-openai
description: Launch and drive the fake-openai mock to capture and assert what an agent harness sends to an openai-compatible endpoint, and to program the responses it receives. Use when writing or debugging an integration test that needs a deterministic, inspectable LLM endpoint (chat-completions and provider introspection), or when reproducing what request a harness issues under a given config.
---

# fake-openai

a standalone binary that mocks an openai-compatible api. launch it, point a
harness at it, program the responses it returns, and inspect every request it
received.

## launch and learn the url

build once with `make`, then run it and read the announced url from stdout:

```
./target/debug/fake-openai --port 0 &
# stdout prints exactly one line:
# base_url=http://127.0.0.1:54321/v1
```

`--port 0` picks a free port. parse the `base_url=` line to learn it. the admin
api lives at the same host:port under `/__admin` (without the `/v1` suffix).

## point a harness at it

set the provider base url and a throwaway key:

```
export OPENAI_BASE_URL=http://127.0.0.1:54321/v1
export OPENAI_API_KEY=test
```

or set the harness config's provider `base_url` / `baseURL` to that value. the
mock answers `GET /v1/models` (default model `fake-model`) so provider
introspection succeeds; pass `--models a,b` to advertise specific ids.

## program the responses

each `POST /v1/chat/completions` pops the next spec from a FIFO queue; an empty
queue returns a fixed fallback stream (`ok` then `[DONE]`). queue specs three
ways:

```
# at startup, from a file (json array, object, or jsonl)
fake-openai --responses responses.json

# in realtime, from stdin (one json spec per line)
printf '{"content":"hi"}\n' | fake-openai --stdin

# at runtime, via the admin api
curl -X POST http://HOST:PORT/__admin/responses -d '{"content":"hi"}'
```

a spec is high-level (rendered to SSE when the request has `stream:true`/absent,
or to a single `chat.completion` json when `stream:false`):

```json
{"content": "hi", "tool_calls": [], "finish_reason": "stop", "delay_ms": 0, "status": 200}
```

or raw for full control (`raw_body` returned verbatim; `content_type` defaults to
`text/event-stream`):

```json
{"raw_body": "data: {...}\n\ndata: [DONE]\n\n", "content_type": "text/event-stream"}
```

`PUT /__admin/responses` replaces the whole queue; `DELETE` clears it; `GET`
shows what is pending.

the empty-queue fallback defaults to a fixed `ok` stream. to change what an
unscripted request returns, set a default response with `--default-response
'<spec>'` or `PUT /__admin/default <spec>` (`DELETE /__admin/default` restores
the built-in). model introspection defaults to a single model `fake-model`;
override with `--models a,b` or `PUT /__admin/models`.

## inspect what the harness sent

```
curl http://HOST:PORT/__admin/captures
# {"captures": [{"path": "/v1/chat/completions", "headers": {...}, "body": {...}}, ...]}
```

each capture is `{path, headers, body}`. filter by `body.tools` to find
tools-bearing requests; detect heartbeats by the `[heartbeat]` substring in a
message's content (a string, or a list of parts with a `text` field). you can
also stream captures as jsonl with `--log-file captures.jsonl` or `--log-stdout`.

## configure behavior and reset

```
# stall the first tools-bearing, non-heartbeat request (keeps a harness alive
# for a heartbeat tick); fires once.
fake-openai --stall-first-with-tools --stall-seconds 5
# or at runtime:
curl -X POST http://HOST:PORT/__admin/behavior -d '{"stall_first_with_tools": true, "stall_seconds": 5}'

# between scenarios: clear captures and re-arm the stall latch.
curl -X POST http://HOST:PORT/__admin/reset
```

## inject faults and control timing

a spec can model a failing or slow upstream so a harness can test its own error
and timeout handling:

```json
{"chunks": ["one ", "two ", "three"], "chunk_delay_ms": 50}
{"error": {"message": "rate limited", "type": "rate_limit_error"}, "status": 429}
{"content": "half a sente", "truncate": true}
{"content": "...", "hang_ms": 30000, "truncate": true}
```

- `chunks`: stream one content delta per element (joined for a non-stream request).
- `error`: return an openai `{"error": {...}}` body with `status` (a bare 200
  becomes 400), even for a streaming request.
- `truncate`: end a stream with no finish frame or `[DONE]`, then close the socket.
- `chunk_delay_ms` / `hang_ms`: pace deltas / hold the socket open before closing.

global timing and validation are behaviors (cli flag or `POST /__admin/behavior`):

```
fake-openai --connect-delay-ms 200   # delay every non-admin request before any bytes
fake-openai --validate-chat          # reject chat reqs missing model/messages/bearer auth (400/401)
```

## drive it from python tests

for an integration test, prefer the bundled client over reimplementing the
spawn/admin glue: `clients/python/fakeopenai.py` (stdlib-only) wraps all of the
above.

```python
import fakeopenai  # from ../fake-openai/clients/python on sys.path

if not fakeopenai.available():        # binary not built -> skip the test
    return
with fakeopenai.FakeOpenAI("--consume-only-with-tools") as fake:
    fake.program([{"content": "hi"}])          # POST /__admin/responses
    fake.behavior(validate_chat=True)          # POST /__admin/behavior
    # ...point the app at fake.base_url, run the scenario...
    caps = fake.chat_captures()                # GET /__admin/captures, chat only
    fake.reset()                               # clear captures, re-arm the stall
```

the client auto-resolves the binary in its own checkout (`FAKE_OPENAI_BIN`
overrides). write the test in python even when the app is rust or ts: a
different-language process driving the real socket stays honestly black-box.

## gotchas

- admin requests (`/__admin/*`) are never captured.
- the stall is one-shot per arming; call `/__admin/reset` to fire it again.
- an empty queue always returns the fixed fallback SSE, regardless of `stream`.
- stdin specs are processed asynchronously; poll `GET /__admin/responses` until
  the expected count appears before sending the matching chat request.
- `connect_delay_ms`/`hang_ms` exercise slow-first-byte and idle/read timeouts,
  NOT a tcp connect timeout: a listening socket completes the handshake at once.
  for a real connect timeout, point the client at a black-hole address.
