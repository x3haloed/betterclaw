# AGENTS

This file is here to reduce drift.

BetterClaw currently has multiple parallel implementations of similar behavior:

- `openai_chatcompletions` and `openai_responses`
- streaming and non-streaming request/response paths
- provider-specific compatibility layers layered on top of shared runtime behavior

When one path is changed and the others are not reviewed, the app regresses in ways that are easy to miss during local debugging. Recent Codex fixes are a good example: a provider compatibility issue can show up first in one transport or one decode mode, while the real requirement applies more broadly.

## Core Rule

Treat these as linked surfaces, not isolated codepaths.

If you change one of the following:

- `src/model/openai_chatcompletions/*`
- `src/model/openai_responses/*`
- streaming behavior
- non-streaming behavior
- payload construction
- response decoding
- provider compatibility behavior

you must explicitly review the sibling implementations for the same issue.

## Required Drift Check

For any model transport change, check all of these:

1. `openai_chatcompletions` payload building
2. `openai_responses` payload building
3. chat completions streaming decode
4. chat completions non-streaming decode
5. responses streaming decode
6. responses non-streaming decode

Do not assume a bug is unique to the path where it was discovered.

## Symmetry Expectations

When behavior should be equivalent, keep it equivalent.

Examples:

- If a provider-specific field must be omitted for one transport, verify whether the sibling transport also sends it.
- If a provider requires streaming, verify both the emitted payload and the decode path that handles the reply.
- If optional tool parameters accept `null` in one path, verify the same contract everywhere tool schemas and validators interact.
- If a request uses an effective payload value, do not branch later on a stale pre-normalization field.

## Streaming vs Non-Streaming

Streaming and non-streaming are separate execution modes and both need deliberate coverage.

- A payload fix is not complete until the matching decode path is checked.
- A decode fix is not complete until the payload construction that triggers that mode is checked.
- If the app can force or override streaming, downstream decode logic must follow the effective payload, not the original request input.

## Tests

Prefer paired tests over one-off tests.

When fixing a transport or decode bug:

- add or update the test that reproduces the bug
- add the sibling transport test if the same invariant should hold there
- add the sibling streaming/non-streaming test if the same invariant should hold there

If you intentionally leave the sibling path unchanged, say why in the commit or handoff note.

## Review Checklist

Before finishing a change in this area, answer these:

1. Does the same issue exist in both `openai_chatcompletions` and `openai_responses`?
2. Does the same issue exist in both streaming and non-streaming paths?
3. Am I branching on the effective payload, or on an earlier field that may have been overridden later?
4. Did I add tests for the affected mode pair(s)?
5. If I did not mirror the change elsewhere, have I documented the reason?

## Bias

Bias toward explicit symmetry and explicit tests.

Do not rely on “this path probably works the same way.”
