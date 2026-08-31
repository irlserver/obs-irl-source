# Quality gates. Every tool is invoked with an explicit config out of
# .config/, so `make check` gives the same answer on every machine and in CI.
#
# `make style` is the only target that rewrites files.

CONFIG_DIR = .config
CARGO = cargo

.PHONY: default build check style style-check lint test spell-check sim clean

default: check

build:
	$(CARGO) build --release --workspace

check: style-check lint test spell-check

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

# The audio speed controller, run closed-loop against a simulated sender.
# Deliberately not part of `check`: it is a design aid, not a gate. Read
# docs/audio-timing-pitfalls.md before touching what it exercises.
sim:
	$(CARGO) run -p irl-core --example speed-controller-sim

clean:
	$(CARGO) clean
