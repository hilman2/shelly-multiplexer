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
  modePill: document.getElementById("mode-pill"),
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
    if (btn.dataset.view === "details") {
      refreshDetails();
    }
  });
});

// ----------------------------------------------------------- battery details

let detailsTimer = null;

async function refreshDetails() {
  try {
    const dbg = await fetch("/api/modbus/debug").then((r) => r.json());
    renderModbusStats(dbg);
    const decodedPerBattery = await Promise.all(
      (dbg.batteries || []).map(async (b) => {
        try {
          return await fetch(`/api/modbus/decoded/${encodeURIComponent(b.id)}`).then((r) =>
            r.json(),
          );
        } catch (e) {
          return { battery_id: b.id, error: String(e) };
        }
      }),
    );
    renderBatteryDetails(decodedPerBattery, dbg.batteries || []);
  } catch (e) {
    document.getElementById("details-body").innerHTML =
      `<div class="hint" style="color:var(--err)">debug fetch failed: ${escapeHtml(String(e))}</div>`;
  }
  // re-arm only while the tab is active
  clearTimeout(detailsTimer);
  if (document.getElementById("view-details").classList.contains("active")) {
    detailsTimer = setTimeout(refreshDetails, 30000);
  }
}

function renderModbusStats(dbg) {
  const body = document.getElementById("modbus-stats-body");
  if (!body) return;
  const s = dbg.server || {};
  const o = dbg.outbound || {};
  const okRate = s.requests_total ? ((s.requests_ok / s.requests_total) * 100).toFixed(1) : "–";
  const outboundOkRate = o.reads_total
    ? ((o.reads_ok / o.reads_total) * 100).toFixed(1)
    : "–";
  body.innerHTML = `
    <div><div class="label">Connections accepted</div><div class="value">${s.connections_accepted ?? 0}</div></div>
    <div><div class="label">Server requests (total / ok)</div><div class="value">${s.requests_total ?? 0} / ${s.requests_ok ?? 0} (${okRate}%)</div></div>
    <div><div class="label">→ IllegalDataAddress</div><div class="value">${s.requests_illegal_address ?? 0}</div></div>
    <div><div class="label">→ ServerDeviceBusy</div><div class="value">${s.requests_server_busy ?? 0}</div></div>
    <div><div class="label">→ IllegalFunction</div><div class="value">${s.requests_illegal_function ?? 0}</div></div>
    <div><div class="label">→ GatewayPathUnavailable</div><div class="value">${s.requests_gateway_unavailable ?? 0}</div></div>
    <div><div class="label">Outbound reads (total / ok)</div><div class="value">${o.reads_total ?? 0} / ${o.reads_ok ?? 0} (${outboundOkRate}%)</div></div>
    <div><div class="label">Outbound reads failed</div><div class="value">${o.reads_failed ?? 0}</div></div>
    <div><div class="label">Outbound writes (total / ok / fail)</div><div class="value">${o.writes_total ?? 0} / ${o.writes_ok ?? 0} / ${o.writes_failed ?? 0}</div></div>
  `;
}

