# Provider Engine Design

This document defines the shape of BetterClaw's model-provider engine.

The key conclusion is:

**The core abstraction should not be SSE.**

## Current Implementation

BetterClaw currently ships two OpenAI-compatible engine families:

- `openai_chatcompletions`
  For `/chat/completions` style providers such as LM Studio and OpenRouter chat mode.
- `openai_responses`
  For `/responses` style providers such as Codex and OpenRouter Responses mode.

Provider presets now infer the engine family:

- `BETTERCLAW_PROVIDER=local`
  Local OpenAI-compatible chat-completions, preserving the existing LM Studio path.
- `BETTERCLAW_PROVIDER=openrouter`
  OpenRouter with `BETTERCLAW_PROVIDER_MODE=chat|responses`.
- `BETTERCLAW_PROVIDER=codex`
  Codex auth/header quirks on top of the shared Responses engine.
- `BETTERCLAW_PROVIDER=copilot`
  GitHub Copilot on top of the shared Responses engine, with `Copilot-Integration-Id` headers and token resolution from direct env vars or a helper command.

Copilot auth in the rewrite is intentionally explicit:

- preferred: `GITHUB_COPILOT_API_TOKEN`
- compatibility fallback: `BETTERCLAW_COPILOT_TOKEN` or legacy `COPILOT_TOKEN`
- refresh hook: `BETTERCLAW_COPILOT_TOKEN_COMMAND`

The installed `copilot` CLI does not currently expose a stable public command for printing the bearer token BetterClaw would need for direct HTTP calls, so BetterClaw uses a generic helper-command hook instead of scraping private CLI internals.

Both OpenAI-compatible engines now also share one runtime-level rate-limit gate:

- HTTP `429` and provider error bodies such as `rate_limit_exceeded` are treated as rate limits
- `Retry-After` is honored when present
- otherwise BetterClaw uses exponential backoff starting at 1 second
- one rate-limit hit blocks all later requests for that runtime until the backoff window expires
- each blocked or retried attempt still produces trace data, so replay/debugging remains intact

SSE is one transport. The real architectural problem is how to normalize many different incremental response shapes into one internal event model.

## Why SSE Is Not Enough

We need to support multiple provider families with different wire protocols and streaming behavior:

- OpenAI-compatible `/v1/chat/completions`
- OpenAI `/v1/responses`
- LM Studio's OpenAI-compatible APIs
- GitHub Copilot session/event streams
- Qwen-Agent style streaming and function-call accumulation

These systems do not share one response format.

What they *do* share is this:

- responses may arrive incrementally
- text may arrive in chunks
- reasoning may arrive in chunks
- tool calls may arrive partially and must be assembled
- usage may arrive late
- completion/error may be signaled separately from content

So the core engine should be built around **incremental event normalization**, not around one transport protocol.

## Grounding From Existing Systems

### Old BetterClaw Codex provider

The old Codex path on `main` used SSE from the Responses API and accumulated:

- `response.output_text.delta`
- `response.output_item.added`
- `response.function_call_arguments.delta`
- `response.completed`

That implementation already proved an important point: a usable provider layer must accumulate fragments into a final tool call and final assistant message.

Reference:
- [`/Users/chad/Repos/betterclaw/src/llm/openai_codex.rs`](\/Users/chad/Repos/betterclaw/src/llm/openai_codex.rs)

### LM Studio

LM Studio supports OpenAI-compatible chat completions and tool use, but:

- tool calls may come back as standard `tool_calls` in non-streaming mode
- in streaming mode, tool call ids, names, and arguments arrive as deltas
- if the model emits malformed tool output, LM Studio may fall back to plain `content`

So even though the endpoint looks OpenAI-compatible, the runtime still needs strong accumulation and fallback-aware parsing.

### Qwen-Agent

Qwen-Agent strongly suggests that full-stream accumulation is the correct abstraction.

Notably:

