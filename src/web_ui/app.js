"use strict";

// Resolve API URLs relative to the document base so the UI works whether
// served at the root (http://host:8080/) or behind a reverse proxy.
const API_BASE = (() => {
  const base = document.baseURI || (location.origin + location.pathname);
  return base.replace(/\/$/, "");
})();
const api = (path) => `${API_BASE}${path.startsWith("/") ? path : "/" + path}`;

const REFRESH_MS = 1000;

const els = {
  connState: document.getElementById("conn-state"),
  lastSeen: document.getElementById("last-seen"),
  gridW: document.getElementById("grid-w"),
  gridDirection: document.getElementById("grid-direction"),
  gridAge: document.getElementById("grid-age"),
  circuitsStatusBody: document.getElementById("circuits-status-body"),
  batteriesStatusBody: document.getElementById("batteries-status-body"),
  cfgPath: document.getElementById("cfg-path"),
  toast: document.getElementById("toast"),
  // editors
  circuitsBody: document.getElementById("circuits-body"),
  batteriesList: document.getElementById("batteries-list"),
  btnAddCircuit: document.getElementById("btn-add-circuit"),
  btnSaveCircuits: document.getElementById("btn-save-circuits"),
  statusCircuits: document.getElementById("status-circuits"),
  btnAddBattery: document.getElementById("btn-add-battery"),
  btnSaveBatteries: document.getElementById("btn-save-batteries"),
  statusBatteries: document.getElementById("status-batteries"),
};

let consecutiveErrors = 0;
let cachedConfig = null;
let circuitIdsCache = [];

// ----------------------------------------------------------- helpers

