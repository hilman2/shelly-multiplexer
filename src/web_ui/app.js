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
  headerCap: document.getElementById("header-cap"),
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
  safetyCard: document.getElementById("safety-card"),
  safetyCapValue: document.getElementById("safety-cap-value"),
  safetySource: document.getElementById("safety-source"),
  safetyBadge: document.getElementById("safety-badge"),
  ackRisk: document.getElementById("ack-risk"),
  ackFuses: document.getElementById("ack-fuses"),
  btnOverride: document.getElementById("btn-override"),
  btnReset: document.getElementById("btn-reset"),
  modal1: document.getElementById("modal-1"),
  modal2: document.getElementById("modal-2"),
  newCapInput: document.getElementById("new-cap-input"),
  toast: document.getElementById("toast"),
  cfgPath: document.getElementById("cfg-path"),
  formReal: document.getElementById("form-real"),
  formVirtual: document.getElementById("form-virtual"),
  formManagement: document.getElementById("form-management"),
  formDispatcher: document.getElementById("form-dispatcher"),
  formSafety: document.getElementById("form-safety"),
  formHa: document.getElementById("form-ha"),
  groupsBody: document.getElementById("groups-body"),
  batteriesList: document.getElementById("batteries-list"),
  btnAddGroup: document.getElementById("btn-add-group"),
  btnSaveGroups: document.getElementById("btn-save-groups"),
  statusGroups: document.getElementById("status-groups"),
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

