const state = {
  selectedThreadId: null,
  selectedTraceId: null,
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

async function selectThread(threadId) {
  state.selectedThreadId = threadId;
  state.selectedTraceId = null;
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
  await loadTraces(lastTurn?.id || null);
  await loadThreads();
}

async function selectTrace(traceId) {
  state.selectedTraceId = traceId;
  const detail = await request(`/api/traces/${traceId}`);
  renderTraceDetail(detail);
  const buttons = document.querySelectorAll(".trace-item");
  for (const button of buttons) {
    button.classList.toggle("active", button.dataset.traceId === traceId);
  }
}

async function loadTraces(turnId) {
  const traceList = document.getElementById("trace-list");
  traceList.innerHTML = "";
  clearTraceDetail();
  if (!turnId) return;
  const traces = await request(`/api/turns/${turnId}/traces`);
  for (const trace of traces) {
    const button = document.createElement("button");
    button.className = "trace-item";
    button.dataset.traceId = trace.id;
    button.innerHTML = `
      <span class="trace-primary">${trace.model}</span>
      <span class="trace-secondary">${trace.outcome} • ${trace.duration_ms}ms</span>
    `;
    button.onclick = () => selectTrace(trace.id);
    traceList.appendChild(button);
  }
  if (traces[0]) {
    await selectTrace(traces[0].id);
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

loadThreads().catch((error) => {
  document.getElementById("timeline").textContent = error.message;
});
