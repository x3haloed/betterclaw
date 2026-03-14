# Provider Engine Design

This document defines the shape of BetterClaw's model-provider engine.

The key conclusion is:

**The core abstraction should not be SSE.**

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

References:
- [`/Users/chad/Repos/Qwen-Agent/qwen_agent/llm/base.py`](\/Users/chad/Repos/Qwen-Agent/qwen_agent/llm/base.py)
- [`/Users/chad/Repos/Qwen-Agent/qwen_agent/llm/qwen_dashscope.py`](\/Users/chad/Repos/Qwen-Agent/qwen_agent/llm/qwen_dashscope.py)

### Copilot SDK

Copilot does not expose a simple HTTP completion stream. It exposes a session event stream over its own protocol, with events such as:

- assistant message delta
- reasoning delta
- final assistant message
- idle
- error

That means Copilot fits naturally into an event-normalizing engine, but poorly into an “SSE-first” engine.

Reference:
- [`/Users/chad/Repos/copilot-sdk/nodejs/src/session.ts`](\/Users/chad/Repos/copilot-sdk/nodejs/src/session.ts)

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
