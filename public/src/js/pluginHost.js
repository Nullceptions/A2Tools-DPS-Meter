// A plugin window's host page (src-tauri/src/plugins). The plugin runs in a
// sandboxed frame with an origin of its own and no access to this page or the
// meter; it asks for things by message, and this page passes on only what the
// plugin's manifest permits.
(() => {
  const { invoke, convertFileSrc } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const id = window.__A2_PLUGIN_ID__;
  const stage = document.querySelector(".pluginStage");
  const toast = document.querySelector(".pluginToast");
  let frame = null;
  let manifest = null;
  let ready = false;
  const queue = [];
  // A pop-up (hidden until the plugin shows it, sized to what it shows) or a
  // panel the user opened from Settings. The meter says which.
  let popup = false;

  const allowed = (permission) => !!manifest?.permissions?.includes(permission);
  const send = (msg) => frame?.contentWindow?.postMessage({ a2: 1, ...msg }, "*");

  // Decoded events are numbered for the meter's run. The page remembers the
  // last one it handed the plugin, so a window opened again (or reloaded)
  // gets each event once: the ones it missed while closed, none twice.
  const CURSOR_KEY = `a2plugin.cursor.${id}`;
  let run = null;
  let delivered = 0;
  const saveCursor = () => {
    try { localStorage.setItem(CURSOR_KEY, JSON.stringify({ run, seq: delivered })); } catch {}
  };

  const deliver = (event, data, extra = {}) => {
    if (event.startsWith("decoded:") && !allowed(`decoder:${event.slice(8)}`)) return;
    if (event === "character" && !allowed("events:character")) return;
    const msg = { event, data, ...extra };
    if (ready) send(msg);
    else queue.push(msg);
  };
  // A numbered event from the meter: skipped if already handed over.
  const deliverLogged = (e) => {
    if (typeof e?.seq !== "number" || e.seq <= delivered) return;
    delivered = e.seq;
    deliver(e.event, e.data, { character: e.character ?? null });
    if (ready) saveCursor();
  };
  // Events that arrive before the page knows the run and the manifest.
  let early = [];

  let toastTimer = 0;
  const showToast = (text) => {
    toast.textContent = text.slice(0, 200);
    toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.hidden = true; }, 4000);
  };

  const calls = {
    ready: () => {
      ready = true;
      for (const m of queue.splice(0)) send(m);
      saveCursor();
      return true;
    },
    "storage.get": ([key]) => invoke("plugin_storage_get", { key: String(key) }),
    "storage.set": ([key, value]) => invoke("plugin_storage_set", { key: String(key), value: String(value) }),
    notify: ([text]) => {
      if (!allowed("notify")) throw new Error('the plugin lacks the "notify" permission');
      if (!popup) showToast(String(text));
      return true;
    },
    // Pop-up only; a panel stays as the user left it (the calls answer false).
    show: () => popupCall({ show: true }),
    hide: () => popupCall({ show: false }),
    fit: ([height]) => {
      const content = Number(height);
      if (!Number.isFinite(content) || content <= 0) throw new Error("fit needs the content's height in pixels");
      const chrome = document.querySelector(".pluginBar").offsetHeight + 2;
      return popupCall({ height: Math.ceil(content) + chrome });
    },
  };
  const popupCall = (args) => {
    if (!allowed("popup")) throw new Error('the plugin lacks the "popup" permission');
    if (!popup) return false;
    return invoke("plugin_popup", args);
  };
  const setMode = (isPopup) => {
    popup = isPopup;
    document.body.classList.toggle("popup", popup);
    if (popup) toast.hidden = true;
    deliver("mode", popup ? "popup" : "panel");
  };
  listen("plugin-mode", (event) => setMode(event.payload === "popup"));

  window.addEventListener("message", async (event) => {
    if (!frame || event.source !== frame.contentWindow) return;
    const msg = event.data;
    if (!msg || msg.a2 !== 1 || !msg.id || typeof msg.call !== "string") return;
    const fn = calls[msg.call];
    try {
      if (!fn) throw new Error(`unknown call ${msg.call}`);
      send({ id: msg.id, result: await fn(Array.isArray(msg.args) ? msg.args : []) });
    } catch (e) {
      send({ id: msg.id, error: String(e?.message || e) });
    }
  });

  // The window is frameless: its bar moves it, and the button closes it.
  document.querySelector(".pluginBar").addEventListener("mousedown", (e) => {
    if (e.button !== 0 || e.target.closest("button, input")) return;
    e.preventDefault();
    invoke("start_tool_drag").catch(() => {});
  });
  // A pop-up's close button dismisses what it shows (the plugin hears
  // "dismissed" and pops up again for something new); a panel closes, or goes
  // back to being its plugin's pop-up.
  document.querySelector(".pluginClose").addEventListener("click", () => {
    if (popup) {
      invoke("plugin_popup", { show: false }).catch(() => {});
      deliver("dismissed", true);
    } else {
      invoke("plugin_close").catch(() => invoke("close_tool_window").catch(() => {}));
    }
  });

  // Panel preferences, per plugin, kept by this page: background opacity, and
  // whether the plugin's controls are folded away ("compact", which the plugin
  // hears as an event and handles itself).
  const PREFS_KEY = `a2plugin.panel.${id}`;
  let panel = { opacity: 100, compact: false };
  try { panel = { ...panel, ...JSON.parse(localStorage.getItem(PREFS_KEY) || "{}") }; } catch {}
  const savePanel = () => { try { localStorage.setItem(PREFS_KEY, JSON.stringify(panel)); } catch {} };
  const opacity = document.querySelector(".pluginOpacity");
  const compact = document.querySelector(".pluginCompact");
  const applyOpacity = () => document.body.style.setProperty("--panel-alpha", String(panel.opacity / 100));
  const applyCompact = () => {
    compact.setAttribute("aria-pressed", String(panel.compact));
    const label = panel.compact ? "Show the controls" : "Hide the controls";
    compact.setAttribute("aria-label", label);
    compact.title = label;
  };
  opacity.value = String(panel.opacity);
  applyOpacity();
  applyCompact();
  opacity.addEventListener("input", () => {
    panel.opacity = Number(opacity.value) || 100;
    applyOpacity();
    savePanel();
  });
  compact.addEventListener("click", () => {
    panel.compact = !panel.compact;
    applyCompact();
    savePanel();
    deliver("compact", panel.compact);
  });

  listen("plugin-event", (event) => {
    const payload = event.payload || {};
    if (typeof payload.event !== "string") return;
    if (typeof payload.seq !== "number") deliver(payload.event, payload.data);
    else if (early) early.push(payload);
    else deliverLogged(payload);
  });

  invoke("plugin_hello").then((hello) => {
    manifest = hello.manifest;
    document.title = manifest.name;
    document.querySelector(".pluginName").textContent = `${manifest.name} ${manifest.version}`;
    // Reviewed plugins come from the signed catalog; anything else is a developer folder.
    document.querySelector(".pluginBadge").hidden = !!hello.reviewed;
    deliver("language", hello.language || "en");
    setMode(!!hello.popup);
    // On Windows a pop-up starts shown (so that it loads); hide it until the
    // plugin has something to show.
    if (popup) invoke("plugin_popup", { show: false }).catch(() => {});
    deliver("compact", panel.compact);
    if (hello.character) deliver("character", hello.character);
    run = hello.run;
    try {
      const cursor = JSON.parse(localStorage.getItem(CURSOR_KEY) || "null");
      if (cursor?.run === run) delivered = Number(cursor.seq) || 0;
    } catch {}
    const logged = [...(hello.replay || []), ...early].sort((a, b) => a.seq - b.seq);
    early = null;
    for (const e of logged) deliverLogged(e);
    frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.setAttribute("referrerpolicy", "no-referrer");
    // The scheme's base (http://a2plugin.localhost/ on Windows), then a plain
    // path: convertFileSrc would encode its "/", and the page's relative
    // links (style.css, plugin.js) would then resolve outside its folder.
    const base = convertFileSrc("", "a2plugin").replace(/\/*$/, "/");
    frame.src = `${base}${encodeURIComponent(id)}/index.html`;
    stage.appendChild(frame);
  }).catch((e) => {
    stage.textContent = `This plugin could not start: ${e}`;
  });
})();