function renderBatteryDetails(decodedList, dbgBatteries) {
  const body = document.getElementById("details-body");
  if (!body) return;
  if (decodedList.length === 0) {
    body.innerHTML = '<div class="hint">no batteries configured</div>';
    return;
  }
  const dbgById = Object.fromEntries(dbgBatteries.map((b) => [b.id, b]));
  body.innerHTML = decodedList
    .map((d) => {
      const dbg = dbgById[d.battery_id] || {};
      if (d.error) {
        return `<div class="card"><h3>${escapeHtml(d.battery_id)}</h3>
          <div class="hint" style="color:var(--err)">${escapeHtml(d.error)}</div></div>`;
      }
      const sections = {};
      (d.registers || []).forEach((r) => {
        if (!sections[r.section]) sections[r.section] = [];
        sections[r.section].push(r);
      });
      const sectionOrder = [
        "Battery",
        "AC Grid",
        "AC Off-grid",
        "MPPT",
        "Energy",
        "Temperature",
        "Cells",
        "State",
        "Control",
        "BMS",
        "Connectivity",
        "Metadata",
      ];
      const sectionHtml = sectionOrder
        .filter((name) => sections[name])
        .map((name) => {
          const rows = sections[name]
            .map(
              (r) => `
            <tr>
              <td class="reg-name">${escapeHtml(r.name)}</td>
              <td class="reg-value">${formatRegisterValue(r)}</td>
              <td class="reg-addr"><code>${r.address}</code></td>
            </tr>`,
            )
            .join("");
          return `
            <details open class="register-section">
              <summary><strong>${escapeHtml(name)}</strong> <small>(${sections[name].length})</small></summary>
              <table class="register-table"><tbody>${rows}</tbody></table>
            </details>`;
        })
        .join("");
      const age = dbg.cache_refreshed_age_s;
      const ageLabel =
        age == null ? "never refreshed" : `${age.toFixed(1)} s ago`;
      return `
        <div class="card">
          <h3>
            ${escapeHtml(d.battery_id)}
            <span class="mode-pill">${escapeHtml(d.marstek_model || "")}</span>
            <span class="mode-pill" title="virtual Modbus unit ID HA reads under">unit ${dbg.virtual_unit_id ?? "?"}</span>
          </h3>
          <p class="hint">Cache: ${dbg.cache_size ?? 0} registers · refreshed ${escapeHtml(ageLabel)}</p>
          ${sectionHtml || '<div class="hint">no decoded sections yet</div>'}
        </div>`;
    })
    .join("");
}