- it discourages `delta_stream=True`
- it accumulates full content, reasoning content, and tool calls over the whole stream
- it merges tool-call fragments by id and concatenates names/arguments until complete

### Copilot SDK

Copilot does not expose a simple HTTP completion stream. It exposes a session event stream over its own protocol, with events such as:

- assistant message delta
- reasoning delta
- final assistant message
- idle
- error

That means Copilot fits naturally into an event-normalizing engine, but poorly into an “SSE-first” engine.

## Streaming Accumulation Modes

Different providers and SDKs expose streaming output with meaningfully different accumulation contracts.

This matters a lot because the runtime must know whether to:

- append the new chunk
- replace the current full text with the latest snapshot
- wait for a final canonical payload

And it also matters for reasoning-tag handling such as `<think>...</think>`.

### 1. Delta-only streaming

Each event contains only the new fragment since the previous event.

Examples:

- Copilot SDK `assistant.message_delta` and `assistant.reasoning_delta`
- Qwen-Agent `delta_stream=True` mode

Runtime rule:

- accumulate by appending deltas

Reasoning-tag implication:

- `<think>` parsing must be stateful across chunk boundaries
- per-chunk stripping is unsafe because tags can be split across frames

### 2. Full-so-far snapshot streaming

Each yielded item contains the full accumulated response up to that point, not just the new fragment.

Examples:

- Qwen-Agent `stream=True, delta_stream=False`
- Qwen DashScope `_full_stream_output`

Runtime rule:

- treat each update as a replacement snapshot, not an append

Reasoning-tag implication:

- easiest streaming mode for `<think>` handling
- the runtime can re-parse the full snapshot each update and derive clean text + reasoning repeatedly

### 3. Delta stream plus final canonical full event

The stream emits incremental deltas while generating, then later emits a final event containing the complete finished content.

Examples:

- Copilot SDK:
  - `assistant.message_delta`
  - `assistant.reasoning_delta`
  - final `assistant.message`
  - final `assistant.reasoning`

Runtime rule:

- append deltas during streaming
- reconcile against the final canonical message when it arrives

Reasoning-tag implication:

- live reasoning stripping may be approximate
- final reconciliation can produce the exact clean text and exact reasoning content

### 4. Inline reasoning inside normal content

Some providers do not expose reasoning in a separate field and instead return:

- `<think>reasoning</think>final answer`

Examples:

- Qwen-Agent documents this as `thought_in_content=True`
- OpenClaw has dedicated utilities to strip and split thinking-tagged text

Runtime rule:

- never assume `content` is safe to deliver as-is
- parse and split reasoning-tagged content before storing visible assistant text

Reasoning-tag implication:

- this is the primary case that requires `<think>` parsing and stripping

### 5. Structured reasoning separate from visible content

Some providers expose reasoning in a first-class field separate from visible assistant text.

Examples:

- Qwen-Agent OpenAI-compatible handling of `reasoning_content`
- Copilot SDK reasoning-specific event stream
- OpenClaw reasoning visibility model

Runtime rule:

- reasoning and visible content should be modeled separately from the start

Reasoning-tag implication:

- `<think>` stripping should still exist as a fallback safety net, but not as the primary mechanism

## Design Consequences

The provider engine should treat accumulation mode as explicit provider metadata, not as an accidental detail.

At minimum, BetterClaw should classify stream decoders by:

- `Delta`
- `FullSnapshot`
- `DeltaPlusFinal`

And separately classify reasoning shape by:

- `Structured`
- `InlineTagged`
- `Unknown`

That gives the runtime enough information to:

- assemble the stream correctly
- avoid duplicate text from snapshot streams
- reconcile with canonical final payloads when available
- handle `<think>` parsing only where it is actually needed

This also explains why a single generic “append all chunks” rule is wrong.

- It breaks full-snapshot streams by duplicating content.
- It breaks inline reasoning streams by leaking `<think>` blocks.
- It underuses structured reasoning providers by collapsing reasoning into content.

