#!/usr/bin/env python3
"""self-test for the fakeopenai python client. spawns the real binary and drives
it through the helper, so it doubles as a conformance check of the admin api and
the endpoint contracts the ecosystem's tests depend on.

run from anywhere: `python3 clients/python/test_fakeopenai.py` (or via
`make test-python`). skips cleanly when the binary is not built."""

import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import fakeopenai

PASS = FAIL = 0


def check(desc, ok, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"PASS: {desc}")
    else:
        FAIL += 1
        print(f"FAIL: {desc}")
        if detail:
            print(f"  {detail}")


def http(method, url, body=None, headers=None):
    """raw request to the mock; returns (status, content_type, bytes)."""
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.status, r.headers.get("Content-Type", ""), r.read()


def chat(base_url, **over):
    """non-stream chat request; returns the parsed completion json."""
    body = {"model": "fake-model", "messages": [{"role": "user", "content": "hi"}],
            "stream": False, **over}
    _, _, raw = http("POST", base_url + "/chat/completions", body,
                     {"Content-Type": "application/json", "Authorization": "Bearer test"})
    return json.loads(raw)


def main():
    if not fakeopenai.available():
        print(f"SKIP: fake-openai binary not found at {fakeopenai.BIN}; run `make build`")
        return 0

    with fakeopenai.FakeOpenAI() as fake:
        check("announces a /v1 base_url", (fake.base_url or "").endswith("/v1"), fake.base_url)
        check("health reports ok", fake.health().get("ok") is True)

        # programmed response is consumed in order, then the queue drains.
        fake.program([{"content": "first"}, {"content": "second"}])
        check("first programmed spec renders", chat(fake.base_url)["choices"][0]["message"]["content"] == "first")
        check("second programmed spec renders", chat(fake.base_url)["choices"][0]["message"]["content"] == "second")
        # an empty queue falls back to the built-in ok stream (always sse).
        _, ct, raw = http("POST", fake.base_url + "/chat/completions",
                          {"model": "fake-model", "messages": [{"role": "user", "content": "hi"}]},
                          {"Content-Type": "application/json"})
        check("empty queue falls back to ok stream",
              "text/event-stream" in ct and '"content":"ok"' in raw.decode(), ct)

        caps = fake.chat_captures()
        check("captures the chat requests", len(caps) == 3, f"got {len(caps)}")
        check("capture preserves the request body", caps[0]["body"].get("model") == "fake-model")

        fake.reset()
        check("reset clears captures", fake.captures() == [])

    # is_heartbeat_request mirrors the binary's predicate (string + list-of-parts).
    check("heartbeat detected in string content",
          fakeopenai.is_heartbeat_request({"messages": [{"content": "ping [heartbeat]"}]}))
    check("heartbeat detected in list-of-parts content",
          fakeopenai.is_heartbeat_request({"messages": [{"content": [{"type": "text", "text": "[heartbeat] now"}]}]}))
    check("non-heartbeat is not flagged",
          not fakeopenai.is_heartbeat_request({"messages": [{"content": "hello"}]}))

    # audio endpoints (pi-omni's surface).
    with fakeopenai.FakeOpenAI() as fake:
        au = fake.base_url + "/audio"
        _, ct, blob = http("POST", au + "/speech",
                           {"input": "hi", "model": "tts-1", "voice": "alloy"},
                           {"Content-Type": "application/json"})
        check("tts raw returns audio/pcm bytes", "audio/pcm" in ct and len(blob) > 0, ct)

        _, ct, blob = http("POST", au + "/speech",
                           {"input": "hi", "model": "tts-1", "voice": "alloy", "stream_format": "sse"},
                           {"Content-Type": "application/json"})
        text = blob.decode()
        check("tts sse returns event-stream delta frames",
              "text/event-stream" in ct and "speech.audio.delta" in text, ct)

        _, _, blob = http("POST", au + "/transcriptions", b"not-a-real-multipart",
                          {"Content-Type": "multipart/form-data"})
        check("stt returns a fixed transcript", json.loads(blob).get("text"))

        # the query param pi-omni sends on its first voices probe must survive.
        st, _, blob = http("GET", au + "/voices?model=tts-1")
        check("voices tolerates a ?model= query", st == 200 and json.loads(blob).get("voices"))

    # llama.cpp's native family, which a decision client reads probabilities out
    # of. it hangs off the origin and not off /v1, which is what root_url is for.
    with fakeopenai.FakeOpenAI("--llamacpp") as fake:
        root = fake.root_url

        def score(n_probs=8):
            _, _, raw = http("POST", root + "/completion",
                             {"prompt": "hi", "n_probs": n_probs},
                             {"Content-Type": "application/json"})
            return json.loads(raw)["completion_probabilities"][0]["top_logprobs"][0]

        _, _, blob = http("GET", root + "/props")
        check("props reports a chat_template", "chat_template" in json.loads(blob))

        _, _, blob = http("POST", root + "/tokenize", {"content": "A B"},
                         {"Content-Type": "application/json"})
        check("tokenize is byte level", json.loads(blob).get("tokens") == [65, 32, 66])

        _, _, blob = http("POST", root + "/completion",
                         {"prompt": "hi", "n_probs": 8},
                         {"Content-Type": "application/json"})
        top = json.loads(blob)["completion_probabilities"][0]["top_logprobs"][0]
        check("an unprogrammed scoring call answers the first label",
              top["id"] == 65 and top["prob"] == 1.0, str(top))

        fake.program_probs({"probs": {"A": 0.25, "B": 0.75}})
        check("the scoring queue is inspectable", len(fake.probs()) == 1)
        _, _, blob = http("POST", root + "/completion",
                         {"prompt": "hi", "n_probs": 8},
                         {"Content-Type": "application/json"})
        top = json.loads(blob)["completion_probabilities"][0]["top_logprobs"][0]
        check("the programmed distribution is what came back",
              top["id"] == 66 and abs(top["prob"] - 0.75) < 1e-9, str(top))

        # n_probs is how a client tells "outside the top k" from "zero", so the
        # width the caller asked for is part of the contract.
        _, _, blob = http("POST", root + "/completion",
                         {"prompt": "hi", "n_probs": 1},
                         {"Content-Type": "application/json"})
        check("n_probs is honored",
              len(json.loads(blob)["completion_probabilities"][0]["top_logprobs"]) == 1)

        try:
            fake.program_probs({"probs": {"not a token": 1.0}})
            check("an unknown token is refused", False, "the spec was accepted")
        except urllib.error.HTTPError as e:
            check("an unknown token is refused", e.code == 400, f"status {e.code}")
        check("a refused spec is not queued", len(fake.probs()) == 0)

        fake.clear_probs()
        _, _, blob = http("POST", root + "/completion",
                         {"prompt": "hi", "n_probs": 8},
                         {"Content-Type": "application/json"})
        top = json.loads(blob)["completion_probabilities"][0]["top_logprobs"][0]
        check("clear_probs restores the built-in answer", top["id"] == 65, str(top))

        # the empty-queue default, which is what a client that widens n_probs -- and
        # so scores one question twice -- actually needs.
        fake.set_default_probs({"probs": {"A": 0.1, "B": 0.9}})
        first = score()
        check("the default scoring answer applies",
              first["id"] == 66 and abs(first["prob"] - 0.9) < 1e-9, str(first))
        check("and to every read, not just the first", score()["id"] == 66)
        fake.clear_default_probs()
        check("clear_default_probs restores the built-in answer", score()["id"] == 65)

    with fakeopenai.FakeOpenAI() as fake:
        # urlopen raises on 4xx, which is the answer wanted here.
        try:
            st, _, _ = http("GET", fake.root_url + "/props")
            check("native endpoints stay closed without --llamacpp", False, f"status {st}")
        except urllib.error.HTTPError as e:
            check("native endpoints stay closed without --llamacpp", e.code == 404,
                  f"status {e.code}")

    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