function formatRegisterValue(r) {
  if (r.value === null || r.value === undefined) {
    return `<span class="hint">–</span>`;
  }
  if (typeof r.value === "number") {
    const formatted = Number.isInteger(r.value) ? r.value : r.value.toFixed(4).replace(/\.?0+$/, "");
    return `<strong>${formatted}</strong>${r.unit ? ` <small>${escapeHtml(r.unit)}</small>` : ""}`;
  }
  return `<strong>${escapeHtml(String(r.value))}</strong>`;
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// ----------------------------------------------------------- status

function renderCircuitsStatus(circuits) {
  if (!circuits || circuits.length === 0) {
    els.circuitsStatusBody.innerHTML =
      '<tr><td colspan="6" class="empty">no circuits configured</td></tr>';
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
  // 10 columns: Battery, Circuit, IP, Plug, Δ since send, Pulse, SoC, Plug age, Last poll, State
  els.batteriesStatusBody.innerHTML = batteries
    .slice()
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((b) => {
      // Multiple state pills can be true at once. Order matters: the
      // first pill is the dominant one (error / silent / stale beat
      // taper / limit annotations).
      const pills = [];
      if (b.plug_cut_for_ms != null) {
        const reason = b.plug_cut_reason || "cutoff active";
        const remaining = fmtMs(b.plug_cut_for_ms);
        // Night cutoff is an EFFICIENCY feature, not an emergency.
        // Render it with a softer label so the user doesn't panic.
        const isNight = reason.startsWith("night cutoff:");
        const label = isNight ? "NIGHT SAVING" : "PLUG OFF";
        const cls = isNight ? "state nightcut" : "state cutoff";
        pills.push(
          `<span class="${cls}" title="${escapeAttr(reason)} — auto-recovery in ${remaining}">${label}</span>` +
          ` <button class="ghost reset-cutoff" type="button" data-bid="${escapeAttr(b.id)}" title="manual reset: re-enable the plug now">reset</button>`
        );
      }
      if (b.last_modbus_write_error) {
        pills.push(`<span class="state err" title="${escapeAttr(b.last_modbus_write_error)}">modbus error</span>`);
      } else if (b.last_modbus_setpoint_w != null) {
        const sp = b.last_modbus_setpoint_w;
        const label =
          sp === 0
            ? "standby"
            : sp < 0
            ? `charge ${Math.round(-sp)} W`
            : `discharge ${Math.round(sp)} W`;
        const age = b.last_modbus_write_ago_ms == null ? "?" : fmtMs(b.last_modbus_write_ago_ms);
        pills.push(`<span class="state info" title="last Modbus setpoint, written ${age} ago">${label}</span>`);
      }
      if (b.last_error) {
        pills.push(`<span class="state err" title="${escapeAttr(b.last_error)}">error</span>`);
      } else if (b.circuit_silent) {
        pills.push('<span class="state silent" title="circuit muted (stale plug or grid)">silent</span>');
      } else if (b.plug_age_ms == null || b.plug_age_ms > 2000) {
        pills.push('<span class="state warn">plug stale</span>');
      } else if (b.pulse_remaining > 0) {
        pills.push('<span class="state pulsing">pulsing</span>');
      } else {
        pills.push('<span class="state ok">idle</span>');
      }
      // Annotation pills — additive, can stack with the dominant pill.
      if (b.active === false) {
        pills.push('<span class="state info" title="no SoC source configured. In pulse mode the dispatcher falls back to empirical full/empty detection. Configure modbus_host (or soc_entity_id in HA mode) for direct SoC gating.">no SoC</span>');
      }
      if (b.charge_locked_for_ms != null) {
        pills.push(`<span class="state warn" title="charge direction locked for ${fmtMs(b.charge_locked_for_ms)} — empirical refusal detection thinks the battery is full. Discharge stays available.">charge locked (full?)</span>`);
      }
      if (b.discharge_locked_for_ms != null) {
        pills.push(`<span class="state warn" title="discharge direction locked for ${fmtMs(b.discharge_locked_for_ms)} — empirical refusal detection thinks the battery is empty. Charging stays available.">discharge locked (empty?)</span>`);
      }
      if (b.soc_full_gated) {
        pills.push('<span class="state warn" title="SoC ≥ soc_full_pct: charging is fully gated to 0 W">SoC full</span>');
      }
      if (b.soc_empty_gated) {
        pills.push('<span class="state warn" title="SoC ≤ soc_empty_pct: discharging is fully gated to 0 W">SoC empty</span>');
      }
      if (b.charge_tapered) {
        const cap = Math.round(b.effective_max_charge_w);
        pills.push(`<span class="state taper" title="charge cap reduced to ${cap} W (BMS taper near full SoC)">charge taper ${cap} W</span>`);
      }
      if (b.discharge_tapered) {
        const cap = Math.round(b.effective_max_discharge_w);
        pills.push(`<span class="state taper" title="discharge cap reduced to ${cap} W (BMS taper near empty SoC)">discharge taper ${cap} W</span>`);
      }
      if (b.at_charge_limit) {
        const cap = Math.round(b.effective_max_charge_w);
        pills.push(`<span class="state limit" title="plug at ≥ 95 % of effective charge cap (${cap} W)">at charge max</span>`);
      }
      if (b.at_discharge_limit) {
        const cap = Math.round(b.effective_max_discharge_w);
        pills.push(`<span class="state limit" title="plug at ≥ 95 % of effective discharge cap (${cap} W)">at discharge max</span>`);
      }
      const stateCell = pills.join(" ");

      // Direction shown next to the battery name should reflect the
      // ACTUAL operating state, not just plug power. The Marstek
      // inverter pulls ~5-10 W in standby and the plug PM Gen3 sees
      // that — without filtering we'd read "Charge" for a battery
      // that's doing nothing.
      //
      // Source priority:
      //  1. Modbus commanded setpoint (we KNOW what we asked for)
      //  2. Battery's own reported power (via Modbus reg 30001/32102)
      //  3. Plug reading, but only above a standby-loss threshold
      const STANDBY_DEADBAND_W = 20;
      let dirState;
      if (b.last_modbus_setpoint_w != null) {
        if (Math.abs(b.last_modbus_setpoint_w) < 1) dirState = "standby";
        else if (b.last_modbus_setpoint_w > 0) dirState = "discharge";
        else dirState = "charge";
      } else if (b.last_battery_power_w != null) {
        if (Math.abs(b.last_battery_power_w) < STANDBY_DEADBAND_W) dirState = "standby";
        else if (b.last_battery_power_w > 0) dirState = "discharge";
        else dirState = "charge";
      } else if (b.plug_w != null) {
        if (Math.abs(b.plug_w) < STANDBY_DEADBAND_W) dirState = "standby";
        else if (b.plug_w > 0) dirState = "discharge";
        else dirState = "charge";
      } else {
        dirState = null;
      }
      const dir =
        dirState === "discharge"
          ? '<span class="dir-tag dir-discharge">discharge</span>'
          : dirState === "charge"
          ? '<span class="dir-tag dir-charge">charge</span>'
          : dirState === "standby"
          ? '<span class="dir-tag dir-standby">standby</span>'
          : "";
      const pulseCell =
        b.pulse_remaining > 0
          ? `<span class="pulses" title="${escapeAttr(fmtW(b.pending_pulse_w))} W × ${b.pulse_remaining} polls remaining">${b.pulse_remaining}×${fmtW(b.pending_pulse_w)}W</span>`
          : b.pending_pulse_w !== 0
          ? `<span class="dim" title="last pulse value (cycle done)">${fmtW(b.pending_pulse_w)} W</span>`
          : '<span class="dim">–</span>';
      // Δ since send: how much the plug has moved relative to the snapshot
      // taken when the most recent pulse cycle started. > deadband counts
      // as "battery responded".
      let deltaCell;
      if (b.plug_w_at_pulse_send != null && b.plug_w != null) {
        const moved = b.plug_w - b.plug_w_at_pulse_send;
        deltaCell = `<span title="plug at pulse send: ${b.plug_w_at_pulse_send.toFixed(0)} W">${fmtW(moved)} W</span>`;
      } else {
        deltaCell = '<span class="dim">–</span>';
      }
      const socCell = `<span title="${escapeAttr(b.soc_source || "no source yet")}">${fmtPct(b.soc_pct)}</span>`;
      return `<tr>
        <td><strong>${escapeHtml(b.id)}</strong> ${dir}</td>
        <td>${escapeHtml(b.circuit)}</td>
        <td class="dim">${escapeHtml(b.address)}</td>
        <td class="num">${fmtW(b.plug_w)} W</td>
        <td class="num">${deltaCell}</td>
        <td class="num">${pulseCell}</td>
        <td class="num">${socCell}</td>
        <td class="num">${fmtMs(b.plug_age_ms)}</td>
        <td class="num">${fmtMs(b.last_marstek_poll_ms_ago)}</td>
        <td class="state-cell">${stateCell}</td>
      </tr>`;
    })
    .join("");
}

// Delegate click handler for the per-battery cutoff-reset buttons. We
// can't bind in renderBatteriesStatus because the table is re-rendered
// every second; a single delegated handler on the table body survives.
document.addEventListener("click", async (ev) => {
  const target = ev.target;
  if (!(target instanceof HTMLElement)) return;
  if (!target.classList.contains("reset-cutoff")) return;
  const bid = target.getAttribute("data-bid");
  if (!bid) return;
  target.disabled = true;
  try {
    const res = await fetch(api(`/api/cutoff/${encodeURIComponent(bid)}/reset`), {
      method: "POST",
    });
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: `HTTP ${res.status}` }));
      throw new Error(err.error || `HTTP ${res.status}`);
    }
    showToast(`cutoff reset for ${bid}`);
  } catch (e) {
    showToast(`cutoff reset failed: ${e.message}`, "err");
  } finally {
    target.disabled = false;
  }
});

