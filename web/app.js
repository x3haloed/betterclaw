const state = {
  selectedThreadId: null,
  selectedTraceId: null,
  selectedTurnTraceDetails: [],
  stream: null,
  refreshTimer: null,
  runtimeSettings: null,
  retentionSettings: null,
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

function summarizeStep(detail) {
  const reduced = detail.reduced_result || {};
  const toolCalls = reduced.tool_calls || [];
  if (toolCalls.length) {
    const toolNames = toolCalls.map((toolCall) => toolCall.name).join(", ");
    return `Tool call${toolCalls.length === 1 ? "" : "s"}: ${toolNames}`;
  }
  if (reduced.content) {
    return reduced.content.replace(/\s+/g, " ").trim().slice(0, 140);
  }
  return "No reduced output";
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
      return `<button class="trace-chip${isActive ? " active" : ""}" data-trace-id="${detail.trace.id}">
        <span class="trace-chip-step">Step ${index + 1}</span>
        <span class="trace-chip-outcome">${detail.trace.outcome}</span>
      </button>`;
    })
    .join("<span class=\"trace-chain-arrow\">→</span>");
  for (const button of chain.querySelectorAll(".trace-chip")) {
    button.onclick = () => selectTrace(button.dataset.traceId);
  }
}

function renderTraceSummary(detail) {
  const trace = detail.trace;
  const reduced = detail.reduced_result || {};
  return [
    ["Model", trace.model],
    ["Outcome", trace.outcome],
    ["Duration", `${trace.duration_ms} ms`],
    ["Tools", trace.tool_names.length ? trace.tool_names.join(", ") : "none"],
    ["Finish", reduced.finish_reason || "n/a"],
    ["Frames", Array.isArray(detail.stream_body) ? detail.stream_body.length : 0],
  ]
    .map(
      ([label, value]) =>
        `<div class="summary-row"><span class="summary-label">${label}</span><span class="summary-value">${value}</span></div>`,
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
  document.getElementById("retention-trace-blob-days").value =
    settings.trace_blob_retention_days;
}

function scheduleThreadRefresh() {
  if (!state.selectedThreadId) return;
  if (state.refreshTimer) {
    clearTimeout(state.refreshTimer);
  }
  state.refreshTimer = setTimeout(() => {
    state.refreshTimer = null;
    selectThread(state.selectedThreadId, { preserveTrace: true }).catch((error) => {
      document.getElementById("timeline").textContent = error.message;
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
  stream.onmessage = () => {
    scheduleThreadRefresh();
  };
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
  const detail = await request(`/api/threads/${threadId}`);
  document.getElementById("thread-title").textContent = detail.thread.title;

  const timeline = await request(`/api/threads/${threadId}/timeline`);
  const timelineEl = document.getElementById("timeline");
  timelineEl.innerHTML = "";
  for (const event of timeline) {
    const item = document.createElement("div");
    item.className = "timeline-item";
    item.innerHTML = `<div class="timeline-kind">${event.kind}</div><pre>${JSON.stringify(event.payload, null, 2)}</pre>`;
    timelineEl.appendChild(item);
  }

  const lastTurn = detail.turns[detail.turns.length - 1];
  await loadTraces(lastTurn?.id || null, options);
  await loadThreads();
}

async function selectTrace(traceId) {
  state.selectedTraceId = traceId;
  const detail =
    state.selectedTurnTraceDetails.find((item) => item.trace.id === traceId) ||
    (await request(`/api/traces/${traceId}`));
  renderTraceDetail(detail);
  const buttons = document.querySelectorAll(".trace-item");
  for (const button of buttons) {
    button.classList.toggle("active", button.dataset.traceId === traceId);
  }
  for (const chip of document.querySelectorAll(".trace-chip")) {
    chip.classList.toggle("active", chip.dataset.traceId === traceId);
  }
}

async function loadTraces(turnId, options = {}) {
  const traceList = document.getElementById("trace-list");
  document.getElementById("trace-chain").innerHTML = "";
  traceList.innerHTML = "";
  clearTraceDetail();
  state.selectedTurnTraceDetails = [];
  if (!turnId) return;
  const traces = await request(`/api/turns/${turnId}/traces`);
  const details = await Promise.all(traces.map((trace) => request(`/api/traces/${trace.id}`)));
  state.selectedTurnTraceDetails = details;
  renderTraceChain(details);
  for (const [index, detail] of details.entries()) {
    const trace = detail.trace;
    const button = document.createElement("button");
    button.className = "trace-item";
    button.dataset.traceId = trace.id;
    button.innerHTML = `
      <span class="trace-step">Step ${index + 1}</span>
      <span class="trace-primary">${trace.model}</span>
      <span class="trace-secondary">${trace.outcome} • ${trace.duration_ms}ms • ${trace.tool_count} tools</span>
      <span class="trace-preview">${summarizeStep(detail)}</span>
    `;
    button.onclick = () => selectTrace(trace.id);
    traceList.appendChild(button);
  }
  if (details.length) {
    const preferredTraceId =
      options.preserveTrace &&
      details.some((detail) => detail.trace.id === state.selectedTraceId)
        ? state.selectedTraceId
        : details[details.length - 1].trace.id;
    await selectTrace(preferredTraceId);
  }
}

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
  await selectThread(state.selectedThreadId);
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
      trace_blob_retention_days: Number(
        document.getElementById("retention-trace-blob-days").value,
      ),
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

Promise.all([loadSettings(), loadThreads()]).catch((error) => {
  document.getElementById("timeline").textContent = error.message;
});
