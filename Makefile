SHELL := /bin/bash

# Locally a clippy finding stays a warning, so a half-finished edit still
# compiles and runs. CI passes `LINT_DENY=-Dwarnings` to turn the same findings
# into a failure; the clippy invocation itself is identical either way.
LINT_DENY ?=

# Empty builds for the host. CI sets a musl target to test the static build
# that Linux releases ship, e.g. `make test TARGET=x86_64-unknown-linux-musl`.
TARGET ?=
TARGET_FLAG := $(if $(TARGET),--target $(TARGET))

.PHONY: all check container-test fmt lint test

all: check

check: fmt lint test

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets -- $(LINT_DENY)

test:
	cargo test $(TARGET_FLAG)

# `make test` inside another Linux distribution, e.g.
# `make container-test IMAGE=rockylinux/rockylinux:9`. Needs Docker.
IMAGE ?=
container-test:
	@test -n "$(IMAGE)" || { echo "container-test: set IMAGE" >&2; exit 1; }
	docker run --rm --volume "$(CURDIR):/work:ro" "$(IMAGE)" sh /work/scripts/container-test.sh
