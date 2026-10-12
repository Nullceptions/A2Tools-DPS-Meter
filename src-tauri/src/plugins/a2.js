// The plugin API (apiVersion 1), loaded by a plugin's page from /_sdk/a2.js.
// A plugin runs in a sandboxed frame: everything goes to the host page as a
// message, and the host passes on only what the manifest's permissions allow.
//
//   a2.on("decoded:pet-list", (data, { character }) => ...)
//                                              one of the plugin's decoders. Each event arrives once,
//                                              in order, even those from while the window was closed
//                                              (since the meter started); `character` is who was
//                                              playing (with events:character), else null
//   a2.on("character", (c) => ...)             your character (name, serverId, class, level)
//   a2.on("language", (code) => ...)           the meter's language ("en", "ko", "zh-Hans", ...), at start and on change
//   a2.on("compact", (on) => ...)              the window's fold button: show only the essentials
//
// Keep the page's background transparent: the window paints the panel at the
// opacity the user picks.
//   await a2.storage.get("state")              the plugin's own saved data
//   await a2.storage.set("state", value)
//   a2.notify("Text")                          a short message in the plugin's window
//   a2.window.show() / a2.window.hide()        its own window, shown on top without taking focus
//                                              (permission "window", plugin API 2)
//   a2.ready()                                 call once the handlers are set
(() => {
  const handlers = {};
  const pending = new Map();
  let seq = 0;
  const call = (name, args) =>
    new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      window.parent.postMessage({ a2: 1, id, call: name, args }, "*");
    });
  window.addEventListener("message", (event) => {
    if (event.source !== window.parent) return;
    const msg = event.data;
    if (!msg || msg.a2 !== 1) return;
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) reject(new Error(msg.error));
      else resolve(msg.result);
      return;
    }
    if (msg.event) {
      for (const fn of handlers[msg.event] || []) {
        try {
          fn(msg.data, { character: msg.character ?? null });
        } catch (e) {
          console.error(e);
        }
      }
    }
  });
  window.a2 = Object.freeze({
    on(event, fn) {
      (handlers[event] ||= []).push(fn);
    },
    storage: Object.freeze({
      get: (key) => call("storage.get", [key]).then((v) => (v == null ? null : JSON.parse(v))),
      set: (key, value) => call("storage.set", [key, JSON.stringify(value)]),
    }),
    notify: (text) => call("notify", [String(text)]),
    window: Object.freeze({
      show: () => call("window.show", []),
      hide: () => call("window.hide", []),
    }),
    ready: () => call("ready", []),
  });
})();