function fmtW(v) {
  if (v == null || isNaN(v)) return "–";
  const sign = v >= 0 ? "+" : "";
  return `${sign}${v.toFixed(0)}`;
}
function fmtMs(ms) {
  if (ms == null || isNaN(ms)) return "–";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)} min`;
  return `${(ms / 3_600_000).toFixed(1)} h`;
}
function fmtPct(v) {
  if (v == null || isNaN(v)) return "–";
  return `${v.toFixed(1)} %`;
}
function escapeHtml(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}
function escapeAttr(s) {
  return escapeHtml(s).replace(/"/g, "&quot;");
}
function setConnOk(ok) {
  els.connState.classList.toggle("ok", ok);
  els.connState.classList.toggle("bad", !ok);
}
function directionLabel(w) {
  if (w == null) return "–";
  if (Math.abs(w) < 5) return "balanced";
  return w > 0 ? `importing ${w.toFixed(0)} W` : `exporting ${(-w).toFixed(0)} W`;
}
function showToast(msg, kind = "ok") {
  els.toast.textContent = msg;
  els.toast.className = `toast ${kind} show`;
  setTimeout(() => els.toast.classList.remove("show"), 3500);
}

// ----------------------------------------------------------- nav

document.querySelectorAll("nav button").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll("nav button").forEach((b) => b.classList.remove("active"));
    btn.classList.add("active");
    document.querySelectorAll(".view").forEach((v) => v.classList.remove("active"));
    document.getElementById(`view-${btn.dataset.view}`).classList.add("active");
  });
});

// ----------------------------------------------------------- status

function renderCircuitsStatus(circuits) {
  if (!circuits || circuits.length === 0) {
    els.circuitsStatusBody.innerHTML =
      '<tr><td colspan="7" class="empty">no circuits configured</td></tr>';
    return;
  }
  els.circuitsStatusBody.innerHTML = circuits
    .slice()
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((c) => {
      const cap = c.cap_w * 0.95;
      const measuredAbs = Math.abs(c.measured_sum_w);
      const headroom = cap - measuredAbs;
      const hClass = headroom < 0 ? "cap-over" : headroom < cap * 0.1 ? "cap-tight" : "cap-ok";
      const stateCell =
        c.silent_for_ms != null
          ? `<span class="state silent">SILENT (${fmtMs(c.silent_for_ms)})</span>`
          : '<span class="state ok">active</span>';
      const members = (c.member_ids || []).join(", ") || '<span class="dim">none</span>';
      return `<tr>
        <td><strong>${escapeHtml(c.id)}</strong></td>
        <td>${c.cap_w.toFixed(0)} W <span class="dim">(${c.fuse_amps} A × ${c.phases} ph)</span></td>
        <td class="num">${fmtW(c.commanded_sum_w)} W</td>
        <td class="num">${fmtW(c.measured_sum_w)} W</td>
        <td class="num ${hClass}">${headroom.toFixed(0)} W</td>
        <td>${stateCell}</td>
        <td class="dim">${members}</td>
      </tr>`;
    })
    .join("");
}

function renderBatteriesStatus(batteries) {
  if (!batteries || batteries.length === 0) {
    els.batteriesStatusBody.innerHTML =
      '<tr><td colspan="10" class="empty">no batteries configured</td></tr>';
    return;
  }
  els.batteriesStatusBody.innerHTML = batteries
    .slice()
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((b) => {
      let stateCell;
      if (b.last_error)
        stateCell = `<span class="state err" title="${escapeAttr(b.last_error)}">error</span>`;
      else if (b.plug_age_ms == null || b.plug_age_ms > 2000)
        stateCell = '<span class="state warn">plug stale</span>';
      else if (b.pulse_remaining > 0)
        stateCell = '<span class="state pulsing">pulsing</span>';
      else stateCell = '<span class="state ok">idle</span>';

      const dir =
        b.commanded_w > 5
          ? '<span class="dir-tag dir-discharge">discharge</span>'
          : b.commanded_w < -5
          ? '<span class="dir-tag dir-charge">charge</span>'
          : "";
      const queue =
        b.pulse_remaining > 0
          ? `<span class="pulses" title="${escapeAttr(fmtW(b.pending_pulse_w))} W × ${b.pulse_remaining} polls">${b.pulse_remaining}×${fmtW(b.pending_pulse_w)}W</span>`
          : '<span class="dim">–</span>';
      const socCell = `<span title="${escapeAttr(b.soc_source || "no source yet")}">${fmtPct(b.soc_pct)}</span>`;
      return `<tr>
        <td><strong>${escapeHtml(b.id)}</strong> ${dir}</td>
        <td>${escapeHtml(b.circuit)}</td>
        <td class="dim">${escapeHtml(b.address)}</td>
        <td class="num">${fmtW(b.commanded_w)} W</td>
        <td class="num">${fmtW(b.plug_w)} W</td>
        <td class="num">${queue}</td>
        <td class="num">${socCell}</td>
        <td class="num">${fmtMs(b.plug_age_ms)}</td>
        <td class="num">${fmtMs(b.last_marstek_poll_ms_ago)}</td>
        <td>${stateCell}</td>
      </tr>`;
    })
    .join("");
}

async function fetchStatus() {
  try {
    const res = await fetch(api("/api/status"), { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    els.gridW.textContent = data.grid_w == null ? "–" : data.grid_w.toFixed(0);
    els.gridDirection.textContent = directionLabel(data.grid_w);
    els.gridAge.textContent = fmtMs(data.grid_age_ms);
    if (els.cfgPath) els.cfgPath.textContent = data.config_path || "–";
    renderCircuitsStatus(data.circuits);
    renderBatteriesStatus(data.batteries);
    setConnOk(true);
    els.lastSeen.textContent = `updated ${new Date().toLocaleTimeString()}`;
    consecutiveErrors = 0;
  } catch (err) {
    consecutiveErrors++;
    setConnOk(false);
    els.lastSeen.textContent = `connection error (${consecutiveErrors}): ${err.message}`;
  }
}

// ----------------------------------------------------------- config: load & forms

function fillForm(form, prefix, obj) {
  if (!form || !obj) return;
  for (const [k, v] of Object.entries(obj)) {
    const input = form.elements.namedItem(k);
    if (!input) continue;
    if (input.type === "checkbox") input.checked = !!v;
    else input.value = v == null ? "" : String(v);
  }
}

function readForm(form) {
  const fd = new FormData(form);
  const out = {};
  for (const el of form.elements) {
    if (!el.name) continue;
    const name = el.name;
    if (el.type === "checkbox") out[name] = el.checked;
    else if (el.type === "number") {
      const v = fd.get(name);
      out[name] = v === "" || v == null ? null : Number(v);
    } else {
      const v = fd.get(name);
      out[name] = v == null ? "" : String(v);
    }
  }
  return out;
}

async function loadConfig() {
  try {
    const res = await fetch(api("/api/config"), { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    cachedConfig = data.config;
    if (els.cfgPath) els.cfgPath.textContent = data.config_path || "–";
    fillForm(document.getElementById("form-real_shelly"), "real_shelly", cachedConfig.real_shelly);
    fillForm(document.getElementById("form-virtual_shelly"), "virtual_shelly", cachedConfig.virtual_shelly);
    fillForm(document.getElementById("form-management"), "management", cachedConfig.management);
    fillForm(document.getElementById("form-dispatcher"), "dispatcher", cachedConfig.dispatcher);
    fillForm(document.getElementById("form-home_assistant"), "home_assistant", cachedConfig.home_assistant);
    renderCircuitsEditor(cachedConfig.circuits || []);
    renderBatteriesEditor(cachedConfig.batteries || []);
  } catch (err) {
    showToast(`Could not load config: ${err.message}`, "err");
  }
}

async function saveSection(name, body) {
  const res = await fetch(api(`/api/config/section/${name}`), {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const err = await res.json();
      if (err && err.error) msg = err.error;
    } catch (_) {}
    throw new Error(msg);
  }
}

document.querySelectorAll("form.cfg-form").forEach((form) => {
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const section = form.dataset.section;
    const status = form.querySelector(".form-status");
    status.textContent = "saving…";
    status.className = "form-status pending";
    try {
      const body = readForm(form);
      await saveSection(section, body);
      status.textContent = "saved ✓";
      status.className = "form-status ok";
      showToast(`${section} saved`);
      // Reload to pick up server-side normalisation.
      loadConfig();
    } catch (err) {
      status.textContent = `error: ${err.message}`;
      status.className = "form-status err";
      showToast(`${section}: ${err.message}`, "err");
    }
  });
});

// ----------------------------------------------------------- circuits editor

function recomputeCircuitCap(row) {
  const f = parseFloat(row.querySelector('[data-f="fuse_amps"]').value) || 0;
  const ph = parseInt(row.querySelector('[data-f="phases"]').value, 10) || 1;
  const v = parseFloat(row.querySelector('[data-f="voltage"]').value) || 230;
  const cap = f * v * ph;
  row.querySelector(".cap-out").textContent = isNaN(cap) ? "–" : `${cap.toFixed(0)} W`;
}

function addCircuitRow(c = {}) {
  const tr = document.createElement("tr");
  tr.innerHTML = `
    <td><input data-f="id" type="text" value="${escapeAttr(c.id || "")}" required></td>
    <td><input data-f="fuse_amps" type="number" min="0.1" step="any" value="${c.fuse_amps ?? 16}" required></td>
    <td>
      <select data-f="phases">
        <option value="1"${(c.phases ?? 1) === 1 ? " selected" : ""}>1</option>
        <option value="3"${c.phases === 3 ? " selected" : ""}>3</option>
      </select>
    </td>
    <td><input data-f="voltage" type="number" min="1" step="any" value="${c.voltage ?? 230}" required></td>
    <td class="cap-out">–</td>
    <td><button type="button" class="ghost" data-act="remove">remove</button></td>
  `;
  els.circuitsBody.appendChild(tr);
  recomputeCircuitCap(tr);
  tr.querySelectorAll("input,select").forEach((el) =>
    el.addEventListener("input", () => recomputeCircuitCap(tr))
  );
  tr.querySelector('[data-act="remove"]').addEventListener("click", () => {
    tr.remove();
    refreshCircuitIds();
    refreshBatteryCircuitDropdowns();
  });
  refreshCircuitIds();
  refreshBatteryCircuitDropdowns();
}

function renderCircuitsEditor(circuits) {
  els.circuitsBody.innerHTML = "";
  for (const c of circuits) addCircuitRow(c);
}

function readCircuits() {
  const rows = [...els.circuitsBody.querySelectorAll("tr")];
  return rows.map((tr) => ({
    id: tr.querySelector('[data-f="id"]').value.trim(),
    fuse_amps: Number(tr.querySelector('[data-f="fuse_amps"]').value),
    phases: parseInt(tr.querySelector('[data-f="phases"]').value, 10),
    voltage: Number(tr.querySelector('[data-f="voltage"]').value),
  }));
}

function refreshCircuitIds() {
  circuitIdsCache = readCircuits()
    .map((c) => c.id)
    .filter(Boolean);
}

els.btnAddCircuit.addEventListener("click", () => addCircuitRow({}));

els.btnSaveCircuits.addEventListener("click", async () => {
  const status = els.statusCircuits;
  status.textContent = "saving…";
  status.className = "form-status pending";
  try {
    await saveSection("circuits", readCircuits());
    status.textContent = "saved ✓";
    status.className = "form-status ok";
    showToast("circuits saved");
    loadConfig();
  } catch (err) {
    status.textContent = `error: ${err.message}`;
    status.className = "form-status err";
    showToast(`circuits: ${err.message}`, "err");
  }
});

// ----------------------------------------------------------- batteries editor

function batteryCircuitOptions(selected) {
  if (circuitIdsCache.length === 0)
    return '<option value="" disabled selected>(add a circuit first)</option>';
  return circuitIdsCache
    .map(
      (id) =>
        `<option value="${escapeAttr(id)}"${selected === id ? " selected" : ""}>${escapeHtml(
          id
        )}</option>`
    )
    .join("");
}

function refreshBatteryCircuitDropdowns() {
  els.batteriesList.querySelectorAll("[data-f=circuit]").forEach((sel) => {
    const cur = sel.value;
    sel.innerHTML = batteryCircuitOptions(cur);
  });
}

function addBatteryCard(b = {}) {
  const card = document.createElement("div");
  card.className = "bat-card";
  card.innerHTML = `
    <div class="bat-grid">
      <label>id<input data-f="id" type="text" value="${escapeAttr(b.id || "")}" required></label>
      <label>address (Marstek IP)<input data-f="address" type="text" value="${escapeAttr(b.address || "")}" required></label>
      <label>circuit<select data-f="circuit" required>${batteryCircuitOptions(b.circuit)}</select></label>
      <label>plug_url<input data-f="plug_url" type="text" placeholder="http://192.168.x.y" value="${escapeAttr(b.plug_url || "")}" required></label>
      <label>max_charge_w<input data-f="max_charge_w" type="number" min="0" step="any" value="${b.max_charge_w ?? 2500}" required></label>
      <label>max_discharge_w<input data-f="max_discharge_w" type="number" min="0" step="any" value="${b.max_discharge_w ?? 800}" required></label>
      <label>capacity_wh<input data-f="capacity_wh" type="number" min="0" step="any" value="${b.capacity_wh ?? 2500}"></label>
      <label>priority_weight<input data-f="priority_weight" type="number" min="0.01" step="any" value="${b.priority_weight ?? 1.0}" required></label>
      <label>vendor
        <select data-f="vendor">
          <option value="marstek"${(b.vendor || "marstek") === "marstek" ? " selected" : ""}>marstek</option>
          <option value="hoymiles"${b.vendor === "hoymiles" ? " selected" : ""}>hoymiles</option>
          <option value="generic"${b.vendor === "generic" ? " selected" : ""}>generic</option>
        </select>
      </label>
      <label>marstek_port<input data-f="marstek_port" type="number" min="1" max="65535" value="${b.marstek_port ?? 30000}"></label>
      <label>soc_interval_ms<input data-f="soc_interval_ms" type="number" min="1000" max="600000" value="${b.soc_interval_ms ?? 30000}"></label>
      <label>soc_full_pct (override; blank = dispatcher default)<input data-f="soc_full_pct" type="number" min="0" max="100" step="any" value="${b.soc_full_pct == null ? "" : b.soc_full_pct}"></label>
      <label>soc_empty_pct (override; blank = dispatcher default)<input data-f="soc_empty_pct" type="number" min="0" max="100" step="any" value="${b.soc_empty_pct == null ? "" : b.soc_empty_pct}"></label>
      <label>soc_entity_id (HA, optional)<input data-f="soc_entity_id" type="text" placeholder="sensor.battery_a_soc" value="${escapeAttr(b.soc_entity_id || "")}"></label>
    </div>
    <div class="bat-actions">
      <button type="button" class="ghost" data-act="remove">remove battery</button>
    </div>
  `;
  els.batteriesList.appendChild(card);
  card.querySelector('[data-act="remove"]').addEventListener("click", () => card.remove());
}

function renderBatteriesEditor(batteries) {
  els.batteriesList.innerHTML = "";
  for (const b of batteries) addBatteryCard(b);
}

function readBatteries() {
  const cards = [...els.batteriesList.querySelectorAll(".bat-card")];
  return cards.map((card) => {
    const get = (f) => card.querySelector(`[data-f="${f}"]`).value.trim();
    const num = (f) => {
      const v = card.querySelector(`[data-f="${f}"]`).value;
      return v === "" ? null : Number(v);
    };
    const out = {
      id: get("id"),
      address: get("address"),
      circuit: get("circuit"),
      plug_url: get("plug_url"),
      max_charge_w: num("max_charge_w"),
      max_discharge_w: num("max_discharge_w"),
      capacity_wh: num("capacity_wh") ?? 0,
      priority_weight: num("priority_weight") ?? 1.0,
      vendor: get("vendor") || "marstek",
      marstek_port: num("marstek_port") ?? 30000,
      soc_interval_ms: num("soc_interval_ms") ?? 30000,
    };
    const ent = get("soc_entity_id");
    if (ent) out.soc_entity_id = ent;
    const socFull = num("soc_full_pct");
    if (socFull != null) out.soc_full_pct = socFull;
    const socEmpty = num("soc_empty_pct");
    if (socEmpty != null) out.soc_empty_pct = socEmpty;
    return out;
  });
}

els.btnAddBattery.addEventListener("click", () => addBatteryCard({}));

els.btnSaveBatteries.addEventListener("click", async () => {
  const status = els.statusBatteries;
  status.textContent = "saving…";
  status.className = "form-status pending";
  try {
    await saveSection("batteries", readBatteries());
    status.textContent = "saved ✓ — restart add-on to apply";
    status.className = "form-status ok";
    showToast("batteries saved (restart required)");
    loadConfig();
  } catch (err) {
    status.textContent = `error: ${err.message}`;
    status.className = "form-status err";
    showToast(`batteries: ${err.message}`, "err");
  }
});

// ----------------------------------------------------------- bootstrap

document.addEventListener("DOMContentLoaded", () => {
  fetchStatus();
  setInterval(fetchStatus, REFRESH_MS);
  loadConfig();
});
