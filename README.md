<h1 align="center">BetterClaw</h1>

<p align="center">
  <strong>Your durable personal AI assistant, always on your side</strong>
</p>

<p align="center">
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache%202.0-blue.svg" alt="License: MIT OR Apache-2.0" /></a>
</p>

---

## What This Is

BetterClaw is a Rust fork of BetterClaw, reshaped to prioritize:

- Web gateway + control dashboard (SSE/WS, job/tool visibility)
- Sandbox architecture (orchestrator/worker, WASM tools, capability policy)
- libSQL-first local persistence (no Postgres required for default dev)
- Discord as a first-class channel (ported using ZeroClaw as a reference implementation)
- Durable continuity model centered on claims + isnads + projections (derived, cited, diffable)

This repo is currently in “fork-and-refactor” mode: much of the implementation is still upstream BetterClaw
until we finish reshaping the data plane and routing/policy surfaces.

## Project Direction (High-Level)

- Episode-oriented orchestration with an explicit policy gate (autonomy dial, scopes, witness windows).
- Append-only event ledger as the source of truth (turns, tool calls/results, routing decisions).
- Derived objects (claims + isnads + S/U/R invariants) that must cite ledger event ids.
- Wake pack snapshots built from projections (and diffed) so context wipes don’t reset intent.
- Rule-based routing where model choice is explainable and inspectable (OpenRouter-heavy by default).

⸻

1) System roles diagram (boxes + responsibilities)

┌───────────────────────────┐
│         User / UI          │  (chat, buttons: pin/confirm/reject, browse memory)
└──────────────┬────────────┘
               │
               v
┌───────────────────────────┐
│        Orchestrator        │  (episode loop, policy, tool routing)
│  - decides what to call    │
│  - enforces autonomy dial  │
│  - builds plan + executes  │
└───────┬───────────┬───────┘
        │           │
        │           │
        v           v
┌───────────────┐  ┌───────────────────────┐
│  Prompt Packs  │  │     Tool Harness      │
│ (versioned)    │  │ (email, files, etc.)  │
│ - agent prompt │  │ logs calls+results    │
│ - compressor   │  └─────────┬─────────────┘
│ - verifier     │            │
└───────┬───────┘            │
        │                    │
        v                    v
┌───────────────────────────┐
│        LLM Provider        │  (one or more models)
│  - BUZZ "voice" model      │
│  - Compressor model        │
│  - Verifier/Reranker       │
└──────────────┬────────────┘
               │
               v
┌──────────────────────────────────────────────────────────┐
│                       Data Plane                         │
│  ┌──────────────────────┐   ┌─────────────────────────┐  │
│  │ Append-only Ledger    │   │   Retrieval Indexes     │  │
│  │ (events + transcripts │   │ - exact (entities/time) │  │
│  │  + tool logs)         │   │ - vector (embeddings)   │  │
│  └──────────┬───────────┘   └───────────┬─────────────┘  │
│             │                           │                │
│             v                           v                │
│  ┌──────────────────────┐   ┌─────────────────────────┐  │
│  │ Claims + Isnads       │   │ Projections / Views     │  │
│  │ (derived, cited)      │   │ - invariants (S/U/R)    │  │
│  │ + transform_id        │   │ - drift/contradictions  │  │
│  └──────────┬───────────┘   │ - wake pack snapshots   │  │
│             │               └───────────┬─────────────┘  │
│             └───────────────────────────┘                │
└──────────────────────────────────────────────────────────┘

S/U/R = Self / User / Relationship

⸻

2) Runtime episode flow (what happens when you “wake” and chat)

[Start Episode]
     |
     v
[Load Policy State]
(autonomy dial, scopes, thresholds)
     |
     v
[Build Wake Pack]
- top invariants (S/U/R)
- drift alerts
- active goals
- relevant recent snippets
     |
     v
[User message arrives]
     |
     v
[Capture Event -> Ledger]
(conversation.turn + hash + metadata)
     |
     v
[Recall Step (hybrid)]
- exact index lookup
- vector candidate search
- rerank/verify
     |
     v
[Compose Context]
Wake Pack + recalled evidence + current turn
     |
     v
[Call BUZZ LLM (voice/agent)]
(prompt pack: agent)
     |
     v
[Does response propose an action?]----No---->[Respond to user]
               |
              Yes
               |
               v
[Policy Gate + Preflight]
- scope allowed?
- confidence ok?
- drift low?
- witness window needed?
               |
       ┌───────┴────────┐
       │ pass            │ fail
       v                 v
[Execute Tool]        [Downgrade]
(log tool.call/result) (propose-only / ask user)
       |
       v
[Capture Tool Result -> Ledger]
       |
       v
[Optional Micro-Distill Trigger]
(decision/correction/action happened)
       |
       v
[Update Projections + Wake Snapshot]
       |
       v
[Continue Episode / End Episode]


⸻

3) CIL distillation flow (background / end-of-episode “compressor”)

[Trigger Distill]
- end of episode
- after decision/correction/action
- daily macro run
     |
     v
[Select Window]
(last N events / last day / last week)
     |
     v
[Gather Evidence Set]
- relevant events
- relevant prior invariants
- contradictions/drift candidates
     |
     v
[Call Compressor LLM]
(prompt pack: compressor)
(output via CFG schema)
     |
     v
[Create Claims + Isnads]
- each invariant must cite event_ids (+ offsets)
- attach transform_id (prompt/version)
     |
     v
[Verifier Pass]
- schema validity
- citations exist
- no uncited assertions
- optional second-model check
     |
     v
[Commit Derived Objects]
- invariants (S/U/R)
- fact updates (with citations)
- drift flags / contradictions
     |
     v
[Reweight + Prune (macro only)]
- merge duplicates
- decay stale
- mark contradicted (never delete)
     |
     v
[Build New Wake Snapshot]
- diff vs previous
- if big diff -> tighten autonomy
     |
     v
[Done]

## Quick Start (libSQL dev build)

BetterClaw keeps BetterClaw’s CLI for now, so commands still use `betterclaw` until we rename the binary.

Build with libSQL only:

```bash
cargo build --no-default-features --features libsql
```

Onboard:

```bash
./target/debug/betterclaw onboard
```

Then run the gateway (exact flags/config are still upstream; see `docs/` and `.env.example`).

## Where To Look (When Hacking)

- Orchestration / worker bridge: `src/orchestrator/`
- Tool registry + schemas + sandboxed tools: `src/tools/` and `wit/tool.wit`
- Web gateway (UI/API, SSE/WS): `src/channels/web/`
- Workspace + retrieval: `src/workspace/` (note: libSQL vector search wiring is currently limited)

## Discord

Discord is not yet wired in this fork. The plan is:

- Implement a native Rust Discord channel using BetterClaw’s channel manager interfaces.
- Use ZeroClaw’s Discord implementation as a reference for lifecycle and edge cases.

## Upstream Credit

- IronClaw (upstream base): [nearai/ironclaw](https://github.com/nearai/ironclaw)
- ZeroClaw (reference for Discord patterns): [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)

BetterClaw itself is inspired by OpenClaw; see `FEATURE_PARITY.md` for upstream tracking.

## License

Licensed under either of:

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT License (`LICENSE-MIT`)

at your option.
