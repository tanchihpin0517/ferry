SHELL := /bin/bash

# Locally a clippy finding stays a warning, so a half-finished edit still
# compiles and runs. CI passes `LINT_DENY=-Dwarnings` to turn the same findings
# into a failure; the clippy invocation itself is identical either way.
LINT_DENY ?=

# Empty builds for the host. CI sets a musl target to test the static build
# that Linux releases ship, e.g. `make test TARGET=x86_64-unknown-linux-musl`.
TARGET ?=
TARGET_FLAG := $(if $(TARGET),--target $(TARGET))

.PHONY: all check container-test filesystem-test fmt install-script install-test lint release shellcheck test

all: check

check: fmt lint shellcheck test

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets -- $(LINT_DENY)

# Each test with a time limit, by cargo-nextest, so that one that hangs fails
# by name; see .config/nextest.toml. nextest runs no doctests, so they run
# after. Locally nextest may be missing, and `cargo test` runs instead, with a
# note; CI, which sets `CI`, has it, and a missing one fails there.
test:
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --profile $(if $(CI),ci,default) $(TARGET_FLAG) && \
		cargo test --doc $(TARGET_FLAG); \
	elif [ -n "$(CI)" ]; then \
		echo "test: cargo-nextest not installed" >&2; exit 1; \
	else \
		echo "test: cargo-nextest not installed, so no test has a time limit" >&2; \
		cargo test $(TARGET_FLAG); \
	fi

# Locally shellcheck may be missing, and is then skipped with a note; CI, which
# sets `CI`, has it, and a missing one fails there.
shellcheck:
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck scripts/*.sh; \
	elif [ -n "$(CI)" ]; then \
		echo "shellcheck: not installed" >&2; exit 1; \
	else \
		echo "shellcheck: not installed, skipped" >&2; \
	fi

# `make test` and `ferry test` on another filesystem, made on a loop device,
# e.g. `make filesystem-test FS=xfs`, or FS=ext4-casefold for a directory that
# ignores case. Linux only; needs sudo and mkfs.$(FS), or mkfs.ext4.
FS ?=
filesystem-test:
	@test -n "$(FS)" || { echo "filesystem-test: set FS" >&2; exit 1; }
	scripts/filesystem-test.sh "$(FS)"

# `make test` inside another Linux distribution, e.g.
# `make container-test IMAGE=rockylinux/rockylinux:8`. Needs Docker.
IMAGE ?=
container-test:
	@test -n "$(IMAGE)" || { echo "container-test: set IMAGE" >&2; exit 1; }
	docker run --rm --volume "$(CURDIR):/work:ro" "$(IMAGE)" sh /work/scripts/container-test.sh

# A release binary, as the release workflow builds it, into dist/, e.g.
# `make release TARGET=x86_64-unknown-linux-musl`. `FERRY_RELEASE` makes it a
# release build, which installs on hosts by downloading this version's release.
VERSION := $(shell sed -n 's/^version = "\(.*\)"$$/\1/p' Cargo.toml | head -n 1)
release: install-script
	@test -n "$(TARGET)" || { echo "release: set TARGET" >&2; exit 1; }
	FERRY_RELEASE=1 cargo build --locked --release --target $(TARGET)
	cp target/$(TARGET)/release/ferry dist/ferry-$(VERSION)-$(TARGET)

# The install.sh a release publishes, with its version written in.
install-script:
	mkdir -p dist
	sed 's/@VERSION@/$(VERSION)/' scripts/install.sh > dist/install.sh

# dist/install.sh, installing the binary `make release` put in dist/, served
# from a directory as a release would be, e.g.
# `make install-test TARGET=x86_64-unknown-linux-musl`.
INSTALL_TEST := target/install-test
install-test:
	@test -n "$(TARGET)" || { echo "install-test: set TARGET" >&2; exit 1; }
	rm -rf $(INSTALL_TEST)
	mkdir -p $(INSTALL_TEST)/served
	cp dist/ferry-$(VERSION)-$(TARGET) $(INSTALL_TEST)/served/
	cd $(INSTALL_TEST)/served && shasum -a 256 ferry-* > SHA256SUMS
	FERRY_DOWNLOAD_URL=file://$(CURDIR)/$(INSTALL_TEST)/served \
		FERRY_INSTALL_DIR=$(CURDIR)/$(INSTALL_TEST)/bin sh dist/install.sh
	$(INSTALL_TEST)/bin/ferry --version
