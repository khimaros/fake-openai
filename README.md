# fake-openai

a standalone, programmable openai-compatible mock for testing agent harnesses.

a harness launches it as a subprocess, points its provider base url at it, drives
a scenario, and inspects what was captured. it replaces the in-process python
mock at `hcp-spec/testing/mock_openai.py` with a language-agnostic process that
is programmable (callers script the response sequence) and observable (every
request is captured and inspectable over http).

## build

```
make            # cargo build
make precommit  # lint + tests + build
```

deterministic builds use `mise` (rust pinned in [mise.toml](mise.toml)). the only
runtime dependencies are `tiny_http` and `serde_json`.

## run

```
fake-openai --port 0
```

on startup it prints a single line a parent process reads to learn the url:

```
base_url=http://127.0.0.1:54321/v1
```

`--port 0` picks a free port. point a harness at the printed url, for example:

```
export OPENAI_BASE_URL=http://127.0.0.1:54321/v1
export OPENAI_API_KEY=test
```

or set the provider `base_url` / `baseURL` in the harness config to the same value.

## endpoints

- `POST /v1/chat/completions` -- returns the next programmed response, or a fixed
  fallback stream when the queue is empty. honors the request's `stream` flag
  (SSE when true or absent, a single `chat.completion` json when false).
- `GET /v1/models` -- lists the configured models (default `fake-model`) for
  provider introspection.
- `GET /v1/models/{id}` -- returns one model object, or 404 if unknown.
- `POST /v1/audio/speech` -- text-to-speech. returns deterministic pcm: raw
  `audio/pcm` bytes, or `text/event-stream` `speech.audio.delta` frames when the
  body sets `stream_format: "sse"`.
- `POST /v1/audio/transcriptions` -- speech-to-text. accepts a multipart upload
  and returns a fixed `{"text": ...}` transcript.
- `GET /v1/audio/voices` -- lists a default voice for onboarding probes.

any other path returns 404 with an `endpoints` listing of what is served (so a
bare `GET /v1` is a discovery aid, not a model list).

## programming responses

callers script the exact sequence of responses through three channels that all
feed one FIFO queue. each chat request pops the next spec; an empty queue falls
back to the fixed stream.

a response spec is either high-level (rendered to SSE or json):

```json
{
  "content": "hello",
  "tool_calls": [],
  "finish_reason": "stop",
  "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
  "status": 200,
  "delay_ms": 0
}
```

or raw (returned verbatim; `content_type` defaults to `text/event-stream`):

```json
{"raw_body": "data: {...}\n\ndata: [DONE]\n\n", "content_type": "text/event-stream", "status": 200}
```

the three channels:

```
# file at startup (json array, single object, or jsonl)
fake-openai --responses responses.json

# stdin in realtime (one json spec per line)
printf '{"content":"first"}\n{"content":"second"}\n' | fake-openai --stdin

# admin api at runtime
curl -X POST http://127.0.0.1:PORT/__admin/responses -d '{"content":"hi"}'
```

the empty-queue fallback defaults to the fixed `ok` stream. override it with
`--default-response '<json spec>'`, or at runtime with `PUT /__admin/default`
(and `DELETE /__admin/default` to restore the built-in).

`--consume-only-with-tools` (or behavior `consume_only_with_tools`) restricts
queue consumption to tools-bearing, non-heartbeat chat requests; title-generation
and heartbeat traffic in between get the fallback without popping a spec. this
lets a caller program a sequence keyed to the tool-call turns of an agent loop.

## fault injection, timing, and validation

a spec can also model a failing or slow upstream, so a harness can test its own
error and timeout handling:

```json
{"chunks": ["one ", "two ", "three"], "chunk_delay_ms": 50}
{"error": {"message": "rate limited", "type": "rate_limit_error"}, "status": 429}
{"content": "half a sente", "truncate": true}
{"content": "...", "hang_ms": 30000, "truncate": true}
```

- `chunks` streams one content delta per element (joined for a non-stream
  request), exercising a client's incremental delta handling.
