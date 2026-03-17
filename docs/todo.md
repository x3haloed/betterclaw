# TO-DO

Big missing areas left to implement:

- [x] collect/improve available tools based on what OpenClaw, copilot-sdk, and Qwen-Agent offer to their agents.
- [x] Add the compressor, wake_pack, durable ledger, and ledger_recall systems
- [x] add our tension/pattern/hypothesis routines back in (c9551d3b7d0798a4976a36d3419ded695355ee7c)
  - [x] Observation types (Tension, Pattern, Hypothesis, Contradiction)
  - [x] DB layer (upsert, list, resolve, summary, stale cleanup)
  - [x] Runtime routines (detect tensions, tool failure patterns, hypotheses, contradictions)
  - [x] Observations block injected into system prompt
  - [x] Wired into turn completion flow
- [x] Add tidepool support
- [x] Add Codex & Copilot providers in
- [x] image and video attachments (image_url for openai-compat, video_url if supported by provider), input via Discord
  - [x] MessageContent enum with Text | Parts (image_url) variants
  - [x] ContentPart enum with Text and ImageUrl
  - [x] OpenAI Responses API payload handles image_url
  - [x] Chat Completions API payload handles image_url
  - [x] Discord input attachment handling
- [x] max tool-loop iterations (MAX_TOOL_LOOP_ITERATIONS=32) to prevent runaway turns
- [x] Tidepool outbound noop filter — suppresses ACK/FYI/no-response-needed messages that create agent feedback loops
- [x] first-class skills support
  - [x] Skill discovery from workspace skills/ directory
  - [x] Skills block injected into system prompt
  - [x] read_skill tool for agents to read full instructions
  - [x] inject_skills setting + DB migration
- [x] tidepool_read_messages tool — read message history from subscribed domains
- [x] Fix Tidepool cursor seeding boundary skip — seed to baseline-1 to avoid losing messages at the boundary
- [x] tidepool_search_messages tool — content-based message search with domain/author/after_message_id filters
- [x] FIX: 2026-03-17T21:47:50.435540Z ERROR betterclaw::channels::discord: Discord inbound turn failed error=SQLite failure: `UNIQUE constraint failed: threads.id`
  - Root cause: concurrent `resolve_thread()` calls both `find_thread()` → None → both `INSERT` with same `id` (external_thread_id)
  - Fix: `INSERT OR IGNORE` in `create_thread()` then always `SELECT` the row back