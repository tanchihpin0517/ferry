SHELL := /bin/bash

# Locally a clippy finding stays a warning, so a half-finished edit still
# compiles and runs. CI passes `LINT_DENY=-Dwarnings` to turn the same findings
# into a failure; the clippy invocation itself is identical either way.
LINT_DENY ?=

# Empty builds for the host. CI sets a musl target to test the static build
# that Linux releases ship, e.g. `make test TARGET=x86_64-unknown-linux-musl`.
TARGET ?=
TARGET_FLAG := $(if $(TARGET),--target $(TARGET))

.PHONY: all check fmt lint test

all: check

check: fmt lint test

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets -- $(LINT_DENY)

test:
	cargo test $(TARGET_FLAG)
