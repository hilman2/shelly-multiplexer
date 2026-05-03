"use strict";

// Resolve API URLs relative to the document base. Direct access uses
// "/api/...", but when this UI is served behind Home Assistant Ingress
// the document is rooted at "/api/hassio_ingress/<token>/" and we need
// "api/..." (no leading slash) so the prefix is preserved.
function api(path) {
  // path starts with "/api/..." for clarity; strip the leading "/" so
  // the URL is relative to whatever base the document was loaded under.
  return path.replace(/^\//, "");
}

const els = {
  connState: document.getElementById("conn-state"),
  lastSeen: document.getElementById("last-seen"),
  uptime: document.getElementById("uptime"),
  pa: document.getElementById("p-a"),
  pb: document.getElementById("p-b"),
  pc: document.getElementById("p-c"),
  ptotal: document.getElementById("p-total"),
  va: document.getElementById("v-a"),
  vb: document.getElementById("v-b"),
  vc: document.getElementById("v-c"),
  direction: document.getElementById("direction-label"),
  eaCons: document.getElementById("e-a-cons"),
  eaRet: document.getElementById("e-a-ret"),
  ebCons: document.getElementById("e-b-cons"),
  ebRet: document.getElementById("e-b-ret"),
  ecCons: document.getElementById("e-c-cons"),
  ecRet: document.getElementById("e-c-ret"),
  allocBody: document.getElementById("alloc-body"),
  toast: document.getElementById("toast"),
  cfgPath: document.getElementById("cfg-path"),
  formReal: document.getElementById("form-real"),
  formVirtual: document.getElementById("form-virtual"),
  formManagement: document.getElementById("form-management"),
  formDispatcher: document.getElementById("form-dispatcher"),
  formHa: document.getElementById("form-ha"),
  circuitsBody: document.getElementById("circuits-body"),
  batteriesList: document.getElementById("batteries-list"),
  btnAddCircuit: document.getElementById("btn-add-circuit"),
  btnSaveCircuits: document.getElementById("btn-save-circuits"),
  statusCircuits: document.getElementById("status-circuits"),
  btnAddBattery: document.getElementById("btn-add-battery"),
  btnSaveBatteries: document.getElementById("btn-save-batteries"),
  statusBatteries: document.getElementById("status-batteries"),
};

document.querySelectorAll("nav button").forEach(btn => {
  btn.addEventListener("click", () => {
    document.querySelectorAll("nav button").forEach(b => b.classList.remove("active"));
    document.querySelectorAll(".view").forEach(v => v.classList.remove("active"));
    btn.classList.add("active");
    document.getElementById("view-" + btn.dataset.view).classList.add("active");
  });
});

function fmtPower(w) {
  if (w === null || w === undefined) return "–";
  if (Math.abs(w) >= 1000) return (w / 1000).toFixed(2) + "k";
  return w.toFixed(0);
}
function fmtVoltage(v) { return v === null || v === undefined ? "–" : v.toFixed(1); }
function fmtKwh(wh) { return wh === null || wh === undefined ? "–" : (wh / 1000).toFixed(3); }
function fmtPct(p) { return p === null || p === undefined ? "–" : p.toFixed(0) + "%"; }
function fmtTemp(t) { return t === null || t === undefined ? "–" : t.toFixed(1) + "°C"; }
function fmtUptime(s) {
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}
function fmtAge(ms) {
  if (ms === null || ms === undefined) return "never";
  if (ms < 1000) return ms + " ms";
  if (ms < 60000) return Math.floor(ms / 1000) + " s";
  if (ms < 3600000) return Math.floor(ms / 60000) + " min";
  return Math.floor(ms / 3600000) + " h";
}
function escape(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function setPhase(el, w) {
  el.textContent = fmtPower(w);
  el.classList.remove("import", "export");
  if (w === null || w === undefined) return;
  if (w > 5) el.classList.add("import");
  else if (w < -5) el.classList.add("export");
}

function showToast(message, type) {
  els.toast.textContent = message;
  els.toast.className = type ? "show " + type : "show";
  setTimeout(() => els.toast.classList.remove("show"), 4000);
}

async function refresh() {
  try {
    const res = await fetch(api("/api/status"), { cache: "no-store" });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();

    const ms = data.real_shelly.last_seen_ms_ago;
    if (ms === null || ms === undefined) {
      els.connState.className = "dot dead";
      els.lastSeen.textContent = "No contact with Shelly";
    } else if (ms < 2000) {
      els.connState.className = "dot live";
      els.lastSeen.textContent = "Live (" + ms + " ms)";
    } else if (ms < 10000) {
      els.connState.className = "dot stale";
      els.lastSeen.textContent = "Stale (" + Math.floor(ms / 1000) + "s)";
    } else {
      els.connState.className = "dot dead";
      els.lastSeen.textContent = "Lost (" + Math.floor(ms / 1000) + "s)";
    }

    els.uptime.textContent = fmtUptime(data.uptime_seconds);

    const r = data.real_shelly;
    setPhase(els.pa, r.a_act_power);
    setPhase(els.pb, r.b_act_power);
    setPhase(els.pc, r.c_act_power);
    setPhase(els.ptotal, r.total_act_power);
    els.va.textContent = fmtVoltage(r.a_voltage);
    els.vb.textContent = fmtVoltage(r.b_voltage);
    els.vc.textContent = fmtVoltage(r.c_voltage);
    if (r.total_act_power === null || r.total_act_power === undefined) {
      els.direction.textContent = "–";
    } else if (r.total_act_power > 5) {
      els.direction.textContent = "Importing from grid";
    } else if (r.total_act_power < -5) {
      els.direction.textContent = "Exporting to grid";
    } else {
      els.direction.textContent = "Balanced";
    }

    const e = data.energy;
    els.eaCons.textContent = fmtKwh(e.a_consumed_wh);
    els.eaRet.textContent = fmtKwh(e.a_returned_wh);
    els.ebCons.textContent = fmtKwh(e.b_consumed_wh);
    els.ebRet.textContent = fmtKwh(e.b_returned_wh);
    els.ecCons.textContent = fmtKwh(e.c_consumed_wh);
    els.ecRet.textContent = fmtKwh(e.c_returned_wh);

    els.allocBody.innerHTML = "";
    data.allocations.sort((a, b) => a.battery_id.localeCompare(b.battery_id));
    for (const a of data.allocations) {
      const tr = document.createElement("tr");
      const liveMs = a.last_request_ms_ago;
      tr.className = liveMs !== null && liveMs < 5000 ? "live" : "stale";
      const socCell = a.soc_error
        ? `<span title="${escape(a.soc_error)}" style="color:var(--positive)">err</span>`
        : fmtPct(a.soc_percent);
      const noteCell = a.note ? `<span class="alloc-note">${escape(a.note)}</span>` : "";
      const multiplexCell = a.multiplex_inactive
        ? `<span class="restart-tag">standby</span>`
        : `<span class="ok-tag">active</span>`;
      const testBtn = `<button type="button" class="row-action btn-test-deactivate" data-battery="${escape(a.battery_id)}" title="Send no responses for 60 s — verifies the inverter shuts off via its CT-watchdog">deactivate 60 s</button>`;
      tr.innerHTML = `
        <td data-label="Battery">${escape(a.battery_id)} ${testBtn}</td>
        <td data-label="IP">${escape(a.address)}</td>
        <td data-label="Circuit">${escape(a.circuit || "–")}</td>
        <td class="num" data-label="SoC">${socCell}</td>
        <td data-label="SoC age">${fmtAge(a.soc_age_ms)}</td>
        <td class="num" data-label="Allocated"><strong>${fmtPower(a.allocated_w)}</strong></td>
        <td data-label="Multiplex">${multiplexCell}</td>
        <td data-label="Note">${noteCell}</td>
        <td data-label="Last request">${fmtAge(a.last_request_ms_ago)}</td>
      `;
      els.allocBody.appendChild(tr);
    }
    // Wire test-deactivate buttons.
    els.allocBody.querySelectorAll(".btn-test-deactivate").forEach(btn => {
      btn.addEventListener("click", async (e) => {
        const id = e.currentTarget.dataset.battery;
        if (!confirm(`Drop CT responses to "${id}" for 60 s?\n\nThe inverter should shut itself off via its CT-watchdog within ~30 s. Use this to verify the multiplex safety mechanism.`)) return;
        try {
          const res = await fetch(api(`/api/battery/${encodeURIComponent(id)}/test-deactivate?seconds=60`), { method: "POST" });
          const data = await res.json();
          if (!res.ok) {
            showToast("Error: " + (data.error || res.status), "error");
          } else {
            showToast(`Deactivated ${id} for 60 s`, "success");
          }
        } catch (err) {
          showToast("Network error: " + err.message, "error");
        }
      });
    });
  } catch (e) {
    els.connState.className = "dot dead";
    els.lastSeen.textContent = "API error: " + e.message;
  }
}

function flagDot(v) {
  if (v === null || v === undefined) return "<span style='color:var(--fg-dim)'>?</span>";
  return v
    ? "<span style='color:var(--negative)'>✓</span>"
    : "<span style='color:var(--positive)'>✗</span>";
}

// === Configuration editor ===

let currentConfig = null;

const SECTION_FIELDS = {
  real_shelly: {
    host: "string",
    udp_port: "int",
    poll_interval_ms: "int",
    request_timeout_ms: "int",
  },
  virtual_shelly: {
    bind_interface: "string",
    udp_port: "int",
    http_port: "int",
    min_sample_period_ms: "int",
    device_mac: "string",
    device_hostname: "string",
    firmware: "string",
  },
  management: {
    bind_address: "string",
  },
  dispatcher: {
    strategy: "string",
    rate_limit_w_per_s: "float",
    deadband_w: "float",
  },
  home_assistant: {
    enabled: "bool",
    url: "string",
    token: "string",
    timeout_ms: "int",
  },
};

function fillForm(form, section, data) {
  const spec = SECTION_FIELDS[section];
  for (const [field, type] of Object.entries(spec)) {
    const input = form.elements.namedItem(field);
    if (!input) continue;
    const value = data[field];
    if (type === "bool") {
      input.checked = !!value;
    } else if (value === null || value === undefined) {
      input.value = "";
    } else {
      input.value = value;
    }
  }
}

function readForm(form, section) {
  const spec = SECTION_FIELDS[section];
  const out = {};
  for (const [field, type] of Object.entries(spec)) {
    const input = form.elements.namedItem(field);
    if (!input) continue;
    if (type === "bool") {
      out[field] = !!input.checked;
    } else if (type === "int") {
      const n = parseInt(input.value, 10);
      if (!Number.isFinite(n)) throw new Error(`${field}: integer required`);
      out[field] = n;
    } else if (type === "float") {
      const n = parseFloat(input.value);
      if (!Number.isFinite(n)) throw new Error(`${field}: number required`);
      out[field] = n;
    } else {
      out[field] = input.value;
    }
  }
  return out;
}

async function postSection(section, payload, statusEl) {
  setStatus(statusEl, "Saving…", null);
  try {
    const res = await fetch(api("/api/config/section/") + encodeURIComponent(section), {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    const data = await res.json();
    if (!res.ok || !data.ok) {
      const msg = "Error: " + (data.error || res.status);
      setStatus(statusEl, msg, "error");
      showToast(msg, "error");
      return false;
    }
    setStatus(statusEl, "Saved", "success");
    showToast("Saved · " + section, "success");
    if (data.restart_hint) console.info(data.restart_hint);
    await loadConfig();
    return true;
  } catch (e) {
    const msg = "Network error: " + e.message;
    setStatus(statusEl, msg, "error");
    showToast(msg, "error");
    return false;
  }
}

function setStatus(el, text, kind) {
  if (!el) return;
  el.textContent = text || "";
  el.className = "form-status" + (kind ? " " + kind : "");
}

function bindSimpleForm(form, section) {
  const status = form.querySelector(".form-status");
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    let payload;
    try {
      payload = readForm(form, section);
    } catch (err) {
      setStatus(status, err.message, "error");
      return;
    }
    await postSection(section, payload, status);
  });
}

bindSimpleForm(els.formReal, "real_shelly");
bindSimpleForm(els.formVirtual, "virtual_shelly");
bindSimpleForm(els.formManagement, "management");
bindSimpleForm(els.formDispatcher, "dispatcher");
bindSimpleForm(els.formHa, "home_assistant");

// --- Circuits editor ---

function renderCircuits(circuits) {
  els.circuitsBody.innerHTML = "";
  for (const c of circuits) addCircuitRow(c);
}

function addCircuitRow(c) {
  c = c || { id: "", fuse_amps: 16, phases: 1, voltage: 230 };
  const tr = document.createElement("tr");
  tr.innerHTML = `
    <td><input data-field="id" type="text" value="${escape(c.id || "")}"></td>
    <td><input data-field="fuse_amps" type="number" min="0" step="any" value="${c.fuse_amps}"></td>
    <td>
      <select data-field="phases">
        <option value="1"${c.phases === 1 ? " selected" : ""}>1</option>
        <option value="3"${c.phases === 3 ? " selected" : ""}>3</option>
      </select>
    </td>
    <td><input data-field="voltage" type="number" min="1" step="any" value="${c.voltage}"></td>
    <td class="num cap-cell">–</td>
    <td><button type="button" class="secondary row-del">×</button></td>
  `;
  tr.querySelector(".row-del").addEventListener("click", () => tr.remove());
  tr.querySelectorAll("input,select").forEach(i =>
    i.addEventListener("input", () => recomputeCircuitCap(tr)));
  els.circuitsBody.appendChild(tr);
  recomputeCircuitCap(tr);
}

function recomputeCircuitCap(tr) {
  const fa = parseFloat(tr.querySelector('[data-field=fuse_amps]').value);
  const ph = parseInt(tr.querySelector('[data-field=phases]').value, 10);
  const v = parseFloat(tr.querySelector('[data-field=voltage]').value);
  const cell = tr.querySelector(".cap-cell");
  if (Number.isFinite(fa) && Number.isFinite(ph) && Number.isFinite(v)) {
    cell.textContent = Math.round(fa * ph * v) + " W";
  } else {
    cell.textContent = "–";
  }
}

function readCircuits() {
  const out = [];
  for (const tr of els.circuitsBody.querySelectorAll("tr")) {
    const id = tr.querySelector('[data-field=id]').value.trim();
    if (!id) throw new Error("circuit id must not be empty");
    const fa = parseFloat(tr.querySelector('[data-field=fuse_amps]').value);
    const ph = parseInt(tr.querySelector('[data-field=phases]').value, 10);
    const v = parseFloat(tr.querySelector('[data-field=voltage]').value);
    if (!Number.isFinite(fa) || !Number.isFinite(ph) || !Number.isFinite(v)) {
      throw new Error("circuit " + id + ": numeric values required");
    }
    out.push({ id, fuse_amps: fa, phases: ph, voltage: v });
  }
  return out;
}

els.btnAddCircuit.addEventListener("click", () => addCircuitRow());
els.btnSaveCircuits.addEventListener("click", async () => {
  let payload;
  try {
    payload = readCircuits();
  } catch (err) {
    setStatus(els.statusCircuits, err.message, "error");
    return;
  }
  await postSection("circuits", payload, els.statusCircuits);
});

// --- Batteries editor ---

function renderBatteries(batteries) {
  els.batteriesList.innerHTML = "";
  for (const b of batteries) addBatteryCard(b);
}

function addBatteryCard(b) {
  b = b || {
    id: "",
    address: "",
    vendor: "marstek",
    circuit: "",
    phase: "all",
    max_charge_w: 2500,
    max_discharge_w: 2500,
    min_soc_percent: 12,
    max_soc_percent: 100,
    priority: 1,
    marstek_port: 30000,
    telemetry_interval_ms: 60000,
    soc_entity_id: null,
  };
  // Backwards-compat: old configs may carry `group` instead of `circuit`.
  const currentCircuit = b.circuit || b.group || "";
  const circuitOptions = (currentConfig?.circuits || currentConfig?.groups || [])
    .map(c => `<option value="${escape(c.id)}"${currentCircuit === c.id ? " selected" : ""}>${escape(c.id)}</option>`)
    .join("");
  const card = document.createElement("div");
  card.className = "battery-card";
  card.innerHTML = `
    <div class="battery-grid">
      <label>id <input data-field="id" type="text" value="${escape(b.id || "")}"></label>
      <label>address (IP) <input data-field="address" type="text" value="${escape(b.address || "")}"></label>
      <label>vendor
        <select data-field="vendor">
          <option value="marstek"${b.vendor === "marstek" ? " selected" : ""}>marstek</option>
          <option value="hoymiles"${b.vendor === "hoymiles" ? " selected" : ""}>hoymiles</option>
          <option value="generic"${b.vendor === "generic" ? " selected" : ""}>generic</option>
        </select>
      </label>
      <label>circuit (required)
        <select data-field="circuit" required>
          <option value=""${!currentCircuit ? " selected" : ""}>— pick a circuit —</option>
          ${circuitOptions}
        </select>
      </label>
      <label>phase
        <select data-field="phase">
          <option value="a"${b.phase === "a" ? " selected" : ""}>a</option>
          <option value="b"${b.phase === "b" ? " selected" : ""}>b</option>
          <option value="c"${b.phase === "c" ? " selected" : ""}>c</option>
          <option value="all"${b.phase === "all" ? " selected" : ""}>all</option>
        </select>
      </label>
      <label>priority <input data-field="priority" type="number" min="0" step="1" value="${b.priority}"></label>
      <label>max_charge_w <input data-field="max_charge_w" type="number" min="0" step="any" value="${b.max_charge_w}"></label>
      <label>max_discharge_w <input data-field="max_discharge_w" type="number" min="0" step="any" value="${b.max_discharge_w}"></label>
      <label>min_soc_percent <input data-field="min_soc_percent" type="number" min="0" max="100" step="any" value="${b.min_soc_percent}"></label>
      <label>max_soc_percent <input data-field="max_soc_percent" type="number" min="0" max="100" step="any" value="${b.max_soc_percent}"></label>
      <label>marstek_port <span class="restart-tag">restart required</span>
        <input data-field="marstek_port" type="number" min="1" max="65535" value="${b.marstek_port}"></label>
      <label>telemetry_interval_ms
        <input data-field="telemetry_interval_ms" type="number" min="500" step="100" value="${b.telemetry_interval_ms}"></label>
      <label>soc_entity_id (HA, optional — overrides direct SoC poll)
        <input data-field="soc_entity_id" type="text" placeholder="sensor.marstek_venus_e_soc" value="${escape(b.soc_entity_id || "")}"></label>
    </div>
    <div class="form-actions">
      <button type="button" class="secondary card-del">Remove battery</button>
    </div>
  `;
  card.querySelector(".card-del").addEventListener("click", () => card.remove());
  els.batteriesList.appendChild(card);
}

function readBatteries() {
  const out = [];
  for (const card of els.batteriesList.querySelectorAll(".battery-card")) {
    const get = (f) => card.querySelector(`[data-field="${f}"]`).value;
    const getNum = (f) => {
      const n = parseFloat(get(f));
      if (!Number.isFinite(n)) throw new Error(f + ": number required");
      return n;
    };
    const getInt = (f) => {
      const n = parseInt(get(f), 10);
      if (!Number.isFinite(n)) throw new Error(f + ": integer required");
      return n;
    };
    const id = get("id").trim();
    if (!id) throw new Error("battery id must not be empty");
    const addr = get("address").trim();
    if (!addr) throw new Error("battery " + id + ": address required");
    const circuitVal = get("circuit");
    if (!circuitVal) throw new Error("battery " + id + ": circuit is required");
    const socEntity = get("soc_entity_id").trim();
    out.push({
      id,
      address: addr,
      vendor: get("vendor"),
      circuit: circuitVal,
      phase: get("phase"),
      max_charge_w: getNum("max_charge_w"),
      max_discharge_w: getNum("max_discharge_w"),
      min_soc_percent: getNum("min_soc_percent"),
      max_soc_percent: getNum("max_soc_percent"),
      priority: getInt("priority"),
      marstek_port: getInt("marstek_port"),
      telemetry_interval_ms: getInt("telemetry_interval_ms"),
      soc_entity_id: socEntity === "" ? null : socEntity,
    });
  }
  return out;
}

els.btnAddBattery.addEventListener("click", () => addBatteryCard());
els.btnSaveBatteries.addEventListener("click", async () => {
  let payload;
  try {
    payload = readBatteries();
  } catch (err) {
    setStatus(els.statusBatteries, err.message, "error");
    return;
  }
  await postSection("batteries", payload, els.statusBatteries);
});

async function loadConfig() {
  try {
    const res = await fetch(api("/api/config"), { cache: "no-store" });
    const cfg = await res.json();
    currentConfig = cfg;
    if (els.cfgPath) els.cfgPath.textContent = cfg.config_path || "config.toml";
    fillForm(els.formReal, "real_shelly", cfg.real_shelly);
    fillForm(els.formVirtual, "virtual_shelly", cfg.virtual_shelly);
    fillForm(els.formManagement, "management", cfg.management);
    fillForm(els.formDispatcher, "dispatcher", cfg.dispatcher);
    fillForm(els.formHa, "home_assistant", cfg.home_assistant || {});
    renderCircuits(cfg.circuits || cfg.groups || []);
    renderBatteries(cfg.batteries || []);
  } catch (e) {
    showToast("Failed to load config: " + e.message, "error");
  }
}

// === Phase detection ===

const btnPhaseDetect = document.getElementById("btn-phase-detect");
const phaseDetectStatus = document.getElementById("phase-detect-status");
const phaseDetectResults = document.getElementById("phase-detect-results");

if (btnPhaseDetect) {
  btnPhaseDetect.addEventListener("click", async () => {
    if (!confirm(
      "This pauses normal dispatch and drives each battery alone for ~50 s.\n" +
      "Make sure no critical loads depend on grid balance right now.\n\nProceed?"
    )) return;
    btnPhaseDetect.disabled = true;
    setStatus(phaseDetectStatus, "Starting…", null);
    try {
      const res = await fetch(api("/api/phase-detect"), { method: "POST" });
      const data = await res.json();
      if (!res.ok) {
        setStatus(phaseDetectStatus, "Error: " + (data.error || res.status), "error");
        btnPhaseDetect.disabled = false;
        return;
      }
      pollPhaseDetect();
    } catch (e) {
      setStatus(phaseDetectStatus, "Network error: " + e.message, "error");
      btnPhaseDetect.disabled = false;
    }
  });
}

async function pollPhaseDetect() {
  try {
    const res = await fetch(api("/api/phase-detect"), { cache: "no-store" });
    const s = await res.json();
    renderDetectStatus(s);
    if (s.running) {
      setTimeout(pollPhaseDetect, 1000);
    } else {
      btnPhaseDetect.disabled = false;
      // Reload config so the new detected_phase shows in the battery cards.
      loadConfig();
    }
  } catch (e) {
    setStatus(phaseDetectStatus, "Status error: " + e.message, "error");
    btnPhaseDetect.disabled = false;
  }
}

function renderDetectStatus(s) {
  const msg = s.message || (s.running ? "running…" : "idle");
  setStatus(phaseDetectStatus, msg, s.last_error ? "error" : (s.running ? null : "success"));
  if (!phaseDetectResults) return;
  const rows = Object.entries(s.results || {}).map(([id, r]) => {
    const pct = (r.confidence * 100).toFixed(0) + "%";
    const cls = r.confidence > 0.6 ? "" : "low-conf";
    return `<tr class="${cls}">
      <td>${escape(id)}</td>
      <td>${escape(r.phase)}</td>
      <td class="num">${pct}</td>
      <td class="num">${fmtPower(r.delta_a_w)}</td>
      <td class="num">${fmtPower(r.delta_b_w)}</td>
      <td class="num">${fmtPower(r.delta_c_w)}</td>
      <td>${escape(r.detected_at)}</td>
    </tr>`;
  }).join("");
  phaseDetectResults.innerHTML = rows
    ? `<table class="alloc"><thead><tr>
         <th>Battery</th><th>Phase</th><th>Confidence</th>
         <th>Δ L1</th><th>Δ L2</th><th>Δ L3</th><th>Detected at</th>
       </tr></thead><tbody>${rows}</tbody></table>`
    : "";
}

// On load, fetch any persisted detection state once so previous results are visible.
fetch(api("/api/phase-detect")).then(r => r.json()).then(renderDetectStatus).catch(() => {});

refresh();
loadConfig();
setInterval(refresh, 1000);
