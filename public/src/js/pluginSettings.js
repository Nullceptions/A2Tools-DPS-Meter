// Settings → Plugins: the installed plugins (Open, Update, Remove), the
// reviewed plugins on offer from the signed catalog (Install), checking for
// updates, and developer mode with its local folders (src-tauri/src/plugins).
(() => {
  if (window.__A2_VIEW__ !== "settings") return;
  const invoke = window.__TAURI__?.core?.invoke;
  if (!invoke) return;
  const t = (key, fallback, vars = {}) => {
    let text = window.i18n?.t?.(key, fallback) ?? fallback;
    for (const [name, value] of Object.entries(vars)) text = text.replace(`{${name}}`, value);
    return text;
  };

  const start = () => {
    const toggle = document.querySelector(".pluginDevModeCheckbox");
    const devRow = document.querySelector(".pluginDevRow");
    const list = document.querySelector(".pluginList");
    const empty = document.querySelector(".pluginListEmpty");
    const catalogList = document.querySelector(".pluginCatalog");
    const catalogStatus = document.querySelector(".pluginCatalogStatus");
    const add = document.querySelector(".pluginAddBtn");
    const update = document.querySelector(".pluginUpdateBtn");
    const status = document.querySelector(".pluginStatus");
    const consent = document.querySelector(".pluginConsent");
    if (!toggle || !devRow || !list || !empty || !catalogList || !add || !update || !status || !consent) return;

    // The status line shows errors in red and other news in the hint colour.
    const say = (text, isError = true) => {
      status.textContent = text;
      status.classList.toggle("pluginError", isError && !!text);
    };

    let view = { devMode: false, online: true, plugins: [] };
    let catalog = null; // the catalog's entries, once fetched
    // Plugins the last update changed, by id → the version before; marked
    // green until Settings closes.
    const updatedFrom = new Map();
    // Newer reviewed versions, by id → { to, newPermissions }.
    const available = new Map();

    // A permission, in plain words.
    const describe = (permission) => {
      if (permission === "storage") return t("settings.plugins.permStorage", "Save its own data on this computer");
      if (permission === "notify") return t("settings.plugins.permNotify", "Show short messages in its window");
      if (permission === "window") {
        return t("settings.plugins.permWindow", "Show and hide its own window, without taking focus from the game");
      }
      if (permission === "events:character") {
        return t("settings.plugins.permCharacter", "See your character's name, server, class and level");
      }
      if (permission.startsWith("decoder:")) {
        return t("settings.plugins.permDecoder", "Read your own game data: {name}", { name: permission.slice(8) });
      }
      return permission;
    };

    // Ask before installing, listing what the plugin may do. Resolves true to go ahead.
    const ask = ({ title, intro, permissions, confirm }) =>
      new Promise((resolve) => {
        consent.querySelector(".hotkeyConsentTitle").textContent = title;
        consent.querySelector(".pluginConsentIntro").textContent = intro;
        const items = permissions.length
          ? permissions.map(describe)
          : [t("settings.plugins.permNone", "Nothing beyond showing its window")];
        consent.querySelector(".pluginConsentList").replaceChildren(
          ...items.map((text) => Object.assign(document.createElement("li"), { textContent: text })),
        );
        const yes = consent.querySelector(".pluginConsentConfirm");
        const no = consent.querySelector(".pluginConsentCancel");
        yes.textContent = confirm;
        const done = (answer) => {
          consent.hidden = true;
          yes.removeEventListener("click", onYes);
          no.removeEventListener("click", onNo);
          consent.removeEventListener("keydown", onKey);
          resolve(answer);
        };
        const onYes = () => done(true);
        const onNo = () => done(false);
        const onKey = (e) => { if (e.key === "Escape") done(false); };
        yes.addEventListener("click", onYes);
        no.addEventListener("click", onNo);
        consent.addEventListener("keydown", onKey);
        consent.hidden = false;
        yes.focus();
      });

    const button = (label, onClick, extra = "") => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = `settingsAction ${extra}`.trim();
      b.textContent = label;
      b.addEventListener("click", onClick);
      return b;
    };

    // A list row: a title (text and elements), a line under it, and buttons.
    const pluginRow = (title, subtitle, isError, buttons) => {
      const item = document.createElement("div");
      item.className = "settingsRow pluginItem";
      const info = document.createElement("div");
      info.className = "settingsInfo";
      const name = document.createElement("div");
      name.className = "settingsLabel";
      name.append(...title);
      const sub = document.createElement("div");
      sub.className = isError ? "settingsHint pluginError" : "settingsHint";
      sub.textContent = subtitle;
      info.append(name, sub);
      const end = document.createElement("div");
      end.className = "settingsRowEnd";
      end.append(...buttons);
      item.append(info, end);
      return item;
    };

    const run = (promise) => promise.then(render).catch((e) => say(String(e)));

    const renderCatalog = () => {
      catalogList.replaceChildren();
      catalogStatus.classList.remove("pluginError");
      if (!view.online) {
        catalogStatus.textContent = t("settings.plugins.offline", "The offline build does not download plugins.");
        return;
      }
      if (!catalog) return;
      const offered = catalog.filter((e) => !e.installed);
      for (const entry of offered) {
        const by = entry.author ? t("settings.plugins.by", "by {author}", { author: entry.author }) : "";
        catalogList.append(
          pluginRow([`${entry.name} ${entry.version}`], [entry.description, by].filter(Boolean).join(" · "), false, [
            button(t("settings.plugins.install", "Install"), () => install(entry, null)),
          ]),
        );
      }
      catalogStatus.textContent = offered.length ? "" : t("settings.plugins.catalogEmpty", "Every reviewed plugin is installed.");
    };

    const loadCatalog = async () => {
      if (!view.online) return renderCatalog();
      if (!catalog) catalogStatus.textContent = t("settings.plugins.catalogLoading", "Loading the plugin list…");
      try {
        catalog = await invoke("plugin_catalog");
      } catch (e) {
        catalog = null;
        catalogList.replaceChildren();
        catalogStatus.textContent = t("settings.plugins.catalogError", "Couldn't load the plugin list: {error}", { error: String(e) });
        catalogStatus.classList.add("pluginError");
        return;
      }
      renderCatalog();
    };

    // Install a catalog entry, or update to it (`from` is the installed
    // version). Asks first, listing the permissions: all of them for a new
    // plugin, only the new ones for an update.
    const install = async (entry, from) => {
      const isUpdate = !!from;
      const permissions = isUpdate ? available.get(entry.id)?.newPermissions || [] : entry.permissions || [];
      if (!isUpdate || permissions.length) {
        const ok = await ask({
          title: isUpdate
            ? t("settings.plugins.consentUpdateTitle", "Update {name}?", { name: entry.name })
            : t("settings.plugins.consentTitle", "Install {name}?", { name: entry.name }),
          intro: isUpdate
            ? t("settings.plugins.consentNew", "The new version also asks to:")
            : t("settings.plugins.consentIntro", "It will be able to:"),
          permissions,
          confirm: isUpdate ? t("settings.plugins.update", "Update") : t("settings.plugins.install", "Install"),
        });
        if (!ok) return;
      }
      say(t("settings.plugins.installing", "Installing…"), false);
      try {
        const next = await invoke("plugin_install", { id: entry.id });
        if (isUpdate) updatedFrom.set(entry.id, from);
        available.delete(entry.id);
        render(next);
        say(t("settings.plugins.installDone", "{name} {version} is installed.", { name: entry.name, version: entry.version }), false);
        await loadCatalog();
      } catch (e) {
        say(String(e));
      }
    };

    const render = (next) => {
      if (next) view = next;
      toggle.checked = !!view.devMode;
      devRow.hidden = !view.devMode;
      list.replaceChildren();
      for (const p of view.plugins || []) {
        const title = [p.error ? p.name : `${p.name} ${p.version}`];
        const wasUpdated = !p.error && updatedFrom.has(p.id);
        if (wasUpdated) {
          const from = updatedFrom.get(p.id);
          const badge = document.createElement("i");
          badge.className = "pluginUpdatedIcon";
          badge.setAttribute("data-lucide", "circle-arrow-up");
          badge.setAttribute("role", "img");
          const label = from
            ? t("settings.plugins.updatedFrom", "Updated from {version}", { version: from })
            : t("settings.plugins.added", "Newly loaded");
          badge.setAttribute("aria-label", label);
          badge.title = label;
          title.push(" ", badge);
        }
        if (!p.reviewed) {
          const tag = document.createElement("span");
          tag.className = "pluginUnreviewedTag";
          tag.textContent = t("settings.plugins.unreviewed", "Unreviewed");
          title.push(" ", tag);
        }
        const buttons = [];
        if (!p.error) {
          buttons.push(button(t("settings.plugins.open", "Open"), () => invoke("plugin_open", { id: p.id }).catch((e) => say(String(e)))));
        }
        const newer = p.reviewed && available.get(p.id);
        if (newer) {
          buttons.push(button(
            t("settings.plugins.updateTo", "Update to {version}", { version: newer.to }),
            () => install({ id: p.id, name: p.name, version: newer.to }, p.version),
            "hotkeyConsentConfirm",
          ));
        }
        buttons.push(button(t("settings.plugins.remove", "Remove"), () => {
          say("");
          if (p.reviewed) run(invoke("plugin_uninstall", { id: p.id })).then(loadCatalog);
          else run(invoke("plugin_remove_folder", { path: p.path }));
        }));
        const item = pluginRow(title, p.error || p.description || p.path, !!p.error, buttons);
        if (wasUpdated) item.classList.add("pluginUpdated");
        list.append(item);
      }
      empty.hidden = list.childElementCount > 0;
      window.lucide?.createIcons?.({ root: list });
    };

    // The list comes with whether developer mode is on.
    invoke("plugin_list").then((v) => { render(v); loadCatalog(); }).catch(() => {});

    toggle.addEventListener("change", () => {
      say("");
      run(invoke("plugin_set_dev_mode", { enabled: toggle.checked }));
    });
    update.addEventListener("click", () => {
      update.disabled = true;
      say(t("settings.plugins.checking", "Checking…"), false);
      invoke("plugin_check_updates")
        .then(({ view: next, updated, available: newer, catalogError }) => {
          for (const u of updated) updatedFrom.set(u.id, u.from || "");
          available.clear();
          for (const a of newer) available.set(a.id, a);
          render(next);
          const lines = [];
          if (updated.length) {
            const changes = updated.map((u) => (u.from ? `${u.name} ${u.from} → ${u.to}` : `${u.name} ${u.to}`)).join(", ");
            lines.push(t("settings.plugins.updated", "Updated: {list}", { list: changes }));
          }
          if (newer.length) {
            const versions = newer.map((a) => `${a.name} ${a.to}`).join(", ");
            lines.push(t("settings.plugins.updatesAvailable", "Updates available: {list}", { list: versions }));
          }
          if (catalogError) {
            lines.push(t("settings.plugins.catalogError", "Couldn't load the plugin list: {error}", { error: catalogError }));
          }
          if (!lines.length) lines.push(t("settings.plugins.upToDate", "Every plugin is up to date."));
          say(lines.join(" "), !!catalogError && !updated.length && !newer.length);
          return loadCatalog();
        })
        .catch((e) => say(String(e)))
        .finally(() => { update.disabled = false; });
    });
    add.addEventListener("click", () => {
      say("");
      run(invoke("plugin_add_folder"));
    });
  };

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
})();
