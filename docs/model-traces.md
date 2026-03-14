# Model Trace Format

This document defines the canonical trace format for model-provider exchanges.

The design goal is simple:

- capture every request in full
- capture every response in full
- make traces easy to filter and replay
- keep storage costs under control

## Principles

### Full Payloads Are Mandatory

Every model invocation should persist:

- the exact request body sent to the provider
- the exact response body returned by the provider

If a provider uses streaming, we should preserve:

- raw stream frames when practical
- the final reconstructed response

### Metadata Must Be Searchable

A human should be able to quickly find traces by:

- agent id
- thread id
- turn id
- provider
- model
- channel
- time range
- outcome

The metadata should be query-friendly even when the payload is stored as a compressed blob.

### One Trace Per Provider Exchange

A trace record represents one outbound request to a model provider and its corresponding result.

If a turn makes multiple model calls, that should produce multiple trace records.

## Trace Record

The canonical shape should look like this:

```json
{
  "trace_id": "uuid",
  "turn_id": "uuid",
  "thread_id": "uuid",
  "agent_id": "default",
  "channel": "tidepool",
  "provider": "openai",
  "model": "gpt-5.4",
  "request_started_at": "2026-03-14T18:51:40.000Z",
  "request_completed_at": "2026-03-14T18:51:42.000Z",
  "duration_ms": 2214,
  "outcome": "ok",
  "request": {
    "headers": {
      "content-type": "application/json"
    },
    "body": {}
  },
  "response": {
    "status": 200,
    "headers": {},
    "body": {}
  },
  "usage": {
    "input_tokens": 36885,
    "output_tokens": 164,
    "cache_read_input_tokens": 0,
    "cache_creation_input_tokens": 0
  },
  "provider_request_id": "req_123",
  "tool_count": 1,
  "tool_names": ["tidepool"],
  "blob_refs": {
    "request_body": null,
    "response_body": null,
    "stream_frames": null
  },
  "error": null
}
```

## Required Fields

### Identity

- `trace_id`
- `turn_id`
- `thread_id`
- `agent_id`

### Routing Context

- `channel`
- `provider`
- `model`

### Timing

- `request_started_at`
- `request_completed_at`
- `duration_ms`

### Payloads

- `request`
- `response`

### Outcome

- `outcome`: one of `ok`, `provider_error`, `transport_error`, `parse_error`, `cancelled`

## Request Object

The request object should preserve the exact provider-facing payload.

Recommended fields:

```json
{
  "headers": {},
  "body": {},
  "body_text": null,
  "stream": true
}
```

Notes:

- redact secrets before persistence
- preserve non-secret headers if useful for debugging
- keep the exact JSON body when possible
- if the provider uses a non-JSON payload, store it as `body_text`

## Response Object

The response object should preserve the exact provider-facing result.

Recommended fields:

```json
{
  "status": 200,
  "headers": {},
  "body": {},
  "body_text": null,
  "streamed": true
}
```

For streaming providers, we should keep both:

- a reconstructed final body
- optional raw frame capture

## Error Object

If the exchange fails, the trace should still be complete.

Recommended shape:

```json
{
  "kind": "parse_error",
  "message": "Tool call 'tidepool' had malformed JSON arguments: expected value at line 1 column 1",
  "details": {}
}
```

The trace record should exist even when:

- the HTTP request fails
- the provider returns a non-200 response
- the provider response is malformed
- the runtime fails to parse the provider payload

## Storage Strategy

We should split trace data into two layers.

### Hot Metadata

Stored in a searchable local store:

- ids
- timestamps
- provider/model
- outcome
- token counts
- tool names
- short error summary
- blob references

### Cold Payloads

Stored as compressed blobs:

- full request body
- full response body
- optional stream frames

This gives us fast filtering without forcing huge JSON payloads into every query path.

## Redaction Rules

We should redact:

- API keys
- bearer tokens
- cookies
- authorization headers
- other configured secrets

We should not redact:

- the structural shape of the request
- tool schemas
- model messages
- provider errors

The point is to preserve enough truth to debug behavior.

## Retention

Recommended defaults:

- keep searchable metadata longer than blobs
- compress blobs immediately
- rotate old payload blobs based on age and disk budget
- allow pinning specific traces for long-term debugging

Suggested baseline:

- metadata retention: 30-90 days
- blob retention: 7-30 days
- pinned traces: explicit manual retention

## Replay

A trace should be replayable.

At minimum, we should be able to:

- inspect the exact request
- inspect the exact response
- rebuild the parsed model result
- compare parsed output to raw payload

Eventually we should support:

- replaying a turn against a local parser
- diffing two traces side by side
- exporting a trace bundle for bug reports

## What Not To Do

Do not:

- log only summaries
- log reconstructed requests instead of the exact provider payload
- discard streaming frames without keeping the final reconstructed body
- mix giant payload blobs into normal operational line logs
- silently skip trace creation on failure paths

The model exchange is the core loop. We should capture it like we mean it.
