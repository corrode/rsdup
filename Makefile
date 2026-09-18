.DEFAULT_GOAL := help

CARGO ?= cargo
ARGS ?= --help
SCAN_PATH ?= src

.PHONY: help build release check test fmt fmt-check lint verify run scan doc install clean

help: ## Show available commands
	@awk 'BEGIN { FS = ":.*## " } /^[a-z-]+:.*## / { printf "  %-12s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

build: ## Build the debug binary
	$(CARGO) build --locked

release: ## Build the optimized binary
	$(CARGO) build --locked --release

check: ## Type-check all targets
	$(CARGO) check --locked --all-targets

test: ## Run unit tests and documentation tests
	$(CARGO) test --locked --all-targets
	$(CARGO) test --locked --doc

fmt: ## Format Rust code
	$(CARGO) fmt --all

fmt-check: ## Check formatting without changing files
	$(CARGO) fmt --all -- --check

lint: ## Run Clippy, treating warnings as errors
	$(CARGO) clippy --locked --all-targets -- -D warnings

verify: fmt-check lint test ## Run formatting, lint, and test checks

run: ## Run the CLI (default: --help); override with ARGS='...'
	$(CARGO) run --locked -- $(ARGS)

scan: ## Scan for syntax duplication without a model; override SCAN_PATH=...
	$(CARGO) run --locked -- "$(SCAN_PATH)" --mode near-miss --view summary

doc: ## Build documentation without dependency docs
	$(CARGO) doc --locked --no-deps

install: ## Install rsdup from this checkout into Cargo's bin directory
	$(CARGO) install --path . --locked

clean: ## Remove Cargo build artifacts
	$(CARGO) clean