- `error` returns an openai-style `{"error": {...}}` body with the given `status`
  (a bare 200 becomes 400), even for a streaming request.
- `truncate` ends a stream with no finish frame or `[DONE]`, then closes the
  socket -- a simulated aborted upstream.
- `chunk_delay_ms` paces streamed deltas; `hang_ms` holds the socket open after
  the last frame before closing.

the global `connect_delay_ms` behavior delays every non-admin request before any
bytes (`--connect-delay-ms <n>` or `POST /__admin/behavior`). these drive
slow-first-byte, idle, and read-timeout paths. note a listening socket cannot
simulate a tcp connect timeout; for that, point the client at a black-hole
address instead.

`--validate-chat` (or behavior `validate_chat`, default off) rejects a chat
request missing a `model`, missing `messages`, or lacking a bearer Authorization
header with an openai-style 400/401, before the queue is consumed.

## admin api

the `/__admin` namespace shares the port and is never itself captured.

| method | path | body | result |
|--------|------|------|--------|
| GET | /__admin/health | - | `{ok, captures, pending_responses}` |
| GET | /__admin/captures | - | `{captures: [{path, headers, body}, ...]}` |
| POST | /__admin/reset | - | clear captures + re-arm the stall latch |
| GET | /__admin/responses | - | the pending response queue |
| POST | /__admin/responses | spec or [spec] | append; `{ok, pending}` |
| PUT | /__admin/responses | [spec] | replace the queue |
| DELETE | /__admin/responses | - | clear the queue |
| GET | /__admin/default | - | the fallback spec, or null when built-in |
| PUT | /__admin/default | spec | set the empty-queue fallback |
| DELETE | /__admin/default | - | restore the built-in `ok` fallback |
| GET | /__admin/behavior | - | current stall/heartbeat config |
| POST | /__admin/behavior | partial | merge config (e.g. `{"stall_first_with_tools": true}`) |
| GET | /__admin/models | - | `{models: [...]}` |
| PUT | /__admin/models | [id] or `{models:[id]}` | set the model list |

## capture log

every non-admin request is captured as `{path, headers, body}`. read it via
`GET /__admin/captures`, or mirror it as jsonl:

```
fake-openai --log-file captures.jsonl   # one capture per line
fake-openai --log-stdout                # same jsonl on stdout
```

## stall and heartbeat

`--stall-first-with-tools` delays the first tools-bearing, non-heartbeat chat
request (by `--stall-seconds`, default 5), keeping a harness alive long enough
for a timer-driven heartbeat to fire. a request whose message content contains
the `--heartbeat-sentinel` (default `[heartbeat]`) is exempt. the stall fires
once; `POST /__admin/reset` re-arms it. behavior is also settable at runtime via
`POST /__admin/behavior`.

## flags

run `fake-openai --help` for the full list.

## python test client

[clients/python/fakeopenai.py](clients/python/fakeopenai.py) is a stdlib-only
python client and process manager, so a consumer's end-to-end test drives the
real binary instead of reimplementing the spawn/admin glue. it is the shared
foundation for the ecosystem's e2e tests, which are written in python (a language
other than the app under test) to keep them honestly black-box.

```python
import sys
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent   # the consumer repo (test in tests/)
sys.path.insert(0, str(ROOT.parent / "fake-openai" / "clients" / "python"))
import fakeopenai

if not fakeopenai.available():        # binary not built; skip
    sys.exit(0)

with fakeopenai.FakeOpenAI("--consume-only-with-tools") as fake:
    fake.program([{"tool_calls": [...]}, {"content": "done"}])
    # ...point the app at fake.base_url and drive a scenario...
    for cap in fake.chat_captures():
        assert not fakeopenai.is_heartbeat_request(cap["body"])
```

the binary auto-resolves to this checkout's `target/debug/fake-openai`
(`FAKE_OPENAI_BIN` overrides). `make test-python` runs the client's self-test,
which also serves as a conformance check of the admin api and endpoint contracts.

## license

GPLv3. see [LICENSE](LICENSE).
