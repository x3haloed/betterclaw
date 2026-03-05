# BetterClaw

**BetterClaw** is a secure, local-first personal AI assistant you run yourself that **learns continuously**.

BetterClaw agents learn and grow over time via:
- Identity externalization
- Attention lensing via early invariant exposure
- RAG with provenance

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache%202.0-blue.svg)](#license)

---

## What BetterClaw Is

BetterClaw is an agent runtime + product surface:

- **Channels:** terminal REPL, web gateway, native Discord + Signal, optional HTTP webhooks, plus WASM channels (Telegram/Slack/etc).
- **Tools:** built-in tools + **WASM tools**, installable via `betterclaw tool …` and `betterclaw registry …`.
- **Security boundaries:** secrets encryption, allowlists/pairing for messaging channels, and tool sandboxing (WASM + optional Docker worker jobs).
- **Durability:** everything important is captured into a **ledger**; background loops build **wake packs** and **recall indexes**.

It is **not** a hosted service. It’s meant to run on your machine (or your server) with your data.

---

## What Makes It Different From Other “-Claw” Agents

BetterClaw’s north star is *durable continuity without turning the model into a historian*:

- **Ledger-first memory:** conversations, tool calls/results, and derived artifacts are events in an append-only ledger (libSQL/SQLite).
- **Two-phase context:** (1) a `wake_pack.v0` snapshot derived by the compressor, then (2) **per-turn ledger recall** injected as *candidate evidence* (with event IDs for citation).
- **Background indexing:** a built-in indexer chunks + embeds new ledger events into `ledger_event_chunks` for hybrid FTS + vector recall.
- **Channel + extension emphasis:** native Discord/Signal for “always-on” usage, and WASM channels/tools for everything else.

Historically this repo started as a fork of IronClaw and borrows patterns from ZeroClaw, but it has diverged substantially in the data plane (ledger + compressor + recall) and channel story.

---

## Quick Start

### 1) Build

```bash
cargo build
```

Tip: run `cargo install --path .` to put `betterclaw` on your `$PATH`. Otherwise use `./target/debug/betterclaw` in the examples below.

### 2) Run the setup wizard

```bash
./target/debug/betterclaw onboard
```

### 3) Run the agent

```bash
./target/debug/betterclaw run
```

Useful commands:

```bash
./target/debug/betterclaw --help
./target/debug/betterclaw doctor
./target/debug/betterclaw status
./target/debug/betterclaw --cli-only
./target/debug/betterclaw --message "Hello!"
```

---

## Channels

Built-in channels (in this repo):

- **CLI REPL** (always enabled)
- **Web gateway** (default: `127.0.0.1:3000`, token-auth; includes SSE/WS + OpenAI-compatible surfaces)
- **Discord** (native gateway websocket integration; allowlists + optional “mention-only” mode)
- **Signal** (via `signal-cli` daemon HTTP API; pairing/allowlist policies)
- **HTTP webhook server** (optional; for inbound webhook-based channels and integrations)

WASM channels (installable, sandboxed):

- Telegram, Slack, WhatsApp, etc. (see `docs/BUILDING_CHANNELS.md` and `docs/TELEGRAM_SETUP.md`)

---

## Extensions (Tools, Channels, MCP)

- **WASM tools:** `betterclaw tool install`, `betterclaw tool list`, `betterclaw tool remove`
- **Registry:** `betterclaw registry …` for browsing/installing extensions
- **MCP:** `betterclaw mcp …` for connecting hosted tool providers

---

## Durability: Ledger, Recall, Wake Packs

BetterClaw maintains three complementary layers:

1. **Ledger (source of truth):** append-only events (turns, tool calls, notes, derived artifacts).
2. **Wake pack (`wake_pack.v0`):** a compact snapshot produced by the compressor to anchor the next turn.
3. **Ledger recall (`<ledger_recall …>`):** per-turn candidate evidence, injected after the wake pack; if used, the agent must cite `event_id`.

For debugging and experimentation:

```bash
./target/debug/betterclaw compressor run-once --window-events 5
./target/debug/betterclaw compressor run-once --window-events 5 --commit
./target/debug/betterclaw compressor reset --dry-run
```

---

## Hacking Guide

Start here:

- Agent loop, recall/indexing: `src/agent/`
- Compressor + wake packs: `src/compressor/`
- Channels (native + web gateway): `src/channels/`
- WASM boundaries (WIT + runtimes): `wit/`, `src/tools/`, `src/channels/wasm/`
- Database + ledger: `src/db/`, `src/ledger/`

---

## Credits

- IronClaw (upstream inspiration / ancestry): <https://github.com/nearai/ironclaw>
- ZeroClaw (Discord patterns + edge cases): <https://github.com/zeroclaw-labs/zeroclaw>
- OpenClaw (workspace inspiration): <https://github.com/openclaw/openclaw>

---

## License

Licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT License (`LICENSE-MIT`)

at your option.
