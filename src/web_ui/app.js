// ShellyMultiplexer admin UI - pulse-mode read-only status.

const REFRESH_MS = 1000;

const els = {
  connState: document.getElementById('conn-state'),
  lastSeen: document.getElementById('last-seen'),
  gridW: document.getElementById('grid-w'),
  gridDirection: document.getElementById('grid-direction'),
  gridAge: document.getElementById('grid-age'),
  circuitsBody: document.getElementById('circuits-body'),
  batteriesBody: document.getElementById('batteries-body'),
  configDump: document.getElementById('config-dump'),
  cfgPath: document.getElementById('cfg-path'),
};

let consecutiveErrors = 0;

function fmtW(v) {
  if (v == null || isNaN(v)) return '–';
  const sign = v >= 0 ? '+' : '';
  return `${sign}${v.toFixed(0)}`;
}

function fmtMs(ms) {
  if (ms == null || isNaN(ms)) return '–';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)} min`;
  return `${(ms / 3_600_000).toFixed(1)} h`;
}

function fmtPct(v) {
  if (v == null || isNaN(v)) return '–';
  return `${v.toFixed(1)} %`;
}

function setConnOk(ok) {
  els.connState.classList.toggle('ok', ok);
  els.connState.classList.toggle('bad', !ok);
}

function directionLabel(w) {
  if (w == null) return '–';
  if (Math.abs(w) < 5) return 'balanced';
  return w > 0 ? `importing ${w.toFixed(0)} W` : `exporting ${(-w).toFixed(0)} W`;
}

function escapeHtml(s) {
  if (s == null) return '';
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function escapeAttr(s) {
  return escapeHtml(s).replace(/"/g, '&quot;');
}

function renderCircuits(circuits) {
  if (!circuits || circuits.length === 0) {
    els.circuitsBody.innerHTML = '<tr><td colspan="7" class="empty">no circuits configured</td></tr>';
    return;
  }
  els.circuitsBody.innerHTML = circuits
    .slice()
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((c) => {
      const cap = c.cap_w * 0.95;
      const measuredAbs = Math.abs(c.measured_sum_w);
      const headroom = cap - measuredAbs;
      const headroomClass = headroom < 0 ? 'cap-over' : headroom < cap * 0.1 ? 'cap-tight' : 'cap-ok';
      const stateCell = c.silent_for_ms != null
        ? `<span class="state silent">SILENT (${fmtMs(c.silent_for_ms)})</span>`
        : '<span class="state ok">active</span>';
      const members = (c.member_ids || []).join(', ') || '<span class="dim">none</span>';
      return `<tr>
        <td><strong>${escapeHtml(c.id)}</strong></td>
        <td>${c.cap_w.toFixed(0)} W <span class="dim">(${c.fuse_amps} A × ${c.phases} ph)</span></td>
        <td class="num">${fmtW(c.commanded_sum_w)} W</td>
        <td class="num">${fmtW(c.measured_sum_w)} W</td>
        <td class="num ${headroomClass}">${headroom.toFixed(0)} W</td>
        <td>${stateCell}</td>
        <td class="dim">${members}</td>
      </tr>`;
    })
    .join('');
}

function renderBatteries(batteries) {
  if (!batteries || batteries.length === 0) {
    els.batteriesBody.innerHTML = '<tr><td colspan="10" class="empty">no batteries configured</td></tr>';
    return;
  }
  els.batteriesBody.innerHTML = batteries
    .slice()
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((b) => {
      let stateCell;
      if (b.last_error) {
        stateCell = `<span class="state err" title="${escapeAttr(b.last_error)}">error</span>`;
      } else if (b.saturated) {
        stateCell = `<span class="state warn" title="reached at ${fmtW(b.saturation_ceiling_w)} W">saturated</span>`;
      } else if (b.plug_age_ms == null || b.plug_age_ms > 2000) {
        stateCell = `<span class="state warn">plug stale</span>`;
      } else if (b.pulse_queue_len > 0) {
        stateCell = `<span class="state pulsing">pulsing</span>`;
      } else {
        stateCell = `<span class="state ok">idle</span>`;
      }
      const directionTag = (b.commanded_w > 5)
        ? '<span class="dir-tag dir-discharge">discharge</span>'
        : (b.commanded_w < -5)
          ? '<span class="dir-tag dir-charge">charge</span>'
          : '';
      const queue = b.pulse_queue_len > 0
        ? `<span class="pulses">${b.pulse_queue_len}</span>`
        : '<span class="dim">–</span>';
      return `<tr>
        <td><strong>${escapeHtml(b.id)}</strong> ${directionTag}</td>
        <td>${escapeHtml(b.circuit)}</td>
        <td class="dim">${escapeHtml(b.address)}</td>
        <td class="num">${fmtW(b.commanded_w)} W</td>
        <td class="num">${fmtW(b.plug_w)} W</td>
        <td class="num">${queue}</td>
        <td class="num">${fmtPct(b.soc_pct)}</td>
        <td>${fmtMs(b.plug_age_ms)}</td>
        <td>${fmtMs(b.last_marstek_poll_ms_ago)}</td>
        <td>${stateCell}</td>
      </tr>`;
    })
    .join('');
}

async function fetchStatus() {
  try {
    const res = await fetch('api/status', { cache: 'no-store' });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    els.gridW.textContent = data.grid_w == null ? '–' : data.grid_w.toFixed(0);
    els.gridDirection.textContent = directionLabel(data.grid_w);
    els.gridAge.textContent = fmtMs(data.grid_age_ms);
    renderCircuits(data.circuits);
    renderBatteries(data.batteries);
    setConnOk(true);
    els.lastSeen.textContent = `updated ${new Date().toLocaleTimeString()}`;
    consecutiveErrors = 0;
  } catch (err) {
    consecutiveErrors++;
    setConnOk(false);
    els.lastSeen.textContent = `connection error (${consecutiveErrors}): ${err.message}`;
  }
}

function loadConfigHint() {
  els.configDump.textContent = `# config.toml is loaded once at startup.
# To change settings, edit /config/config.toml (via Studio Code Server)
# and restart this add-on. Live editing was removed with the multiplex
# layer - the new pulse architecture depends on a static plug topology.

[real_shelly]
host = "192.168.1.50"      # IP of the Shelly Pro 3EM measuring grid power
udp_port = 2020

[dispatcher]
cycle_ms = 200
deadband_w = 30             # ignore deltas smaller than this
hit_tolerance_w = 15        # |commanded - plug| <= this counts as "pulse landed"
pulse_count = 3             # pulses per delta change (Marstek needs >= 2)
plug_stale_s = 2.0
group_silent_after_stale_s = 60.0
circuit_headroom = 0.95
saturation_gap_w = 100
saturation_window_s = 8

[[circuits]]
id = "1"
fuse_amps = 16

[[battery]]
id = "A"
address = "192.168.1.61"               # static IP of the Marstek
circuit = "1"
plug_url = "http://192.168.1.71"       # MANDATORY Shelly Plug PM Gen3
max_charge_w = 2500
max_discharge_w = 800
capacity_wh = 2500
priority_weight = 1.0
`;
}

document.addEventListener('DOMContentLoaded', () => {
  loadConfigHint();
  fetchStatus();
  setInterval(fetchStatus, REFRESH_MS);
});
