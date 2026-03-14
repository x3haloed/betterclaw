const state = {
  selectedThreadId: null,
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

async function loadTraces(turnId) {
  const traceList = document.getElementById("trace-list");
  const traceDetail = document.getElementById("trace-detail");
  traceList.innerHTML = "";
  traceDetail.textContent = "";
  if (!turnId) return;
  const traces = await request(`/api/turns/${turnId}/traces`);
  for (const trace of traces) {
    const button = document.createElement("button");
    button.className = "trace-item";
    button.textContent = `${trace.model} • ${trace.outcome}`;
    button.onclick = async () => {
      const detail = await request(`/api/traces/${trace.id}`);
      traceDetail.textContent = JSON.stringify(detail, null, 2);
    };
    traceList.appendChild(button);
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
