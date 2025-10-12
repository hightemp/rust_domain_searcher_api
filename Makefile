# Makefile for rust_domain_searcher_api

.DEFAULT_GOAL := help
SHELL := /bin/bash

CARGO ?= cargo
BIN ?= rust_domain_searcher_api
BIN_DIR ?= ./bin
CONFIG ?= ./domain_search.config.yaml
ADDR ?= :8082

.PHONY: help tidy fmt test build run run-debug reset build-linux build-macos build-windows clean
.PHONY: install uninstall purge service-start service-stop service-restart service-status logs

help:
	@echo "Targets:"
	@echo "  make build            - Build $(BIN) into $(BIN_DIR)/"
	@echo "  make run              - Run API (ADDR=$(ADDR), CONFIG=$(CONFIG))"
	@echo "  make run-debug        - Run API with RUST_LOG=debug (ADDR=$(ADDR), CONFIG=$(CONFIG))"
	@echo "  make reset            - Reset storage (delete domain files and state), then exit"
	@echo "  make tidy             - Update Cargo.lock (cargo update)"
	@echo "  make fmt              - cargo fmt"
	@echo "  make test             - cargo test"
	@echo "  make build-linux      - Build Linux amd64 binary"
	@echo "  make build-macos      - Build macOS arm64 binary"
	@echo "  make build-windows    - Build Windows amd64 binary"
	@echo "  make clean            - cargo clean and remove $(BIN_DIR)"
	@echo "  make install          - Build and install as systemd service (binary, config, env, unit) and start it"
	@echo "  make uninstall        - Remove systemd unit and binary (keeps config/data)"
	@echo "  make purge            - Fully remove service, binary, config, data, and user"
	@echo "  make service-start    - Start systemd service"
	@echo "  make service-stop     - Stop systemd service"
	@echo "  make service-restart  - Restart systemd service"
	@echo "  make service-status   - Show systemd service status"
	@echo "  make logs             - Follow service logs via journalctl"
	@echo ""
	@echo "Variables:"
	@echo "  BIN=$(BIN) BIN_DIR=$(BIN_DIR) ADDR=$(ADDR) CONFIG=$(CONFIG)"

tidy:
	$(CARGO) update

fmt:
	$(CARGO) fmt

test:
	$(CARGO) test

build:
	$(CARGO) build --release
	mkdir -p $(BIN_DIR)
	cp target/release/$(BIN) $(BIN_DIR)/$(BIN)

run:
	$(CARGO) run -- --addr $(ADDR) --config $(CONFIG)

run-debug:
	RUST_LOG=debug RUST_BACKTRACE=1 $(CARGO) run -- --addr $(ADDR) --config $(CONFIG)

reset:
	$(CARGO) run -- --config $(CONFIG) --reset

build-linux:
	$(CARGO) build --release --target x86_64-unknown-linux-gnu
	mkdir -p $(BIN_DIR)
	cp target/x86_64-unknown-linux-gnu/release/$(BIN) $(BIN_DIR)/$(BIN)-linux-amd64

build-macos:
	$(CARGO) build --release --target aarch64-apple-darwin
	mkdir -p $(BIN_DIR)
	cp target/aarch64-apple-darwin/release/$(BIN) $(BIN_DIR)/$(BIN)-darwin-arm64

build-windows:
	$(CARGO) build --release --target x86_64-pc-windows-gnu
	mkdir -p $(BIN_DIR)
	cp target/x86_64-pc-windows-gnu/release/$(BIN).exe $(BIN_DIR)/$(BIN)-windows-amd64.exe

clean:
	$(CARGO) clean
	rm -rf $(BIN_DIR)

# --- Deployment (systemd) ---

SUDO ?= sudo
UNIT := ./deploy/$(BIN).service
ENV_SAMPLE := ./deploy/$(BIN).env.sample

BIN_INSTALL := /usr/local/bin/$(BIN)
ETC_DIR := /etc/$(BIN)
LIB_DIR := /var/lib/$(BIN)
ENV_PATH := $(ETC_DIR)/$(BIN).env
CFG_PATH := $(ETC_DIR)/domain_search.config.yaml
USER := $(BIN)
GROUP := $(BIN)

install: build
	$(SUDO) useradd --system --no-create-home --shell /usr/sbin/nologin $(USER) 2>/dev/null || true
	$(SUDO) install -D -m 0755 ./bin/$(BIN) $(BIN_INSTALL)
	$(SUDO) install -D -m 0644 $(UNIT) /etc/systemd/system/$(BIN).service
	$(SUDO) install -d -m 0755 $(ETC_DIR)
	$(SUDO) install -d -m 0755 $(LIB_DIR)
	@if [ ! -f "$(ENV_PATH)" ]; then \
	  $(SUDO) install -m 0644 $(ENV_SAMPLE) $(ENV_PATH); \
	fi
	@if [ ! -f "$(CFG_PATH)" ]; then \
	  if [ -f ./domain_search.config.yaml ]; then SRC=./domain_search.config.yaml; else SRC=../domain_search.config.yaml; fi; \
	  $(SUDO) install -m 0644 $$SRC $(CFG_PATH); \
	fi
	$(SUDO) chown -R $(USER):$(GROUP) $(LIB_DIR)
	$(SUDO) systemctl daemon-reload
	$(SUDO) systemctl enable $(BIN)
	$(SUDO) systemctl restart $(BIN)

uninstall:
	$(SUDO) systemctl disable --now $(BIN) 2>/dev/null || true
	$(SUDO) rm -f /etc/systemd/system/$(BIN).service
	$(SUDO) rm -f $(BIN_INSTALL)
	$(SUDO) systemctl daemon-reload

purge: uninstall
	$(SUDO) rm -rf $(ETC_DIR)
	$(SUDO) rm -rf $(LIB_DIR)
	$(SUDO) userdel $(USER) 2>/dev/null || true

service-start:
	$(SUDO) systemctl start $(BIN)

service-stop:
	$(SUDO) systemctl stop $(BIN)

service-restart:
	$(SUDO) systemctl restart $(BIN)

service-status:
	$(SUDO) systemctl status $(BIN) --no-pager

logs:
	$(SUDO) journalctl -u $(BIN) -e -f