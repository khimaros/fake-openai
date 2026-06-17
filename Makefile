build:
	cargo build
.PHONY: build

test:
	cargo test
.PHONY: test

# self-test for the shared python client (clients/python). needs the binary
# built, so precommit runs `build` first.
test-python:
	python3 clients/python/test_fakeopenai.py
.PHONY: test-python

lint:
	cargo check
	cargo clippy
.PHONY: lint

format:
	cargo fmt
.PHONY: format

precommit: lint build test test-python
.PHONY: precommit
