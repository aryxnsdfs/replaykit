"use strict";

const state = { interactions: [], divergedSteps: new Map(), report: null, active: null };

async function boot() {
  const res = await fetch("/api/run");
  if (!res.ok) {
    document.getElementById("empty").innerHTML = "<p>Failed to load run.</p>";
    return;
  }
  const data = await res.json();
  state.interactions = data.interactions || [];
  state.report = data.report || null;
  indexDivergences();
  renderSummary(data.manifest);
  renderList();
}

function indexDivergences() {
  state.divergedSteps = new Map();
  if (state.report && Array.isArray(state.report.divergences)) {
    for (const d of state.report.divergences) {
      state.divergedSteps.set(d.step, d);
    }
  }
}

function renderSummary(m) {
  if (!m) return;
  const ratio = m.total_blob_bytes > 0 ? (m.total_logical_bytes / m.total_blob_bytes).toFixed(1) : "1.0";
  const el = document.getElementById("summary");
  el.innerHTML = `
    <span><b>${m.interaction_count}</b> steps</span>
    <span><b>${humanBytes(m.total_logical_bytes)}</b> logical</span>
    <span><b>${humanBytes(m.total_blob_bytes)}</b> on disk · <b>${ratio}×</b></span>
    <span>${(m.providers || []).join(", ")}</span>`;
  document.getElementById("count").textContent = m.interaction_count;
}

function renderList() {
  const list = document.getElementById("list");
  list.innerHTML = "";
  for (const i of state.interactions) {
    const li = document.createElement("li");
    li.dataset.step = i.step;
    const diverged = state.divergedSteps.has(i.step);
    if (diverged) li.classList.add("diverged");
    const dotClass = i.status >= 400 ? "dot err" : diverged ? "dot warn" : "dot";
    li.innerHTML = `
      <span class="step-no">${i.step}</span>
      <span class="${dotClass}"></span>
      <span class="li-main">
        <div class="li-endpoint">${escapeHtml(shortEndpoint(i))}</div>
        <div class="li-meta">${i.method} · ${i.status} · ${humanBytes(i.resp_bytes)}${i.stream ? " · stream" : ""}</div>
      </span>`;
    li.addEventListener("click", () => selectStep(i.step));
    list.appendChild(li);
  }
}

function shortEndpoint(i) {
  return (i.host || "") + (i.path || "");
}

async function selectStep(step) {
  state.active = step;
  for (const li of document.querySelectorAll("#list li")) {
    li.classList.toggle("active", Number(li.dataset.step) === step);
  }
  const res = await fetch(`/api/interaction/${step}`);
  if (!res.ok) return;
  const data = await res.json();
  renderDetail(data);
}

function renderDetail(d) {
  document.getElementById("empty").hidden = true;
  document.getElementById("view").hidden = false;

  const banner = document.getElementById("divergence-banner");
  const div = state.divergedSteps.get(d.step);
  if (div) {
    banner.hidden = false;
    banner.innerHTML = `<h4>⚠ Divergence at step ${div.step}</h4><div>${escapeHtml(div.summary || "")}</div>` +
      (div.diff ? `<pre>${colorDiff(div.diff)}</pre>` : "");
  } else {
    banner.hidden = true;
  }

  document.getElementById("req-method").textContent = d.request.method;
  document.getElementById("req-url").textContent = d.request.url;
  renderHeaders("req-headers", d.request.headers);
  document.getElementById("req-body").textContent = d.request.body || "";

  document.getElementById("resp-status").textContent = d.response.status;
  document.getElementById("resp-stream").hidden = !d.response.stream;
  renderHeaders("resp-headers", d.response.headers);
  document.getElementById("resp-body").textContent = d.response.body || "";
}

function renderHeaders(id, headers) {
  const t = document.getElementById(id);
  t.innerHTML = "";
  for (const h of headers || []) {
    const name = (h.name || "").toLowerCase();
    let value = h.value || "";
    if (["authorization", "x-api-key", "api-key", "cookie", "set-cookie"].includes(name)) {
      value = "<redacted>";
    }
    const tr = document.createElement("tr");
    tr.innerHTML = `<td class="k">${escapeHtml(h.name)}</td><td class="v">${escapeHtml(value)}</td>`;
    t.appendChild(tr);
  }
}

function colorDiff(diff) {
  return escapeHtml(diff)
    .split("\n")
    .map((l) => {
      if (l.startsWith("+")) return `<span class="add">${l}</span>`;
      if (l.startsWith("-")) return `<span class="del">${l}</span>`;
      return l;
    })
    .join("\n");
}

function humanBytes(n) {
  if (n == null) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let v = n, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

function escapeHtml(s) {
  return String(s == null ? "" : s)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

boot();
