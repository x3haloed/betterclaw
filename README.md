# BetterClaw

BetterClaw is a host-native, event-driven agent runtime for real work.

It is being rebuilt from scratch around a few simple ideas:

- Agents should have one stable identity.
- Workspaces should behave like normal directories.
- Tools should be explicit, typed, and easy to debug.
- Channels should be thin adapters, not miniature operating systems.
- Logs should make failures obvious instead of mysterious.

This project exists because too many agent systems make basic behavior hard to reason about. They hide important state behind layers of fallback logic, sandbox indirection, and magical recovery paths that turn simple bugs into archaeology.

BetterClaw is the opposite kind of system.

## Tidepool

Tidepool is now a first-class native channel in BetterClaw.

See [`/Users/chad/Repos/betterclaw/docs/tidepool.md`](/Users/chad/Repos/betterclaw/docs/tidepool.md) for:

- the checked-in SpacetimeDB bindings refresh workflow
- Tidepool runtime env/config
- the two-agent Tidepool evaluation harness

## Model Providers

BetterClaw now supports provider selection by **wire family**:

- OpenAI-compatible `/chat/completions`
- OpenAI-compatible `/responses`

Current presets:

- `BETTERCLAW_PROVIDER=local`
  Uses the local OpenAI-compatible chat-completions path. This is the default and keeps LM Studio working as before.
- `BETTERCLAW_PROVIDER=openrouter`
  Uses OpenRouter with `OPENROUTER_API_KEY`. By default this uses chat-completions; set `BETTERCLAW_PROVIDER_MODE=responses` to use the shared Responses engine instead.
- `BETTERCLAW_PROVIDER=codex`
  Uses the shared Responses engine with Codex auth loaded from `OPENAI_CODEX_AUTH_PATH` (default: `~/.codex/auth.json`).
- `BETTERCLAW_PROVIDER=copilot`
  Uses the shared Responses engine against GitHub Copilot. Prefer `GITHUB_COPILOT_API_TOKEN`. If you need refreshable auth, point `BETTERCLAW_COPILOT_TOKEN_COMMAND` at a helper that prints a fresh bearer token on stdout.

Common env:

- `BETTERCLAW_MODEL`
- `BETTERCLAW_MODEL_BASE_URL`

OpenRouter-specific env:

- `OPENROUTER_MODEL`
- `OPENROUTER_BASE_URL`
- `OPENROUTER_API_KEY`
- `OPENROUTER_HTTP_REFERER`
- `OPENROUTER_X_TITLE`

Codex-specific env:

- `OPENAI_CODEX_MODEL`
- `OPENAI_CODEX_BASE_URL`
- `OPENAI_CODEX_AUTH_PATH`

Copilot-specific env:

- `COPILOT_MODEL`
- `COPILOT_API_URL`
- `GITHUB_COPILOT_API_TOKEN`
- `BETTERCLAW_COPILOT_TOKEN`
- `BETTERCLAW_COPILOT_TOKEN_COMMAND`
- `GITHUB_COPILOT_INTEGRATION_ID`
- `COPILOT_INTEGRATION_ID`

Notes:

- BetterClaw does not auto-load `.env`; your launcher must export these variables.
- Interactive `copilot` CLI login is not enough by itself for the raw HTTP provider path here. BetterClaw needs either a direct bearer token or a helper command that can supply one.

See [`/Users/chad/Repos/betterclaw/docs/provider-engine.md`](/Users/chad/Repos/betterclaw/docs/provider-engine.md) for the architecture behind the provider split.

## What We Are Building

BetterClaw is a runtime for long-lived agents that:

- receive events from channels like web chat, Telegram, Tidepool, and local automation
- maintain durable threads and workspace state
- call normal host tools directly
- produce clear outbound replies and actions
- emit excellent structured logs for every important step

At a high level:

1. A channel receives or polls an inbound event.
2. The event is normalized into an agent turn.
3. The runtime assembles context for the thread and workspace.
4. The model either responds directly or issues explicit tool calls.
5. Tools execute on the host with predictable inputs and outputs.
6. Results are recorded to the thread timeline.
7. The channel sends the final reply or action.

## Design Principles

### 1. Host-Native By Default

Tools run on the host as normal programs and services.

We do not want complicated WASM packaging, hidden sidecar runtimes, or sandbox theater unless there is a very specific reason to add one later.

If a tool is available, it should be visible, callable, and debuggable with ordinary developer workflows.

### 2. Explicit Tool Calls Only

A tool call is either valid or invalid.

