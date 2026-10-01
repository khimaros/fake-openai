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

## v0.5: image, voice, and gateway surface

enough of the surface for a workflow engine (`flow`) to run its real workflows
against the mock: image generation, voice cloning, and the model metadata a
client's model picker reads. non-standard vendor behavior is opt-in, so the same
mock can prove a client works against a plain openai server.

- [x] `POST /v1/images/generations` returning a real decodable png, honoring `n`
  and `response_format`
- [x] `POST /v1/audio/voices` voice cloning, echoing the requested voice name
- [x] `GET/POST /v1/voices` registry enrolment behind a `consent_attestation`
  gate, so a client can be proven against the crispasr shape too
- [x] the registry remembers what it stored: a duplicate name is 409 until
  `?force=true`, so a re-run's failure is reachable from a test
- [x] model objects carry caller-supplied metadata verbatim (`--models-json`,
  `PUT /__admin/models`), so modality tags and aliases are readable
- [x] opt-in `llamaswap` behavior (default off): `/upstream/<model>/v1/*` routes
  and alias resolution in `/v1/models/{id}`
- [ ] flow: python e2e suite driving the real workflows against the mock

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

- [x] closed-loop context window: derive response usage from the request size and
  return a `context_length_exceeded` 400 when a request exceeds a configurable
  window (`--context-window` / `--chars-per-token`). a fixed per-turn usage cannot
  reproduce a harness's compaction/overflow feedback loop -- where a compacted
  (smaller) request must report a smaller usage, which is what decides whether the
  harness compacts again -- so this closes the loop for context-window testing.
- [x] speech pcm carries POSITION, not just bytes: `/v1/audio/speech` returns a
  deterministic ramp instead of 1536 zero bytes, so a consumer can assert the
  audio it received is the audio that was sent -- in order, unshifted and
  untruncated. silence proves only that something arrived. raised by hmux's
  voice face (phase 36), whose two real decoding hazards are a byte-shifted
  s16_le stream (an odd-length http chunk carried wrong) and a silently dropped
  sse frame; against a zero payload BOTH look identical to success.
- [x] `empty_transcript` behavior: `/v1/audio/transcriptions` answers `{"text": ""}`. a
  transcriber returning NOTHING is a real, ordinary outcome -- whisper's own vad rejects a
  silent clip and yields an empty string, not an error -- and it is a case consumers get wrong:
  hmux's voice face said nothing back, leaving a client wedged in "transcribing" forever. the
  mock could not reproduce it, so nothing caught it.
- [ ] request matching / stubbing by body (wiremock-style), beyond the
  sequential queue
- [ ] web ui built on the admin api

## v0.7: prove the audio was real

- [x] measure the audio a transcription request carries, and expose it in the
      capture as `audio: {bytes, samples, peak, rms}`. the handler currently
      ignores the request entirely (`_req`) and answers a fixed transcript, so a
      caller that commits pure SILENCE gets a transcript, a reply, and a green
      test. that cost a downstream harness months: its emulator's microphone was
      delivering zeros the whole time and every spoken scenario still passed.
- [x] `--reject-silent-audio`, off by default: answer a transcription request
      whose audio is silent with an openai-style 400 instead of a transcript, so
      the failure lands at the moment it happens rather than three layers later.
      off by default because every existing consumer relies on the fixed
      transcript, and requirement 5 must not regress.
- [x] `--require-voice-consent`, off by default: gate `/v1/audio/speech` on a
      `consent_attestation` when the voice is not the preset, so a client that
      attests only at enrolment fails here rather than on its second real call.
      off by default because it is a crispasr extension, not openai.

## v0.8: llama.cpp's native api, for a client that reads probabilities

driven by `verdict`, a decision-model harness that reads the probability mass on
declared option tokens at one position and generates no text. it talks to
llama.cpp's NATIVE api (`/props`, `/tokenize`, `/completion` with `n_probs`) and
not to the openai surface, so nothing here could drive it -- which left that
consumer's e2e tests needing real weights and a live server. they could then only
assert what the model happened to score, never that the client coped with a
distribution the test CHOSE: low mass, a label outside the top k, a backend that
went away mid-decision.

- [x] opt-in `llamacpp` behavior (`--llamacpp`, default off, so the openai
      surface a dozen consumers depend on does not move): `GET /props`,
      `POST /tokenize`, `POST /completion`
- [x] `--chat-template <text>` to configure what `/props` reports (also settable
      at runtime through `/__admin/behavior`, the way `transcript` is), since a client
      derives its prompt affixes from that template and has to be able to be told
      what template it is answering
- [x] a byte-level tokenizer: id == byte, bos 256, eos 257, n_vocab 258. the
      point is that a token id is DERIVABLE from the text, so the ids a client
      resolves by calling `/tokenize` always agree with the ids the scoring
      response reports, and a test scripts "the answer was B" as `B`
- [x] a SECOND response queue for scoring specs (`GET|POST|PUT|DELETE
      /__admin/probs`), each `{probs, cache_n, status, error, delay_ms}`. the chat
      queue cannot carry both: one generated turn issues many scoring calls, and
      interleaving them would make either queue's sequence unreadable. a spec naming
      a token the vocabulary lacks is a 400 and stores nothing; the file and stdin
      channels the chat queue has are not wired up for it yet
- [x] `/completion` renders `completion_probabilities[0].top_logprobs` sorted by
      probability with `logprob = ln(p)`, padded to `min(n_probs, n_vocab)`
      entries. the padding and the truncation are the feature: `n_probs` is how a
      client tells "not in the top k" from "probability zero", and a mock that
      always answers the whole distribution erases the case
- [x] `--default-probs` / `PUT /__admin/default_probs`: the empty-queue scoring
      answer. a client that widens `n_probs` scores one question twice, so a queue
      of one spec is spent on the first read and the second answers something else
      -- which left the widened path untestable until the default existed
- [x] `timings.prompt_n` derived from the request (the same argument as
      `context_window`'s derived usage): a longer prompt reports a longer
      prompt_n, so prefix reuse and cache accounting are assertable at all
- [x] native routes behind the gateway prefix: llama-swap serves them at
      `/upstream/<model>/<path>` with NO `/v1`, which the existing rewrite does
      not strip. gated on `llamaswap` as well, so a client's plain and gateway
      paths are two separate proofs
- [x] native errors as a bare `{"error": "..."}` STRING, unlike openai's error
      object: the two shapes are not interchangeable and a client that renders
      one shows an operator nothing when it reads the other
- [x] verdict: a python e2e suite driving its jev server against the mock, with no
      weights anywhere (`python/tests_e2e/test_fake_backend.py`) -- a formatter
      derived from the configured template, the prompt as the backend received it,
      mass landed off the option labels, a 503 arriving as a retryable 529

known gap: a 258-piece byte vocabulary cannot exercise the wide alphabet, which
needs single-character non-ascii tokens past the 52 ascii letters. either the
vocabulary grows a configurable synthetic range or that path stays model-bound.

