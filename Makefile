SHELL := /usr/bin/env bash
CARGO ?= $(shell command -v cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)
DB ?= $(HOME)/.ai-memory/memory.db

.DEFAULT_GOAL := help

.PHONY: help deploy deploy-ollama deploy-openai deploy-lite demo build build-all test test-python test-ts test-all daemon doctor clean

help: ## Show available AI Memory targets
	@echo "Engram (mem) — Developer & Deployment Targets"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

deploy: ## Launch modern interactive onboarding wizard
	@./install.sh --db "$(DB)"

deploy-ollama: ## One-click local Ollama setup (dev default)
	@./install.sh --mode ollama --db "$(DB)" --yes

deploy-openai: ## One-click production OpenAI setup (requires OPENAI_API_KEY)
	@./install.sh --mode openai --db "$(DB)" --release --yes

deploy-lite: ## One-click zero-model Lite CPU setup
	@./install.sh --mode lite --db "$(DB)" --yes

demo: ## Setup Lite mode with seeded Obsidian Wiki demo & live verification
	@./install.sh --mode lite --db "$(DB)" --seed-demo --yes

build: ## Build release 'mem' binary offline
	@$(CARGO) build --release --offline -p mem-cli

build-all: ## Build release 'mem', 'mem-daemon', and 'mem-fuse' binaries
	@$(CARGO) build --release --offline -p mem-cli -p mem-daemon -p mem-fuse

daemon: ## Start HTTP REST / SSE / MCP daemon on 127.0.0.1:7700
	@$(CARGO) run --offline -p mem-daemon -- --db "$(DB)"

test: ## Run full Rust workspace unit & integration test suite offline
	@$(CARGO) test --workspace --all-targets --offline

test-python: build-all ## Build PyO3 native extension and run Python SDK tests
	@$(CARGO) build --release --manifest-path bindings/python/Cargo.toml
	@cp bindings/python/target/release/lib_native.so bindings/python/python/engram/_native.so 2>/dev/null || cp bindings/python/target/release/lib_native.dylib bindings/python/python/engram/_native.so
	@PYTHONPATH=bindings/python/python MEM_DAEMON_BIN=$(CURDIR)/target/release/mem-daemon python3 -m unittest discover -s bindings/python/tests -v

test-ts: build-all ## Run TypeScript SDK integration tests on Node >= 22
	@MEM_BIN=$(CURDIR)/target/release/mem MEM_DAEMON_BIN=$(CURDIR)/target/release/mem-daemon node --test bindings/typescript/tests/sdk.test.ts

test-all: test test-python test-ts ## Run Rust, Python, and TypeScript test suites

doctor: ## Run health diagnostics on database and configured providers
	@$(CARGO) run --offline -p mem-cli -- --db "$(DB)" doctor

clean: ## Remove Cargo build artifacts
	@$(CARGO) clean