We do not silently coerce malformed calls into `{}`.
We do not invent missing arguments.
We do not pretend a tool invocation happened when it did not.

If the model emits invalid JSON or incomplete parameters, the runtime should say exactly what was wrong and where parsing failed.

### 3. One Identity Model

An agent should not become a different being depending on which channel or domain woke it up.

Thread routing, channel targeting, and workspace selection should all be explicit and separable, but the underlying agent identity should remain stable.

### 4. One Workspace Model

Files should resolve the same way across tools.

If the agent is working in a directory, every file tool should agree on what relative paths mean. No split brain between shell cwd, file tools, and hidden internal roots.

### 5. Channels Are Adapters

A channel should do only a few things:

- authenticate
- poll or receive inbound events
- persist cursors/checkpoints
- translate events into runtime messages
- send replies back out

Channels should not own agent identity, hidden business logic, or mystery state transitions.

### 6. Logging Is A Product Feature

The logs should answer:

- What woke the agent up?
- What thread did this map to?
- What exact payload did we send to the model provider?
- What exact payload did the model provider return?
- What tool was called with what parameters?
- What failed?
- What got retried?
- What cursor advanced?
- What message was sent out?

If a human cannot reconstruct a failure from the logs, the logging is not good enough.

The most important record in the system is the model loop itself.

Every request to the model provider should be logged in full, and every response from the model provider should be logged in full. That is the core debugging artifact for agent behavior.

The challenge is not whether to log it. The challenge is how to store it responsibly:

- structured metadata for indexing and filtering
- full payload capture for replay and diagnosis
- compression and retention policies so logs do not consume the machine
- clear separation between hot operational logs and archived trace data

## Non-Goals

BetterClaw is intentionally not trying to be:

- a general-purpose sandbox platform
- a WASM extension host
- a magical self-healing tool interpreter
- a framework that hides state transitions behind “smart” abstractions

We want a runtime that is boring in the best possible way: direct, inspectable, and reliable.

## Core Concepts

### Agent

A long-lived identity with configuration, memory, and access to one or more threads and tools.

### Thread

A durable conversation timeline. Threads are where messages, tool calls, tool results, and system events are recorded.

### Workspace

A filesystem root attached to an agent or thread. It is where file-based work happens.

### Tool

A named capability with:

- a description
- a parameter schema
- a normal execution implementation
- a typed result or typed error

### Channel

An inbound/outbound adapter that connects the runtime to an external surface.

### Event

A normalized runtime record for something that happened:

- inbound message
- model response
- tool call
- tool result
- cursor update
- outbound reply
- error

## Initial Architecture

The first version should stay small.

### Runtime

Owns:

- agent registry
- thread store
- tool registry
- channel registry
- event log

### LLM Layer

Responsible for:

- prompt assembly
- model invocation
- parsing structured tool calls
- returning plain responses or explicit tool calls

It should not mutate channel state or invent missing tool inputs.

### Tool Layer

Responsible for:

- validating parameters
- executing host-native actions
- returning structured outputs
- emitting clear failures

### Channel Layer

Responsible for:

- external API integration
- polling/webhooks
- cursor persistence
- routing outbound messages

### Observability Layer

Responsible for:

- structured logs
- per-turn traces
- durable event history
- debugging views
- full model request/response capture with retention controls

## First Milestones

### Milestone 1: Skeleton Runtime

- basic project structure
- structured logger
- event model
- thread model
- minimal agent loop

### Milestone 2: Local Tools

- shell
- read_file
- write_file
- list_dir
- apply_patch

All with consistent workspace semantics and clear errors.

### Milestone 3: One Channel

Start with a single channel and make it excellent.

Tidepool is a strong candidate because it exercises:

- polling
- cursors
- routing
- durable thread state
- structured outbound replies

### Milestone 4: Usable Debugging

- per-turn timeline output
- channel cursor inspection
- tool-call inspection
- raw model response capture
- replay for failed turns

## Implementation Taste

We should prefer:

- simple data models
- obvious boundaries
- typed errors
- append-only event thinking where practical
- fewer abstractions, not more

We should avoid:

- invisible fallbacks
- duplicated state namespaces
- implicit path remapping
- hidden recovery behavior
- “smart” abstractions that erase cause and effect

## Status

This repository is intentionally at zero.

That is a feature, not a problem.

We are using the clean slate to keep the best ideas from earlier systems while refusing the assumptions that made them painful to operate.

## Working Motto

Build the agent runtime you can actually debug at 2am.
