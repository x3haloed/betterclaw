const state = {
  selectedThreadId: null,
  selectedTraceId: null,
  selectedThreadTraceDetails: [],
  selectedThreadTimeline: [],
  stream: null,
  refreshTimer: null,
  runtimeSettings: null,
  retentionSettings: null,
  inspectorView: "compressor",
};

async function request(path, options = {}) {
  const response = await fetch(path, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(body.error || response.statusText);
  }
  return response.json();
}

function pretty(value) {
  if (value == null) return "null";
  return JSON.stringify(value, null, 2);
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function formatTimestamp(value) {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

function renderMarkdown(value) {
  const text = value || "";
  const safe = text.replaceAll("<", "&lt;").replaceAll(">", "&gt;");
  if (window.marked?.parse) {
    return window.marked.parse(safe, { breaks: true, gfm: true, headerIds: false, mangle: false });
  }
  return `<pre>${escapeHtml(text)}</pre>`;
}

function traceRole(detail) {
  return detail?.request_body?.betterclaw_role || "agent";
}

function parseCompressorContent(detail) {
  const content = detail?.reduced_result?.content;
  if (typeof content !== "string") return null;
  try {
    return JSON.parse(content);
  } catch {
    return null;
  }
}

function summarizeStep(detail) {
  const reduced = detail.reduced_result || {};
  const parsed = traceRole(detail) === "compressor" ? parseCompressorContent(detail) : null;
  if (parsed?.summary) {
    return parsed.summary;
  }
  const toolCalls = reduced.tool_calls || [];
  if (toolCalls.length) {
    const toolNames = toolCalls.map((toolCall) => toolCall.name).join(", ");
    return `Tool call${toolCalls.length === 1 ? "" : "s"}: ${toolNames}`;
  }
  if (typeof reduced.content === "string" && reduced.content.trim()) {
    return reduced.content.replace(/\s+/g, " ").trim().slice(0, 140);
  }
  return "No reduced output";
}

function setInspectorView(view) {
  state.inspectorView = view;
  document.getElementById("compressor-view").hidden = view !== "compressor";
  document.getElementById("diagnostics-view").hidden = view !== "diagnostics";
  document.getElementById("view-compressor").classList.toggle("active", view === "compressor");
  document.getElementById("view-diagnostics").classList.toggle("active", view === "diagnostics");
}

function renderTraceChain(details) {
  const chain = document.getElementById("trace-chain");
  if (!details.length) {
    chain.innerHTML = "";
    return;
  }
  chain.innerHTML = details
    .map((detail, index) => {
      const isActive = detail.trace.id === state.selectedTraceId;
      const role = traceRole(detail);
      return `<button class="trace-chip${isActive ? " active" : ""}" data-trace-id="${escapeHtml(detail.trace.id)}">
        <span class="trace-chip-step">${escapeHtml(role)} • step ${index + 1}</span>
        <span class="trace-chip-outcome">${escapeHtml(detail.trace.outcome)}</span>
      </button>`;
    })
    .join('<span class="trace-chain-arrow">→</span>');
  for (const button of chain.querySelectorAll(".trace-chip")) {
    button.onclick = () => selectTrace(button.dataset.traceId);
  }
}

function renderTraceSummary(detail) {
  const trace = detail.trace;
  const reduced = detail.reduced_result || {};
  return [
    ["Role", traceRole(detail)],
    ["Model", trace.model],
    ["Outcome", trace.outcome],
    ["Duration", `${trace.duration_ms} ms`],
    ["Tools", trace.tool_names.length ? trace.tool_names.join(", ") : "none"],
    ["Finish", reduced.finish_reason || "n/a"],
    ["Frames", Array.isArray(detail.stream_body) ? detail.stream_body.length : 0],
    ["Started", formatTimestamp(trace.request_started_at)],
  ]
    .map(
      ([label, value]) =>
        `<div class="summary-row"><span class="summary-label">${escapeHtml(label)}</span><span class="summary-value">${escapeHtml(value)}</span></div>`,
    )
    .join("");
}

function renderTraceDetail(detail) {
  document.getElementById("trace-empty").hidden = true;
  const traceDetail = document.getElementById("trace-detail");
  traceDetail.hidden = false;
  document.getElementById("trace-summary").innerHTML = renderTraceSummary(detail);
  document.getElementById("trace-reduced").textContent = pretty(detail.reduced_result);
  document.getElementById("trace-request").textContent = pretty(detail.request_body);
  document.getElementById("trace-response").textContent = pretty(detail.response_body);
  document.getElementById("trace-frames").textContent = pretty(detail.stream_body);
}

function clearTraceDetail() {
  document.getElementById("trace-empty").hidden = false;
  document.getElementById("trace-detail").hidden = true;
  document.getElementById("trace-summary").innerHTML = "";
  document.getElementById("trace-reduced").textContent = "";
  document.getElementById("trace-request").textContent = "";
  document.getElementById("trace-response").textContent = "";
  document.getElementById("trace-frames").textContent = "";
}

function renderConversation(turns) {
  const conversation = document.getElementById("conversation");
  conversation.innerHTML = "";
  if (!turns.length) {
    conversation.innerHTML = '<div class="empty-state">This thread is empty. Start the conversation below.</div>';
    return;
  }

  for (const turn of turns) {
    conversation.appendChild(
      createMessageCard({
        role: "User",
        kind: "user",
        status: turn.status,
        timestamp: turn.created_at,
        body: escapeHtml(turn.user_message).replaceAll("\n", "<br />"),
      }),
    );

    if (turn.assistant_message) {
      conversation.appendChild(
        createMessageCard({
          role: "Assistant",
          kind: "assistant",
          status: turn.status,
          timestamp: turn.updated_at,
          body: renderMarkdown(turn.assistant_message),
        }),
      );
    } else if (turn.status === "running" || turn.status === "pending") {
      conversation.appendChild(
        createMessageCard({
          role: "Assistant",
          kind: "status",
          status: turn.status,
          timestamp: turn.updated_at,
          body: "<p>Working…</p>",
        }),
      );
    }

    if (turn.error) {
      conversation.appendChild(
        createMessageCard({
          role: "Runtime",
          kind: "status",
          status: turn.status,
          timestamp: turn.updated_at,
          body: `<p>${escapeHtml(turn.error)}</p>`,
        }),
      );
    }
  }
}

function createMessageCard({ role, kind, status, timestamp, body }) {
  const card = document.createElement("article");
  card.className = `message-card ${kind}`;
  card.innerHTML = `
    <div class="message-meta">
      <span class="message-role">${escapeHtml(role)}</span>
      <span class="message-status">${escapeHtml(status)} • ${escapeHtml(formatTimestamp(timestamp))}</span>
    </div>
    <div class="message-body">${body}</div>
  `;
  return card;
}

function renderDiagnosticsTimeline(timeline) {
  const timelineEl = document.getElementById("timeline");
  timelineEl.innerHTML = "";
  if (!timeline.length) {
    timelineEl.innerHTML = '<div class="empty-state">No diagnostics events for this thread yet.</div>';
    return;
  }
  for (const event of timeline) {
    const item = document.createElement("div");
    item.className = "timeline-item";
    item.innerHTML = `<div class="timeline-kind">${escapeHtml(event.kind)}</div><pre>${escapeHtml(pretty(event.payload))}</pre>`;
    timelineEl.appendChild(item);
  }
}

function renderCompressorHome(details) {
  const home = document.getElementById("compressor-home");
  home.innerHTML = "";
  const compressorDetails = details.filter((detail) => traceRole(detail) === "compressor").reverse();
  if (!compressorDetails.length) {
    home.innerHTML = '<div class="empty-state">No compressor activity for this thread yet.</div>';
    return;
  }

  for (const detail of compressorDetails) {
    const parsed = parseCompressorContent(detail);
    const trace = detail.trace;
    const card = document.createElement("article");
    card.className = "compressor-card";
    const wakePack = parsed?.wake_pack ? renderMarkdown(parsed.wake_pack) : `<pre>${escapeHtml(typeof detail.reduced_result?.content === "string" ? detail.reduced_result.content : pretty(detail.reduced_result))}</pre>`;
    card.innerHTML = `
      <div class="compressor-header">
        <div class="compressor-title">${escapeHtml(parsed?.summary || summarizeStep(detail))}</div>
        <div class="compressor-meta">${escapeHtml(trace.outcome)} • ${escapeHtml(formatTimestamp(trace.request_started_at))}</div>
      </div>
      <div class="compressor-body">${wakePack}</div>
      <div class="compressor-stats">
        ${renderCompressorStat("Invariants", (parsed?.invariant_self?.length || 0) + (parsed?.invariant_user?.length || 0) + (parsed?.invariant_relationship?.length || 0))}
        ${renderCompressorStat("Drift", (parsed?.drift_flags?.length || 0) + (parsed?.drift_contradictions?.length || 0) + (parsed?.drift_merges?.length || 0))}
        ${renderCompressorStat("Duration", `${trace.duration_ms} ms`)}
      </div>
    `;
    home.appendChild(card);
  }
}

function renderCompressorStat(label, value) {
  return `<div class="compressor-stat"><span class="compressor-stat-label">${escapeHtml(label)}</span><span class="compressor-stat-value">${escapeHtml(value)}</span></div>`;
}

async function loadThreads() {
  const threads = await request("/api/threads");
  const list = document.getElementById("thread-list");
  list.innerHTML = "";
  for (const thread of threads) {
    const button = document.createElement("button");
    button.className = "thread-item";
    if (thread.id === state.selectedThreadId) button.classList.add("active");
    button.textContent = thread.title;
    button.onclick = () => selectThread(thread.id);
    list.appendChild(button);
  }
  if (!state.selectedThreadId && threads[0]) {
    await selectThread(threads[0].id);
  }
}

function renderSettings(settings) {
  state.runtimeSettings = settings;
  document.getElementById("settings-model").value = settings.model;
  document.getElementById("settings-system-prompt").value = settings.system_prompt;
  document.getElementById("settings-temperature").value = settings.temperature;
  document.getElementById("settings-max-tokens").value = settings.max_tokens;
  document.getElementById("settings-max-history-turns").value = settings.max_history_turns;
  document.getElementById("settings-stream").checked = settings.stream;
  document.getElementById("settings-allow-tools").checked = settings.allow_tools;
}

async function loadSettings() {
  const settings = await request("/api/settings/runtime");
  renderSettings(settings);
  const retention = await request("/api/settings/retention");
  renderRetentionSettings(retention);
}

function renderRetentionSettings(settings) {
  state.retentionSettings = settings;
  document.getElementById("retention-trace-blob-days").value = settings.trace_blob_retention_days;
}

function scheduleThreadRefresh() {
  if (!state.selectedThreadId) return;
  if (state.refreshTimer) clearTimeout(state.refreshTimer);
  state.refreshTimer = setTimeout(() => {
    state.refreshTimer = null;
    selectThread(state.selectedThreadId, { preserveTrace: true }).catch((error) => {
      document.getElementById("conversation").innerHTML = `<div class="empty-state">${escapeHtml(error.message)}</div>`;
    });
  }, 120);
}

function connectThreadStream(threadId) {
  if (state.stream) {
    state.stream.close();
    state.stream = null;
  }
  document.getElementById("live-indicator").hidden = true;
  if (!threadId) return;
  const stream = new EventSource(`/api/threads/${threadId}/stream`);
  document.getElementById("live-indicator").hidden = false;
  stream.onmessage = () => scheduleThreadRefresh();
  stream.onerror = () => {
    document.getElementById("live-indicator").hidden = true;
    stream.close();
    if (state.selectedThreadId === threadId) {
      setTimeout(() => connectThreadStream(threadId), 1000);
    }
  };
  state.stream = stream;
}

async function selectThread(threadId, options = {}) {
  state.selectedThreadId = threadId;
  if (!options.preserveTrace) {
    state.selectedTraceId = null;
  }
  connectThreadStream(threadId);
  const [detail, timeline, traceDetails] = await Promise.all([
    request(`/api/threads/${threadId}`),
    request(`/api/threads/${threadId}/timeline`),
    request(`/api/threads/${threadId}/trace-details`),
  ]);

  document.getElementById("thread-title").textContent = detail.thread.title;
  document.getElementById("thread-subtitle").textContent = `${detail.turns.length} turn${detail.turns.length === 1 ? "" : "s"}`;
  renderConversation(detail.turns);

  state.selectedThreadTimeline = timeline;
  renderDiagnosticsTimeline(timeline);

  state.selectedThreadTraceDetails = traceDetails;
  renderTraceChain(traceDetails);
  renderTraceList(traceDetails);
  renderCompressorHome(traceDetails);

  if (traceDetails.length) {
    const preferredTraceId =
      options.preserveTrace && traceDetails.some((detail) => detail.trace.id === state.selectedTraceId)
        ? state.selectedTraceId
        : traceDetails[traceDetails.length - 1].trace.id;
    await selectTrace(preferredTraceId);
  } else {
    clearTraceDetail();
  }

  await loadThreads();
}

function renderTraceList(details) {
  const traceList = document.getElementById("trace-list");
  traceList.innerHTML = "";
  if (!details.length) {
    traceList.innerHTML = '<div class="empty-state">No traces recorded for this thread.</div>';
    return;
  }
  for (const [index, detail] of details.entries()) {
    const trace = detail.trace;
    const button = document.createElement("button");
    button.className = "trace-item";
    button.dataset.traceId = trace.id;
    button.innerHTML = `
      <span class="trace-step">${escapeHtml(traceRole(detail))} • step ${index + 1}</span>
      <span class="trace-primary">${escapeHtml(trace.model)}</span>
      <span class="trace-secondary">${escapeHtml(trace.outcome)} • ${escapeHtml(`${trace.duration_ms}ms`)} • ${escapeHtml(`${trace.tool_count} tools`)}</span>
      <span class="trace-preview">${escapeHtml(summarizeStep(detail))}</span>
    `;
    button.onclick = () => selectTrace(trace.id);
    traceList.appendChild(button);
  }
}

async function selectTrace(traceId) {
  state.selectedTraceId = traceId;
  const detail = state.selectedThreadTraceDetails.find((item) => item.trace.id === traceId) || (await request(`/api/traces/${traceId}`));
  renderTraceDetail(detail);
  for (const button of document.querySelectorAll(".trace-item")) {
    button.classList.toggle("active", button.dataset.traceId === traceId);
  }
  for (const chip of document.querySelectorAll(".trace-chip")) {
    chip.classList.toggle("active", chip.dataset.traceId === traceId);
  }
}

document.getElementById("view-compressor").onclick = () => setInspectorView("compressor");
document.getElementById("view-diagnostics").onclick = () => setInspectorView("diagnostics");

document.getElementById("new-thread").onclick = async () => {
  const thread = await request("/api/threads", {
    method: "POST",
    body: JSON.stringify({ title: "New Thread" }),
  });
  await selectThread(thread.id);
};

document.getElementById("composer").onsubmit = async (event) => {
  event.preventDefault();
  if (!state.selectedThreadId) return;
  const input = document.getElementById("message-input");
  const content = input.value.trim();
  if (!content) return;
  await request(`/api/threads/${state.selectedThreadId}/messages`, {
    method: "POST",
    body: JSON.stringify({ content }),
  });
  input.value = "";
  await selectThread(state.selectedThreadId, { preserveTrace: true });
};

document.getElementById("settings-form").onsubmit = async (event) => {
  event.preventDefault();
  const status = document.getElementById("settings-status");
  status.textContent = "Saving...";
  const payload = {
    model: document.getElementById("settings-model").value.trim(),
    system_prompt: document.getElementById("settings-system-prompt").value,
    temperature: Number(document.getElementById("settings-temperature").value),
    max_tokens: Number(document.getElementById("settings-max-tokens").value),
    max_history_turns: Number(document.getElementById("settings-max-history-turns").value),
    stream: document.getElementById("settings-stream").checked,
    allow_tools: document.getElementById("settings-allow-tools").checked,
  };
  const settings = await request("/api/settings/runtime", {
    method: "PUT",
    body: JSON.stringify(payload),
  });
  renderSettings(settings);
  status.textContent = "Saved";
  setTimeout(() => {
    if (status.textContent === "Saved") status.textContent = "";
  }, 1500);
};

document.getElementById("retention-form").onsubmit = async (event) => {
  event.preventDefault();
  const status = document.getElementById("retention-status");
  status.textContent = "Saving...";
  const retention = await request("/api/settings/retention", {
    method: "PUT",
    body: JSON.stringify({
      trace_blob_retention_days: Number(document.getElementById("retention-trace-blob-days").value),
    }),
  });
  renderRetentionSettings(retention);
  status.textContent = "Saved";
  setTimeout(() => {
    if (status.textContent === "Saved") status.textContent = "";
  }, 1500);
};

document.getElementById("prune-traces").onclick = async () => {
  const status = document.getElementById("retention-status");
  status.textContent = "Pruning...";
  const report = await request("/api/runtime/prune-traces", {
    method: "POST",
    body: JSON.stringify({}),
  });
  status.textContent = `Pruned ${report.pruned_blob_count} blobs, reclaimed ${report.reclaimed_bytes} bytes`;
  if (state.selectedThreadId) {
    await selectThread(state.selectedThreadId, { preserveTrace: true });
  }
};

setInspectorView("compressor");

Promise.all([loadSettings(), loadThreads()]).catch((error) => {
  document.getElementById("conversation").innerHTML = `<div class="empty-state">${escapeHtml(error.message)}</div>`;
});
