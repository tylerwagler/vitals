# vitals is a Rust binary (rust/); the Python package (src/vitals) is the
# reference implementation it is held to, and the rollback.
#
#   make rust   -> dist/vitals-x86_64, dist/vitals-aarch64 (glibc >= 2.39:
#                  Ubuntu 24.04+, Debian 13, DGX OS)
#   make pyz    -> dist/vitals.pyz, the Python build (python3 >= 3.11)
#   make test   -> Rust unit tests, then the API suite against both builds
#
# The cross builds use cargo-zigbuild (cargo install cargo-zigbuild) with a
# zig on PATH or in ZIG (uv tool install ziglang provides python-zig).

PYTHON ?= python3
GLIBC ?= 2.39
ZIG ?= $(shell command -v zig || command -v python-zig)
RUST_SRC := $(shell find rust/src -name '*.rs') rust/Cargo.toml rust/Cargo.lock

.PHONY: rust pyz test clean
rust: dist/vitals-x86_64 dist/vitals-aarch64
pyz: dist/vitals.pyz

dist/vitals-x86_64: $(RUST_SRC)
	cd rust && CARGO_ZIGBUILD_ZIG_PATH=$(ZIG) cargo zigbuild --release --target x86_64-unknown-linux-gnu.$(GLIBC)
	mkdir -p dist && cp rust/target/x86_64-unknown-linux-gnu/release/vitals $@

dist/vitals-aarch64: $(RUST_SRC)
	cd rust && CARGO_ZIGBUILD_ZIG_PATH=$(ZIG) cargo zigbuild --release --target aarch64-unknown-linux-gnu.$(GLIBC)
	mkdir -p dist && cp rust/target/aarch64-unknown-linux-gnu/release/vitals $@

dist/vitals.pyz: $(shell find src/vitals -name '*.py')
	rm -rf build/pyz && mkdir -p build/pyz dist
	cp -r src/vitals build/pyz/
	find build/pyz -name __pycache__ -prune -exec rm -rf {} +
	$(PYTHON) -m zipapp build/pyz -m 'vitals.cli:main' -p '/usr/bin/env python3' -c -o $@
	@echo "built $@ ($$(stat -c %s $@) bytes)"

test: dist/vitals-x86_64 dist/vitals.pyz
	cd rust && cargo test --release -q
	$(PYTHON) -m pytest -q
	VITALS_BIN=dist/vitals-x86_64 $(PYTHON) -m pytest -q tests/test_api.py
	$(PYTHON) scripts/compare.py "$(PYTHON) dist/vitals.pyz" dist/vitals-x86_64

clean:
	rm -rf build dist rust/target
