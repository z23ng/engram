SHELL := /usr/bin/env bash
CARGO ?= $(shell command -v cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)
DB ?= $(HOME)/.ai-memory/memory.db

.DEFAULT_GOAL := help

.PHONY: help deploy deploy-ollama deploy-openai deploy-lite demo build test doctor clean

help: ## Show available AI Memory targets
	@echo "AI Memory (mem) — Developer & Deployment Targets"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

deploy: ## Launch interactive one-click deployment wizard
	@./scripts/deploy.sh --db "$(DB)"

deploy-ollama: ## One-click local Ollama deployment (dev default)
	@./scripts/deploy.sh --mode ollama --db "$(DB)"

deploy-openai: ## One-click production OpenAI deployment (requires OPENAI_API_KEY)
	@./scripts/deploy.sh --mode openai --db "$(DB)" --release

deploy-lite: ## One-click zero-model Lite CPU deployment
	@./scripts/deploy.sh --mode lite --db "$(DB)"

demo: ## Deploy in Lite/Local mode with seeded Obsidian Wiki demo & live search
	@./scripts/deploy.sh --mode lite --db "$(DB)" --seed-demo

build: ## Build release 'mem' binary offline
	@$(CARGO) build --release --offline -p mem-cli

test: ## Run full Rust workspace unit & integration test suite offline
	@$(CARGO) test --offline

doctor: ## Run health diagnostics on database and configured providers
	@$(CARGO) run --offline -p mem-cli -- --db "$(DB)" doctor

clean: ## Remove Cargo build artifacts
	@$(CARGO) clean