## Design Decision

BetterClaw should use:

- one **model engine**
- one internal **normalized event stream**
- multiple **provider dialect adapters**

The provider engine should treat:

- SSE
- plain JSON responses
- SDK callback/event streams
- iterator-based streaming APIs

as transport/input variants that get decoded into one internal event vocabulary.

## Core Architecture

### 1. Transport Layer

This layer is responsible for obtaining raw provider output.

Examples:

- HTTP JSON request/response
- HTTP SSE stream
- JSON-RPC session event stream
- in-process iterator or callback stream

This layer should not try to produce final assistant text or final tool calls.

Its job is only to yield raw frames/events in provider-native form.

### 2. Dialect Decoder Layer

This layer converts raw provider-native frames into BetterClaw normalized events.

Each provider family gets its own decoder.

Examples:

- `OpenAiChatCompletionsDecoder`
- `OpenAiResponsesDecoder`
- `CopilotSessionDecoder`
- `QwenAgentDecoder`

This is where provider-specific field names, partial tool-call shapes, and completion semantics live.

### 3. Normalized Event Layer

All decoders should emit the same internal event types.

Suggested initial event set:

- `ExchangeStarted`
- `TextDelta`
- `ReasoningDelta`
- `ToolCallStarted`
- `ToolCallNameDelta`
- `ToolCallArgumentsDelta`
- `ToolCallFinished`
- `UsageUpdated`
- `Completed`
- `Failed`

These events are intentionally low-level. They preserve incremental behavior without baking provider-specific field names into the runtime.

### 4. Exchange Accumulator

One accumulator should consume normalized events and build the final reduced result:

- final assistant text
- final reasoning text
- fully assembled tool calls
- usage
- finish status
- parse/assembly errors

This should be the canonical reduction path for all providers.

The accumulator should also support partial UI updates later, since it can expose the progressively built state at any point.

### 5. Trace Capture

For every exchange, BetterClaw should persist:

- exact request payload
- raw transport frames/events
- normalized events
- final reduced result

This gives us two debugging levels:

- raw truth from the provider
- normalized truth from our runtime

That separation is important because it lets us debug whether a bug came from:

- the provider
- the dialect decoder
- the accumulator
- the turn executor

## Proposed Internal Types

These are conceptual names, not final Rust API commitments.

### Request types

- `ModelExchangeRequest`
- `ProviderRequestEnvelope`

### Raw transport types

- `RawProviderFrame`
- `RawProviderExchange`

### Normalized event types

- `ModelEvent`
- `ModelEventKind`

### Reduced result types

- `ReducedModelExchange`
- `ReducedToolCall`
- `ReducedUsage`

### Decoder interfaces

- `Transport`
- `DialectDecoder`
- `ExchangeAccumulator`

## Adapter Strategy

The first implementation should not try to support every provider at once.

Recommended order:

1. `OpenAiChatCompletionsDecoder`
   - covers LM Studio immediately
   - gives us OpenAI-compatible local and hosted providers

2. `OpenAiResponsesDecoder`
   - covers the old Codex-style path
   - exercises richer SSE semantics and tool-call assembly

3. `CopilotSessionDecoder`
   - covers event-stream semantics that are not HTTP-response-based

4. `QwenAgentDecoder`
   - covers Qwen-specific accumulated stream patterns and function-call shapes

This order gives us the most general value fastest while keeping the implementation incremental.

## Important Non-Goals

The provider engine should not:

- invent missing tool arguments
- silently convert malformed provider tool calls into `{}` 
- normalize away raw provider truth
- collapse transport and decoding into one opaque abstraction

We want a system that is easy to debug when one provider behaves badly.

## Practical Rule

The provider engine should be described this way:

**BetterClaw runs model exchanges through transport adapters, decodes them into normalized events, and reduces them into one canonical result.**

That is the shape that can cover OpenAI, LM Studio, Copilot, and Qwen-Agent without forcing them into one fake protocol.
