# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What is filest

Simple REST file server with HTTP/1.1, HTTP/2, and HTTP/3 (QUIC) support. Written in Rust (edition 2024) using axum, tokio, quinn, and h3.

## Build & Run

```bash
cargo build
cargo run              # requires NS_* env vars
cargo test
cargo clippy
```

## Configuration

All via environment variables (or `.env` file via dotenv):

- `LISTEN_ADDR` — bind address (default `0.0.0.0:8090`), used for both TCP and UDP (QUIC)
- `NS_<NAME>=<path>` — register a namespace; at least one required. Name is lowercased.
- `CERT_PATH` / `KEY_PATH` — optional TLS certs (PEM). Without them: TCP serves plain HTTP, QUIC uses auto-generated self-signed certs.

## Architecture

Single-file server (`src/main.rs`). No modules yet.

- **Namespaces**: files are organized by namespace prefix in the URL (`/{namespace}/path/to/file`). Each namespace maps to a directory on disk via `NS_*` env vars.
- **Routing**: single catch-all route `/{*path}` dispatched by HTTP method:
  - `GET` — serve file or list directory (JSON)
  - `PUT` — upload/overwrite file (creates parent dirs)
  - `DELETE` — remove file
  - `PATCH` — rename (JSON body `{"destination": "new/path"}`, same namespace)
- **Transport**: TCP listener (HTTP/1.1 + H2 via ALPN when TLS) and QUIC endpoint (HTTP/3) run concurrently on the same address (TCP vs UDP).
- **Path traversal protection**: rejects `..` in paths.

## Deploy

FreeBSD deploy scripts in `deploy/` — rc script + env file + install script.
