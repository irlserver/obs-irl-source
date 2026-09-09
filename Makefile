# Quality gates. Every tool is invoked with an explicit config out of
# .config/, so `make check` gives the same answer on every machine and in CI.
#
# `make style` is the only target that rewrites files.

CONFIG_DIR = .config
CARGO = cargo

.PHONY: default build check style style-check lint test spell-check tls-provider sim clean

default: check

build:
	$(CARGO) build --release --workspace

check: style-check lint test spell-check tls-provider

style:
	$(CARGO) fmt -- --config-path $(CONFIG_DIR)/rustfmt.toml

style-check:
	$(CARGO) fmt --check -- --config-path $(CONFIG_DIR)/rustfmt.toml

lint:
	$(CARGO) xlint

test:
	$(CARGO) xtest

spell-check:
	codespell --config $(CONFIG_DIR)/codespellrc

# crates/irl-provider asks ureq for the *ring* rustls provider, which ships
# pregenerated assembly and needs no cmake, nasm, perl or go on any runner.
# rustls's own default provider is aws-lc-rs, so one future dependency enabling
# rustls with default features would unify the feature and quietly add a cmake
# requirement to all three CI jobs. Cheaper to assert than to rediscover on a
# red build.
tls-provider:
	@grep -q '^name = "ring"' Cargo.lock \
		|| { echo 'Cargo.lock: expected the ring rustls provider'; exit 1; }
	@! grep -q '^name = "aws-lc-sys"' Cargo.lock \
		|| { echo 'Cargo.lock: aws-lc-sys pulled in; pin rustls back to ring'; exit 1; }
	@echo "  ok    rustls provider is ring"

# The audio speed controller, run closed-loop against a simulated sender.
# Deliberately not part of `check`: it is a design aid, not a gate. Read
# docs/audio-timing-pitfalls.md before touching what it exercises.
sim:
	$(CARGO) run -p irl-core --example speed-controller-sim

clean:
	$(CARGO) clean