async function fetchStatus() {
  try {
    const res = await fetch(api("/api/status"), { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    els.gridW.textContent = data.grid_w == null ? "–" : data.grid_w.toFixed(0);
    els.gridDirection.textContent = directionLabel(data.grid_w);
    els.gridAge.textContent = fmtMs(data.grid_age_ms);
    if (els.cfgPath) els.cfgPath.textContent = data.config_path || "–";
    if (els.modePill && data.dispatch_mode) {
      els.modePill.textContent = `${data.dispatch_mode} mode`;
      els.modePill.classList.toggle("mode-pill-modbus", data.dispatch_mode === "modbus");
      els.modePill.classList.toggle("mode-pill-pulse", data.dispatch_mode === "pulse");
    }
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
    fillForm(document.getElementById("form-location"), "location", cachedConfig.location || {});
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
      <label class="soc-modbus">marstek_model
        <select data-f="marstek_model">
          <option value="venus_e_v1_v2"${(["venus_e_v1_v2", "venus_e_v12", "venus_e_v1v2", undefined, null, ""].includes(b.marstek_model) || !b.marstek_model) ? " selected" : ""}>Venus E v1 / v2 (most common — SoC reg 32104)</option>
          <option value="venus_e_v3"${(b.marstek_model === "venus_e_v3" || b.marstek_model === "venus_e") ? " selected" : ""}>Venus E v3 (SoC reg 34002)</option>
          <option value="venus_d"${b.marstek_model === "venus_d" ? " selected" : ""}>Venus D</option>
          <option value="venus_a"${b.marstek_model === "venus_a" ? " selected" : ""}>Venus A</option>
        </select>
      </label>
      <label class="soc-modbus">modbus_host — IP of the RS485-to-LAN bridge (Waveshare / EW11 / DR134 / M5Stack). On Venus E V3 with Ethernet cable, use the same value as "address". Leave blank to keep the battery inactive.<input data-f="modbus_host" type="text" placeholder="e.g. 192.168.1.91" value="${escapeAttr(b.modbus_host || "")}"></label>
      <label class="soc-modbus">modbus_port<input data-f="modbus_port" type="number" min="1" max="65535" value="${b.modbus_port ?? 502}"></label>
      <label class="soc-modbus">modbus_unit_id<input data-f="modbus_unit_id" type="number" min="1" max="255" value="${b.modbus_unit_id ?? 1}"></label>
      <label class="soc-modbus">virtual_unit_id (HA reads this battery under this unit ID on our virtual Modbus server; blank = index+1)<input data-f="virtual_unit_id" type="number" min="1" max="247" value="${b.virtual_unit_id == null ? "" : b.virtual_unit_id}"></label>
      <label>soc_interval_ms<input data-f="soc_interval_ms" type="number" min="1000" max="600000" value="${b.soc_interval_ms ?? 30000}"></label>
      <label>soc_full_pct (override; blank = dispatcher default)<input data-f="soc_full_pct" type="number" min="0" max="100" step="any" value="${b.soc_full_pct == null ? "" : b.soc_full_pct}"></label>
      <label>soc_empty_pct (override; blank = dispatcher default)<input data-f="soc_empty_pct" type="number" min="0" max="100" step="any" value="${b.soc_empty_pct == null ? "" : b.soc_empty_pct}"></label>
      <label>charge_taper_soc_pct (cap charge above this SoC)<input data-f="charge_taper_soc_pct" type="number" min="0" max="100" step="any" value="${b.charge_taper_soc_pct == null ? "" : b.charge_taper_soc_pct}"></label>
      <label>charge_taper_w (cap to this many W)<input data-f="charge_taper_w" type="number" min="0" step="any" value="${b.charge_taper_w == null ? "" : b.charge_taper_w}"></label>
      <label>discharge_taper_soc_pct (cap discharge below this SoC)<input data-f="discharge_taper_soc_pct" type="number" min="0" max="100" step="any" value="${b.discharge_taper_soc_pct == null ? "" : b.discharge_taper_soc_pct}"></label>
      <label>discharge_taper_w (cap to this many W)<input data-f="discharge_taper_w" type="number" min="0" step="any" value="${b.discharge_taper_w == null ? "" : b.discharge_taper_w}"></label>
      <label class="soc-ha">soc_entity_id (HA — required when HA mode is on)<input data-f="soc_entity_id" type="text" placeholder="sensor.battery_a_soc" value="${escapeAttr(b.soc_entity_id || "")}"></label>
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
  applyHaModeToBatteryEditor();
}

// Toggle visibility of Modbus-only / HA-only per-battery fields based on
// the CURRENT effective SoC source, which is decided by:
//   - dispatcher.mode = "modbus"          → SoC via Modbus on the battery
//   - dispatcher.mode = "pulse" + HA on   → SoC via HA entity
//   - dispatcher.mode = "pulse" + HA off  → SoC via Modbus
// The HA-only `soc_entity_id` field only appears in the second case.
// Modbus fields are always visible (they're needed for setpoint writes
// in modbus dispatch mode regardless of HA settings).
function applyHaModeToBatteryEditor() {
  const mode =
    (cachedConfig && cachedConfig.dispatcher && cachedConfig.dispatcher.mode) || "modbus";
  const haEnabled = !!(cachedConfig && cachedConfig.home_assistant && cachedConfig.home_assistant.enabled);
  const haActive = mode === "pulse" && haEnabled;
  // Modbus fields always shown — modbus_host / marstek_model / port /
  // unit_id are needed for setpoint writes in modbus dispatch AND for
  // SoC poll in pulse-without-HA. The only case they're truly unused
  // is pulse + HA, and even then we show them so users can switch
  // back without losing config.
  els.batteriesList.querySelectorAll(".soc-modbus").forEach((el) => {
    el.style.display = "";
  });
  els.batteriesList.querySelectorAll(".soc-ha").forEach((el) => {
    el.style.display = haActive ? "" : "none";
  });
  // Banner inside the HA form telling the user it's inert in modbus mode.
  const haWarn = document.getElementById("ha-modbus-warning");
  if (haWarn) {
    haWarn.style.display = mode === "modbus" ? "" : "none";
  }
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
      marstek_model: get("marstek_model") || "venus_e_v1_v2",
      modbus_port: num("modbus_port") ?? 502,
      modbus_unit_id: num("modbus_unit_id") ?? 1,
      soc_interval_ms: num("soc_interval_ms") ?? 30000,
    };
    const vUnit = num("virtual_unit_id");
    if (vUnit != null) out.virtual_unit_id = vUnit;
    const mbHost = get("modbus_host");
    if (mbHost) out.modbus_host = mbHost;
    const ent = get("soc_entity_id");
    if (ent) out.soc_entity_id = ent;
    const socFull = num("soc_full_pct");
    if (socFull != null) out.soc_full_pct = socFull;
    const socEmpty = num("soc_empty_pct");
    if (socEmpty != null) out.soc_empty_pct = socEmpty;
    const chargeTaperSoc = num("charge_taper_soc_pct");
    if (chargeTaperSoc != null) out.charge_taper_soc_pct = chargeTaperSoc;
    const chargeTaperW = num("charge_taper_w");
    if (chargeTaperW != null) out.charge_taper_w = chargeTaperW;
    const dischargeTaperSoc = num("discharge_taper_soc_pct");
    if (dischargeTaperSoc != null) out.discharge_taper_soc_pct = dischargeTaperSoc;
    const dischargeTaperW = num("discharge_taper_w");
    if (dischargeTaperW != null) out.discharge_taper_w = dischargeTaperW;
    return out;
  });
}

els.btnAddBattery.addEventListener("click", () => {
  addBatteryCard({});
  applyHaModeToBatteryEditor();
});

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