function renderSafety(s) {
  els.safetyCapValue.textContent = (s.effective_cap_w || 0).toFixed(0) + " W";
  els.safetySource.textContent =
    "Source: " + (s.source === "runtime" ? "Web UI override" : "Config file") +
    (s.last_changed_ms_ago !== null && s.last_changed_ms_ago !== undefined
      ? " · " + fmtAge(s.last_changed_ms_ago) + " ago"
      : "");
  if (s.override_active) {
    els.safetyBadge.textContent = "Override active";
    els.safetyBadge.className = "badge override";
    els.safetyCard.classList.add("override-active");
  } else {
    els.safetyBadge.textContent = "Default cap";
    els.safetyBadge.className = "badge default";
    els.safetyCard.classList.remove("override-active");
  }
  setAck(els.ackRisk, s.acknowledged_higher_risk, "Risk acknowledged");
  setAck(els.ackFuses, s.acknowledged_separate_fuses, "Separate fuses confirmed");
  els.headerCap.textContent = (s.effective_cap_w || 0).toFixed(0) + " W";
}
function setAck(el, ok, label) {
  el.classList.toggle("set", !!ok);
  el.querySelector(".ack-icon").textContent = ok ? "✓" : "○";
  el.querySelector("span:last-child").textContent =
    label + (ok ? "" : " (pending)");
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
      tr.innerHTML = `
        <td>${escape(a.battery_id)}</td>
        <td>${escape(a.address)}</td>
        <td>${a.group ? escape(a.group) : "–"}</td>
        <td class="num">${socCell}</td>
        <td>${fmtAge(a.soc_age_ms)}</td>
        <td class="num"><strong>${fmtPower(a.allocated_w)}</strong></td>
        <td>${noteCell}</td>
        <td>${fmtAge(a.last_request_ms_ago)}</td>
      `;
      els.allocBody.appendChild(tr);
    }

    renderSafety(data.safety);
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
  safety: {
    max_total_w: "float",
    acknowledged_higher_risk: "bool",
    acknowledged_separate_fuses: "bool",
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
bindSimpleForm(els.formSafety, "safety");
bindSimpleForm(els.formHa, "home_assistant");

// --- Groups editor ---

function renderGroups(groups) {
  els.groupsBody.innerHTML = "";
  for (const g of groups) addGroupRow(g);
}

function addGroupRow(g) {
  g = g || { id: "", fuse_amps: 16, phases: 1, voltage: 230 };
  const tr = document.createElement("tr");
  tr.innerHTML = `
    <td><input data-field="id" type="text" value="${escape(g.id || "")}"></td>
    <td><input data-field="fuse_amps" type="number" min="0" step="any" value="${g.fuse_amps}"></td>
    <td>
      <select data-field="phases">
        <option value="1"${g.phases === 1 ? " selected" : ""}>1</option>
        <option value="3"${g.phases === 3 ? " selected" : ""}>3</option>
      </select>
    </td>
    <td><input data-field="voltage" type="number" min="1" step="any" value="${g.voltage}"></td>
    <td class="num cap-cell">–</td>
    <td><button type="button" class="secondary row-del">×</button></td>
  `;
  tr.querySelector(".row-del").addEventListener("click", () => tr.remove());
  tr.querySelectorAll("input,select").forEach(i =>
    i.addEventListener("input", () => recomputeGroupCap(tr)));
  els.groupsBody.appendChild(tr);
  recomputeGroupCap(tr);
}

function recomputeGroupCap(tr) {
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

function readGroups() {
  const out = [];
  for (const tr of els.groupsBody.querySelectorAll("tr")) {
    const id = tr.querySelector('[data-field=id]').value.trim();
    if (!id) throw new Error("group id must not be empty");
    const fa = parseFloat(tr.querySelector('[data-field=fuse_amps]').value);
    const ph = parseInt(tr.querySelector('[data-field=phases]').value, 10);
    const v = parseFloat(tr.querySelector('[data-field=voltage]').value);
    if (!Number.isFinite(fa) || !Number.isFinite(ph) || !Number.isFinite(v)) {
      throw new Error("group " + id + ": numeric values required");
    }
    out.push({ id, fuse_amps: fa, phases: ph, voltage: v });
  }
  return out;
}

els.btnAddGroup.addEventListener("click", () => addGroupRow());
els.btnSaveGroups.addEventListener("click", async () => {
  let payload;
  try {
    payload = readGroups();
  } catch (err) {
    setStatus(els.statusGroups, err.message, "error");
    return;
  }
  await postSection("groups", payload, els.statusGroups);
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
    group: null,
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
  const groupOptions = (currentConfig?.groups || [])
    .map(g => `<option value="${escape(g.id)}"${b.group === g.id ? " selected" : ""}>${escape(g.id)}</option>`)
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
      <label>group
        <select data-field="group">
          <option value=""${!b.group ? " selected" : ""}>—</option>
          ${groupOptions}
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
    const groupVal = get("group");
    const socEntity = get("soc_entity_id").trim();
    out.push({
      id,
      address: addr,
      vendor: get("vendor"),
      group: groupVal === "" ? null : groupVal,
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
    fillForm(els.formSafety, "safety", cfg.safety);
    renderGroups(cfg.groups || []);
    renderBatteries(cfg.batteries || []);
  } catch (e) {
    showToast("Failed to load config: " + e.message, "error");
  }
}

// === Safety override flow ===

function openModal(m) { m.classList.add("open"); }
function closeModals() {
  els.modal1.classList.remove("open");
  els.modal2.classList.remove("open");
}

document.querySelectorAll("[data-action=cancel]").forEach(b => {
  b.addEventListener("click", closeModals);
});
[els.modal1, els.modal2].forEach(m => {
  m.addEventListener("click", e => { if (e.target === m) closeModals(); });
});

els.btnOverride.addEventListener("click", () => openModal(els.modal1));

els.modal1.querySelector("[data-action=step1-ok]").addEventListener("click", () => {
  els.modal1.classList.remove("open");
  els.newCapInput.focus();
  openModal(els.modal2);
});

els.modal2.querySelector("[data-action=step2-ok]").addEventListener("click", async () => {
  const v = parseFloat(els.newCapInput.value);
  if (!isFinite(v) || v < 3000 || v > 20000) {
    showToast("Invalid value (allowed range: 3000–20000 W)", "error");
    return;
  }
  try {
    const res = await fetch(api("/api/safety"), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        max_total_w: v,
        acknowledged_higher_risk: true,
        acknowledged_separate_fuses: true,
      }),
    });
    const data = await res.json();
    if (!res.ok || !data.ok) {
      showToast("Error: " + (data.error || res.status), "error");
      return;
    }
    closeModals();
    showToast("Override active: " + data.effective_cap_w + " W", "success");
    refresh();
  } catch (e) {
    showToast("Network error: " + e.message, "error");
  }
});

els.btnReset.addEventListener("click", async () => {
  if (!confirm("Reset cap to the value from the configuration file?")) return;
  try {
    const res = await fetch(api("/api/safety/reset"), { method: "POST" });
    const data = await res.json();
    if (!res.ok || !data.ok) throw new Error(data.error || res.status);
    showToast("Cap reset: " + data.effective_cap_w + " W", "success");
    refresh();
  } catch (e) {
    showToast("Error: " + e.message, "error");
  }
});

refresh();
loadConfig();
setInterval(refresh, 1000);
