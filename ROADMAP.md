# roadmap

planned work for `fake-openai`. mark items done as they land.

## v0.1: standalone programmable mock

- [x] basic project scaffolding (cargo, mise, makefile, license, doc set, gitignore)
- [x] e2e integration tests over http (written before implementation)
- [x] chat-completions + models handlers with request capture
- [x] programmable response queue via file, stdin, and admin api
- [x] http admin control plane (`/__admin` captures/reset/responses/behavior/health)
- [x] jsonl capture logging to file and/or stdout
- [x] configurable stall + `[heartbeat]` sentinel behavior
- [x] free-port selection and `base_url=` stdout announcement
- [x] provider introspection: configurable `/v1/models` and `/v1/models/{id}`
- [x] sane single default model (`fake-model`) and overridable default
  response (`--default-response`, `/__admin/default`)
- [x] `make precommit` green (clippy + e2e tests + build)
- [x] agent-facing SKILL.md

## v0.2: migrate every consumer off the python mock

- [x] conditional queue consumption: `--consume-only-with-tools` so the queue is
  consumed only by tools-bearing non-heartbeat chat requests (needed to replay
  opencode-evolve's sequenced tool_call -> done scenarios)
- [x] audio endpoints for pi-omni: `POST /v1/audio/speech` (raw pcm and sse
  `speech.audio.delta` frames), `POST /v1/audio/transcriptions` (-> `{text}`),
  `GET /v1/audio/voices`
- [x] scope `/v1/models` so a bare `/v1` 404s with an endpoint listing instead
  of masquerading as a model list
- [x] migrate `airun` integration test onto the binary (model `fake-model`)
- [x] migrate `pi-evolve` integration test (3 scenarios) onto the binary
- [x] migrate `opencode-evolve` tests onto the binary (stall + two sequenced
  tool_call scenarios + plain capture scenarios)
- [x] retire `hcp-spec/testing/mock_openai.py` and its three symlinks

## v0.3: fault injection and timing control

driven by pi-serve's need for deterministic streaming, error, and timeout tests.

- [x] multi-chunk streaming: a `chunks` field renders one SSE content delta per
  element (and joins them for non-stream)
- [x] error injection: an `error` field returns an openai-style error body with
  the given `status` (default 400), for both stream and non-stream requests
- [x] stream truncation: `truncate` ends a stream early (no finish chunk, no
  `[DONE]`) to simulate an aborted upstream
- [x] timing knobs: per-response `chunk_delay_ms` (inter-delta pacing) and
  `hang_ms` (hold open, then close); global `connect_delay_ms` behavior. honest
  about not simulating a tcp connect timeout (a listening socket cannot)
- [x] opt-in request validation: a `validate_chat` behavior rejects malformed or
  unauthenticated chat requests with an openai-style 400/401

## v0.4: shared python test client and an all-python e2e suite

a single stdlib-only python client (`clients/python/fakeopenai.py`) so every
consumer drives the binary the same way, and every e2e test is written in a
language other than the app under test (true black-box -- no in-process stubbing
to silently mask broken wiring).

- [x] `FakeOpenAI` context manager (spawn on a free port, read `base_url=`,
  expose the admin api, terminate on exit) plus `is_heartbeat_request`, with a
  self-test that doubles as a binary conformance check (`make test-python`)
- [ ] migrate every consumer's e2e test onto the client, all in python:
  - [ ] opencode-evolve
  - [ ] airun
  - [ ] pi-evolve
  - [ ] pi-serve (rewrite the in-process ts test as a black-box python test)
- [ ] pi-omni: full voice-loop e2e (chat + stt + tts through one mock) driven
  from a python websocket client
- [ ] pi-webui: chat e2e driven from a python websocket client

## follow-ups

- [ ] request matching / stubbing by body (wiremock-style), beyond the
  sequential queue
- [ ] web ui built on the admin api
