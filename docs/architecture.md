# Architecture

This document describes the initial shape of BetterClaw as a host-native, event-driven agent runtime.

## Runtime Boundary

The runtime owns five things:

- agents
- workspaces
- threads
- tools
- channels

It also owns an append-only event stream that records what happened during each turn.

## Turn Flow

1. A channel receives or polls an inbound event.
2. The runtime resolves the agent and thread.
3. Context is assembled from thread state and workspace state.
4. The LLM returns either text or explicit tool calls.
5. Tools execute with validated parameters.
6. Tool outputs are written back into the thread timeline.
7. The channel adapter emits the final outbound message.

## Important Constraints

- Agent identity must stay stable across channels.
- Relative paths must resolve consistently across all file tools.
- Channels may persist cursors, but they should not own agent memory.
- The runtime must never invent missing tool parameters.
- Every important state transition should be visible in structured logs.

## Logging Standard

The runtime should eventually emit one coherent timeline per turn:

- inbound event received
- thread resolved
- prompt assembled
- model invoked
- model responded
- tool requested
- tool validated
- tool executed
- outbound message emitted
- cursor advanced

Every log line should make it easy to answer:

- what happened
- to which agent/thread/channel
- with which identifiers
- and why it failed, if it failed

## Model Request Logging

The single most important debugging surface in the system is the model-provider exchange.

BetterClaw should log:

- the full request payload sent to the provider
- the full response payload returned by the provider
- timing, model, token usage, and request identifiers

This should be treated as first-class trace data, not best-effort debug text.

The goal is simple: if an agent behaves strangely, we should be able to inspect the exact request/response pair that caused it.

## Retention Strategy

Full payload logging does not mean careless logging.

We should design for:

- compressed payload storage
- configurable retention windows
- separation between searchable metadata and blob payloads
- easy replay/export for a single turn without tailing giant log files

The system should be generous with truth and conservative with disk.

## Trace Schema

The concrete model-provider trace format is defined in [`/Users/chad/Repos/betterclaw/docs/model-traces.md`](/Users/chad/Repos/betterclaw/docs/model-traces.md).

That document should be treated as the source of truth for:

- request/response capture
- metadata indexing
- error recording
- payload blob storage
- replay expectations
