/**
 * Timers: the EU event schedule and boss respawn countdowns, with an alert
 * (banner on the meter and a sound) shortly before each and when it starts.
 *
 * Event times are server times on the EU servers (Europe/Berlin); the resets
 * follow UTC, so they move by an hour on the local clock when daylight saving
 * time starts or ends. Schedule as published by Shugo.gg and Questlog
 * (NCSOFT does not publish one for Global).
 *
 * Boss respawns start from the kill: the backend notes each tracked boss's
 * combat death (see `timers.trackedCodes`) as `timers.kill.<code>`, unix ms.
 * A kill nobody here saw can be entered by hand.
 */
(() => {
  if (window.A2_VIEW !== "main") return;
  const bridge = window.javaBridge;
  if (!bridge) return;

  const MINUTE = 60_000;
  const HOUR = 60 * MINUTE;
  const SERVER_ZONE = "Europe/Berlin";

  const hm = (text) => {
    const [h, m] = text.split(":").map(Number);
    return h * 60 + m;
  };
  /** Minutes of the day from `first`, every `stepMin`, until midnight. */
  const every = (first, stepMin) => {
    const out = [];
    for (let t = hm(first); t < 1440; t += stepMin) out.push(t);
    return out;
  };

  // days: 0 = Sunday … 6 = Saturday, in the event's zone; omitted = daily.
  const EVENTS = [
    { id: "rift", name: "Spacetime Rift", times: every("02:00", 180), durationMin: 60 },
    { id: "kaira", name: "Watcher Kaira", times: every("01:00", 180), durationMin: 30 },
    { id: "shugo", name: "Shugo Festival", times: every("00:00", 60), durationMin: 10, quiet: true },
    { id: "invasion", name: "Dimensional Invasion", times: every("00:30", 60), durationMin: 15, quiet: true },
    { id: "siege", name: "Artifact Siege", days: [1, 4, 6], times: [hm("21:00")], durationMin: 30 },
    { id: "siegeBosses", name: "Siege Bosses", days: [1, 4, 6], times: [hm("21:30")], durationMin: 30 },
    { id: "nahma", name: "Guardian Lord Nahma", days: [0, 5], times: [hm("21:00")], durationMin: 30 },
    { id: "daily", name: "Daily reset", zone: "UTC", times: [hm("07:00")] },
    { id: "weekly", name: "Weekly reset", zone: "UTC", days: [3], times: [hm("07:00")] },
  ];

  // Bosses with alerts on by default. `code` (NPC code) lets a kill seen in
  // combat start the countdown; `spawnId` ties it to the map's boss list.
  const BOSSES = [
    { id: "boss2400800", code: 2400800, spawnId: 111021, name: "Immortal Gartua", respawnMs: 12 * HOUR },
  ];

  // The map's field boss list names bosses by spawn id only (map id × 100 +
  // n). Names matched against the in-game list; unknown ones show their id.
  const FIELD_BOSS_NAMES = {
    111008: "Blue Wave Kelpina",
    111013: "Specter Archon Axios",
    111015: "Executioner Barthien",
    111016: "Drakan Battalion Weap…",
    111018: "Visionary Karuka",
    111020: "Sharp Shylak",
    111021: "Immortal Gartua",
    111022: "High Commander Lagta",
    111023: "Soul Ruler Kashapa",
    111024: "Silent Dartan",
  };

  const KEY_TRACKED = "timers.trackedCodes";
  const KEY_KILL = "timers.kill.";
  const KEY_MUTED = "timers.muted";
  const KEY_CUBE_ALERT = "timers.cubeAlert"; // read by the backend
  const KEY_CUBES = "timers.cubes"; // { since: [unix ms…], newAt }, set by the backend
  const KEY_OPEN = "timers.panelOpen";
  const KEY_LEAD = "timers.leadMin";
  const KEY_ALERT_BEFORE = "timers.alertBefore";
  const KEY_ALERT_START = "timers.alertAtStart";
  const KEY_FIELD = "timers.fieldBosses";
  const KEY_ALL_BOSSES = "timers.showAllBosses";

  // ---- Time zones -------------------------------------------------------

  const formatters = new Map();
  const zoneParts = (ms, zone) => {
    let fmt = formatters.get(zone);
    if (!fmt) {
      fmt = new Intl.DateTimeFormat("en-US", {
        timeZone: zone, hourCycle: "h23", year: "numeric", month: "numeric",
        day: "numeric", hour: "numeric", minute: "numeric", second: "numeric",
      });
      formatters.set(zone, fmt);
    }
    const p = Object.fromEntries(fmt.formatToParts(ms).map((x) => [x.type, Number(x.value)]));
    return { y: p.year, mo: p.month - 1, d: p.day, h: p.hour, mi: p.minute, s: p.second };
  };
  const zoneOffset = (ms, zone) => {
    const p = zoneParts(ms, zone);
    return Date.UTC(p.y, p.mo, p.d, p.h, p.mi, p.s) - Math.floor(ms / 1000) * 1000;
  };
  /** The instant `minutes` past midnight on y-mo-d, on the clock of `zone`. */
  const zonedInstant = (y, mo, d, minutes, zone) => {
    const asUtc = Date.UTC(y, mo, d, 0, minutes);
    const first = zoneOffset(asUtc, zone);
    const second = zoneOffset(asUtc - first, zone);
    return asUtc - second;
  };

  /** The running or next occurrence of `event`: { start, end }. */
  const nextOccurrence = (event, now) => {
    const zone = event.zone || SERVER_ZONE;
    const today = zoneParts(now, zone);
    const durationMs = (event.durationMin || 0) * MINUTE;
    for (let offset = -1; offset <= 8; offset++) {
      const day = new Date(Date.UTC(today.y, today.mo, today.d + offset));
      if (event.days && !event.days.includes(day.getUTCDay())) continue;
      for (const minutes of event.times) {
        const start = zonedInstant(day.getUTCFullYear(), day.getUTCMonth(), day.getUTCDate(), minutes, zone);
        if (start + durationMs > now) return { start, end: start + durationMs };
      }
    }
    return null;
  };

  // ---- Settings ---------------------------------------------------------

  const listSetting = (key) => (bridge.getSetting(key) || "").split(",").filter(Boolean);
  const isMuted = (item) => {
    const raw = bridge.getSetting(KEY_MUTED);
    if (raw == null) return !!item.quiet; // quiet ones start muted
    return raw.split(",").includes(item.id);
  };
  const setMuted = (item, muted) => {
    const ids = new Set(bridge.getSetting(KEY_MUTED) == null
      ? [...EVENTS, ...bossItems()].filter((x) => x.quiet).map((x) => x.id)
      : listSetting(KEY_MUTED));
    if (muted) ids.add(item.id); else ids.delete(item.id);
    bridge.setSetting(KEY_MUTED, [...ids].join(","));
  };
  const flagSetting = (key) => bridge.getSetting(key) !== "false"; // on by default
  const leadMinutes = () => {
    const minutes = Number(bridge.getSetting(KEY_LEAD));
    return Number.isFinite(minutes) && minutes > 0 ? minutes : 5;
  };
  const leadMs = () => {
    return leadMinutes() * MINUTE;
  };
  const killTime = (boss) => {
    if (!boss.code) return null;
    const ms = Number(bridge.getSetting(KEY_KILL + boss.code));
    return Number.isFinite(ms) && ms > 0 ? ms : null;
  };
  /** `{ spawnId: [alive, unixMs, seenMs] }` as the backend saved it. */
  const fieldBosses = () => {
    try {
      const parsed = JSON.parse(bridge.getSetting(KEY_FIELD) || "{}");
      return parsed && typeof parsed === "object" ? parsed : {};
    } catch {
      return {};
    }
  };

  /** The tracked bosses, then every other field boss the map list has shown. */
  const bossItems = () => {
    const items = [...BOSSES];
    const known = new Set(BOSSES.map((b) => b.spawnId));
    for (const spawnId of Object.keys(fieldBosses()).map(Number).sort((a, b) => a - b)) {
      if (known.has(spawnId)) continue;
      items.push({
        id: `field${spawnId}`,
        spawnId,
        name: FIELD_BOSS_NAMES[spawnId] || `Field boss ${spawnId}`,
        quiet: true,
      });
    }
    return items;
  };

  // ---- Formatting -------------------------------------------------------

  const countdown = (ms) => {
    const total = Math.max(0, Math.round(ms / 1000));
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    if (h >= 24) return `${Math.floor(h / 24)}d ${h % 24}h ${String(m).padStart(2, "0")}m`;
    if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m`;
    if (m >= 10) return `${m}m`;
    return `${m}m ${String(s).padStart(2, "0")}s`;
  };
  const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const weekdayClock = (ms, now) => {
    const sameDay = new Date(ms).toDateString() === new Date(now).toDateString();
    return sameDay ? clock(ms)
      : `${new Date(ms).toLocaleDateString([], { weekday: "short" })} ${clock(ms)}`;
  };

  // ---- Sound ------------------------------------------------------------

  let audio = null;
  const beep = () => {
    try {
      audio ||= new (window.AudioContext || window.webkitAudioContext)();
      if (audio.state === "suspended") audio.resume();
      const t0 = audio.currentTime + 0.02;
      [0, 0.22, 0.44].forEach((at, i) => {
        const osc = audio.createOscillator();
        const gain = audio.createGain();
        osc.type = "sine";
        osc.frequency.value = i === 2 ? 1175 : 880;
        gain.gain.setValueAtTime(0.0001, t0 + at);
        gain.gain.exponentialRampToValueAtTime(0.25, t0 + at + 0.02);
        gain.gain.exponentialRampToValueAtTime(0.0001, t0 + at + 0.18);
        osc.connect(gain).connect(audio.destination);
        osc.start(t0 + at);
        osc.stop(t0 + at + 0.2);
      });
    } catch (error) {
      bridge.logToDebug?.(`Timers: sound failed: ${error}`);
    }
  };

  // ---- DOM --------------------------------------------------------------

  const meter = document.querySelector(".meter");
  const headerBtns = document.querySelector(".headerBtns");
  const list = document.querySelector(".meter .list");
  if (!meter || !headerBtns || !list) return;

  const toggleBtn = document.createElement("div");
  toggleBtn.className = "headerBtn timersBtn";
  toggleBtn.setAttribute("role", "button");
  toggleBtn.tabIndex = 0;
  toggleBtn.setAttribute("aria-label", "Timers");
  toggleBtn.title = "Timers";
  toggleBtn.innerHTML = '<i data-lucide="alarm-clock"></i>';
  headerBtns.insertBefore(toggleBtn, headerBtns.querySelector(".historyBtn") || headerBtns.firstChild);

  const panel = document.createElement("div");
  panel.className = "timersPanel";
  list.after(panel);

  // In the flow under the header, not floating: the window is sized to the
  // meter's content, so a floating banner could be cut off.
  const alerts = document.createElement("div");
  alerts.className = "timerAlerts";
  (meter.querySelector(".header") || list).after(alerts);

  window.lucide?.createIcons?.({ root: toggleBtn });

  const panelOpen = () => bridge.getSetting(KEY_OPEN) === "true";
  const applyOpen = () => {
    const open = panelOpen();
    panel.classList.toggle("isOpen", open);
    toggleBtn.classList.toggle("isActive", open);
  };
  toggleBtn.addEventListener("click", () => {
    bridge.setSetting(KEY_OPEN, String(!panelOpen()));
    applyOpen();
    render(Date.now());
  });

  const showAlert = (text) => {
    const row = document.createElement("div");
    row.className = "timerAlert";
    row.dataset.noDrag = ""; // a click dismisses rather than drags the window
    row.textContent = text;
    row.title = "Dismiss";
    const remove = () => row.remove();
    row.addEventListener("click", remove);
    alerts.prepend(row);
    // Three at most, not counting the Hidden Cube banner, which stays.
    const timed = [...alerts.children].filter((el) => !el.classList.contains("timerCube"));
    timed.slice(3).forEach((el) => el.remove());
    setTimeout(remove, 20_000);
    beep();
  };

  // ---- Rows ---------------------------------------------------------------

  const isBoss = (item) => !!(item.spawnId || item.code);

  const rowTemplate = (item) => `
    <div class="timerRow" data-id="${item.id}">
      <button type="button" class="timerMute" title="Alerts on/off"><i data-lucide="bell"></i></button>
      <span class="timerName"></span>
      <span class="timerWhen"></span>
      <span class="timerLeft"></span>
      ${item.code ? `
      <span class="timerActions">
        <button type="button" class="timerKilled" title="Killed just now"><i data-lucide="skull"></i></button>
        <button type="button" class="timerEdit" title="Set kill time"><i data-lucide="pencil"></i></button>
        <input type="time" class="timerEditInput" data-no-drag />
      </span>` : ""}
    </div>`;

  panel.innerHTML = `
    <div class="timersSection">Bosses</div>
    <div class="timersBosses"></div>
    <div class="timersSection">Events</div>
    ${EVENTS.map(rowTemplate).join("")}
    <div class="timersSection">Alerts</div>
    <div class="timersAlertSettings" data-no-drag>
      <label><input type="checkbox" class="timersAlertBefore" /> Before:</label>
      <input type="number" class="timersLead" min="1" max="120" step="1" /> min
      <label><input type="checkbox" class="timersAlertStart" /> At start</label>
    </div>
    <div class="timersAlertSettings" data-no-drag>
      <label><input type="checkbox" class="timersCubeAlert" /> Hidden Cube nearby</label>
    </div>`;
  window.lucide?.createIcons?.({ root: panel });
  const bossList = panel.querySelector(".timersBosses");

  const alertBeforeBox = panel.querySelector(".timersAlertBefore");
  const alertStartBox = panel.querySelector(".timersAlertStart");
  const leadInput = panel.querySelector(".timersLead");
  const syncAlertSettings = () => {
    alertBeforeBox.checked = flagSetting(KEY_ALERT_BEFORE);
    alertStartBox.checked = flagSetting(KEY_ALERT_START);
    cubeBox.checked = flagSetting(KEY_CUBE_ALERT);
    if (document.activeElement !== leadInput) leadInput.value = String(leadMinutes());
    leadInput.disabled = !alertBeforeBox.checked;
  };
  alertBeforeBox.addEventListener("change", () => {
    bridge.setSetting(KEY_ALERT_BEFORE, String(alertBeforeBox.checked));
    syncAlertSettings();
  });
  alertStartBox.addEventListener("change", () => bridge.setSetting(KEY_ALERT_START, String(alertStartBox.checked)));
  const cubeBox = panel.querySelector(".timersCubeAlert");
  cubeBox.addEventListener("change", () => bridge.setSetting(KEY_CUBE_ALERT, String(cubeBox.checked)));
  leadInput.addEventListener("change", () => {
    const minutes = Math.round(Number(leadInput.value));
    if (minutes >= 1 && minutes <= 120) bridge.setSetting(KEY_LEAD, String(minutes));
    syncAlertSettings();
  });

  // Tracked bosses and those with alerts on are always listed; the rest of the
  // map's field bosses only on request, or the overlay would be two dozen rows.
  const showAllBosses = () => bridge.getSetting(KEY_ALL_BOSSES) === "true";
  let bossListSignature = "";
  const rebuildBosses = () => {
    const all = bossItems();
    const hidden = all.filter((b) => !BOSSES.includes(b) && isMuted(b));
    const shown = showAllBosses() ? all : all.filter((b) => !hidden.includes(b));
    const signature = `${shown.map((b) => b.id).join(",")}|${hidden.length}|${showAllBosses()}`;
    if (signature === bossListSignature) return;
    bossListSignature = signature;
    bossList.innerHTML = shown.map(rowTemplate).join("") + (hidden.length
      ? `<button type="button" class="timersMore">${showAllBosses() ? "Show fewer" : `Show all field bosses (+${hidden.length})`}</button>`
      : "");
    window.lucide?.createIcons?.({ root: bossList });
  };

  const rowOf = (id) => panel.querySelector(`.timerRow[data-id="${id}"]`);
  const itemById = (id) => [...EVENTS, ...bossItems()].find((x) => x.id === id);

  panel.addEventListener("click", (e) => {
    if (e.target.closest(".timersMore")) {
      bridge.setSetting(KEY_ALL_BOSSES, String(!showAllBosses()));
      render(Date.now());
      return;
    }
    const row = e.target.closest(".timerRow");
    const item = row && itemById(row.dataset.id);
    if (!item) return;
    if (e.target.closest(".timerMute")) {
      setMuted(item, !isMuted(item));
    } else if (e.target.closest(".timerKilled")) {
      bridge.setSetting(KEY_KILL + item.code, String(Date.now()));
    } else if (e.target.closest(".timerEdit")) {
      const input = row.querySelector(".timerEditInput");
      row.classList.toggle("isEditing");
      if (row.classList.contains("isEditing")) {
        input.value = new Date().toTimeString().slice(0, 5);
        input.focus();
      }
    }
    render(Date.now());
  });

  // A typed kill time is the latest past moment on the local clock with that
  // time of day: 23:30 entered at 00:15 is last night.
  panel.addEventListener("keydown", (e) => {
    const input = e.target.closest(".timerEditInput");
    if (!input) return;
    const row = input.closest(".timerRow");
    if (e.key === "Escape") {
      row.classList.remove("isEditing");
    } else if (e.key === "Enter" && /^\d{2}:\d{2}$/.test(input.value)) {
      const [h, m] = input.value.split(":").map(Number);
      const when = new Date();
      when.setHours(h, m, 0, 0);
      if (when.getTime() > Date.now()) when.setDate(when.getDate() - 1);
      bridge.setSetting(KEY_KILL + itemById(row.dataset.id).code, String(when.getTime()));
      row.classList.remove("isEditing");
      render(Date.now());
    }
  });

  // ---- State and alerts -----------------------------------------------------

  /**
   * { start, end } for an event; for a boss, start is its respawn (or, when the
   * map list saw it alive, its spawn). Whichever is newer wins: the map list,
   * or a kill seen in combat or entered by hand. Null when nothing is known.
   */
  const stateOf = (item, now) => {
    if (!isBoss(item)) return nextOccurrence(item, now);
    const seen = item.spawnId ? fieldBosses()[item.spawnId] : null;
    const killed = killTime(item);
    if (killed != null && item.respawnMs && (!Array.isArray(seen) || killed > seen[2])) {
      const start = killed + item.respawnMs;
      return { start, end: start };
    }
    if (!Array.isArray(seen)) return null;
    return { start: seen[1], end: seen[1] };
  };

  const fired = new Set();
  const checkAlerts = (now) => {
    const lead = leadMs();
    const before = flagSetting(KEY_ALERT_BEFORE);
    const atStart = flagSetting(KEY_ALERT_START);
    for (const item of [...bossItems(), ...EVENTS]) {
      if (isMuted(item)) continue;
      const state = stateOf(item, now);
      if (!state) continue;
      const boss = isBoss(item);
      const soon = `${item.id}@${state.start}:soon`;
      const now0 = `${item.id}@${state.start}:now`;
      const untilStart = state.start - now;
      if (untilStart > 0 && untilStart <= lead && !fired.has(soon)) {
        fired.add(soon);
        if (before) showAlert(`${item.name} ${boss ? "respawns" : "starts"} in ${Math.round(untilStart / MINUTE)} min`);
      } else if (untilStart <= 0 && untilStart > -MINUTE && !fired.has(now0)) {
        fired.add(now0);
        fired.add(soon);
        if (atStart) showAlert(boss ? `${item.name} is up` : `${item.name} has started`);
      }
    }
    if (fired.size > 500) fired.clear();
    renderCubes(now);
  };

  // ---- Hidden Cubes ----------------------------------------------------------

  // A banner that stays while a Hidden Cube is in range: the backend drops a
  // cube when it is looted or out of range. A click hides it until a cube
  // comes (back) into range. It beeps only for a cube never seen before.
  const cubeRow = document.createElement("div");
  cubeRow.className = "timerAlert timerCube";
  cubeRow.dataset.noDrag = "";
  cubeRow.title = "Dismiss";
  let cubesDismissedAt = 0;
  let cubeBeepedAt = null;
  cubeRow.addEventListener("click", () => {
    cubesDismissedAt = Date.now();
    cubeRow.remove();
  });
  const cubesState = () => {
    try {
      const state = JSON.parse(bridge.getSetting(KEY_CUBES) || "{}");
      return {
        since: Array.isArray(state.since) ? state.since : [],
        dist: Array.isArray(state.dist) ? state.dist : [],
        posAt: Number(state.posAt) || 0,
        newAt: Number(state.newAt) || 0,
      };
    } catch (_) {
      return { since: [], dist: [], posAt: 0, newAt: 0 };
    }
  };
  const renderCubes = (now) => {
    const { since, dist, posAt, newAt } = cubesState();
    // What was already known when the meter started is not news.
    if (cubeBeepedAt == null) cubeBeepedAt = newAt;
    const show = since.some((t) => t > cubesDismissedAt);
    if (!show) {
      cubeRow.remove();
      return;
    }
    const count = since.length > 1 ? `${since.length} Hidden Cubes nearby` : "Hidden Cube nearby";
    // Distance from where the server last said you were: every few seconds
    // while you move, so say how old it is once that matters.
    const known = dist.filter((d) => typeof d === "number");
    let away = "";
    if (known.length) {
      away = ` · ${since.length > 1 ? "nearest " : ""}~${Math.min(...known)} m`;
      if (posAt && now - posAt > 15_000) away += ` (as of ${countdown(now - posAt)} ago)`;
    }
    cubeRow.textContent = `${count}${away} · in range for ${countdown(now - Math.min(...since))}`;
    if (!cubeRow.isConnected) alerts.prepend(cubeRow);
    if (newAt > cubeBeepedAt) {
      cubeBeepedAt = newAt;
      if (now - newAt < MINUTE) beep();
    }
  };

  const render = (now) => {
    applyOpen();
    if (!panelOpen()) return;
    rebuildBosses();
    syncAlertSettings();
    for (const item of [...bossItems(), ...EVENTS]) {
      const row = rowOf(item.id);
      if (!row) continue;
      const state = stateOf(item, now);
      row.classList.toggle("isMuted", isMuted(item));
      row.querySelector(".timerName").textContent = item.name;
      const when = row.querySelector(".timerWhen");
      const left = row.querySelector(".timerLeft");
      let live = false;
      if (!state) {
        when.textContent = "time unknown";
        left.textContent = "";
      } else if (isBoss(item)) {
        live = now >= state.start;
        when.textContent = live ? `up since ${weekdayClock(state.start, now)}` : `at ${weekdayClock(state.start, now)}`;
        left.textContent = live ? "UP" : countdown(state.start - now);
      } else {
        live = now >= state.start;
        when.textContent = weekdayClock(state.start, now);
        left.textContent = live ? `LIVE ${countdown(state.end - now)}` : countdown(state.start - now);
      }
      row.classList.toggle("isLive", live);
      row.classList.toggle("isSoon", !live && !!state && state.start - now <= Math.max(leadMs(), 5 * MINUTE));
    }
  };

  // The backend tracks kills only of the bosses listed here.
  bridge.setSetting(KEY_TRACKED, BOSSES.filter((b) => b.code).map((b) => b.code).join(","));

  const tick = () => {
    const now = Date.now();
    checkAlerts(now);
    render(now);
  };
  // Alerts for what already began before the meter started would be noise.
  (window.a2SettingsReady || Promise.resolve()).then(() => {
    const now = Date.now();
    for (const item of [...bossItems(), ...EVENTS]) {
      const state = stateOf(item, now);
      if (state && state.start <= now) fired.add(`${item.id}@${state.start}:now`);
    }
    tick();
    setInterval(tick, 1000);
  });
})();
