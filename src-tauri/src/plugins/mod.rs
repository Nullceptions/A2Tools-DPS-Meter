//! Plugins: features beside the meter, kept out of its core (docs/plugins-design.md).
//!
//! A plugin is a folder: `manifest.json`, an `index.html` and its scripts and
//! data. It runs in a window of its own (`plugin-<id>`), inside a sandboxed
//! frame with an origin of its own (the `a2plugin` scheme served here), no
//! network and no access to the meter: it talks only to the host page, which
//! passes on what its manifest's permissions allow. Game data reaches it
//! through its own decoders (`decoder`), as numbers.
//!
//! Two kinds load: reviewed plugins, installed from the signed catalog
//! (`catalog`) into `app_data/plugins/<id>/`, always; and in developer mode,
//! plugins from local folders, for authors to build and test them.

#[cfg(feature = "online")]
pub mod catalog;
pub mod decoder;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::Digest;
use serde_json::Value;
use tauri::{Emitter, Manager};

use crate::app::AppState;
use decoder::{Decoder, DecoderSpec};

/// Developer mode on or off.
pub const DEV_MODE_KEY: &str = "dpsMeter.pluginDevMode";
/// The folders loaded in developer mode, a JSON array of paths.
pub const DEV_FOLDERS_KEY: &str = "dpsMeter.devPlugins";
/// The scheme plugin files are served from (`http://a2plugin.localhost/` on
/// Windows, `a2plugin://localhost/` elsewhere).
pub const SCHEME: &str = "a2plugin";
/// The newest plugin API this meter speaks; it runs older ones too.
/// 2 added decoders' `self` field and `a2.window` (the `window` permission).
const API_VERSION: u32 = 2;
/// Which plugin windows were open, and whether hidden, when the meter last
/// ran: a JSON object of id → "shown" | "hidden". They open again at start, so
/// a plugin that watches in the background (hidden) keeps doing so.
pub const WINDOWS_KEY: &str = "dpsMeter.pluginWindows";
/// Largest value one storage key may hold.
const MAX_STORED: usize = 1024 * 1024;
/// How long a decoded event waits before it is stamped with the character and
/// sent. At login the game sends some data (the pet list) a few records
/// before the one that says which character is playing; the wait lets that
/// record land first, so the data is filed under the right character.
const SETTLE: Duration = Duration::from_millis(500);
/// Events kept per plugin for the run, replayed to its window when it opens.
const MAX_LOGGED: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing)]
    pub decoders: Vec<DecoderSpec>,
    #[serde(default)]
    pub ui: Option<Value>,
}

struct Loaded {
    manifest: Manifest,
    dir: PathBuf,
    decoders: Vec<Decoder>,
    /// Installed from the signed catalog, not a developer folder.
    reviewed: bool,
}

/// What Settings lists.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub path: String,
    pub permissions: Vec<String>,
    /// Installed from the signed catalog (otherwise a developer folder).
    pub reviewed: bool,
    /// Set when the folder could not be loaded.
    pub error: Option<String>,
}

/// What Settings shows: developer mode, and each folder's plugin (or error).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsView {
    pub dev_mode: bool,
    /// Whether this build can reach the catalog (not the offline build).
    pub online: bool,
    pub plugins: Vec<PluginInfo>,
}

fn view(app: &tauri::AppHandle, host: &PluginHost) -> PluginsView {
    let dev_mode = app
        .try_state::<AppState>()
        .is_some_and(|s| s.settings.get(DEV_MODE_KEY).as_deref() == Some("true"));
    PluginsView { dev_mode, online: cfg!(feature = "online"), plugins: host.list() }
}

/// Where reviewed plugins are installed.
fn plugins_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let state = app.try_state::<AppState>().ok_or("no app state")?;
    Ok(state.app_data_dir.join("plugins"))
}

/// A decoded event waiting out `SETTLE`.
struct Pending {
    at: Instant,
    plugin: String,
    event: String,
    data: Value,
}

/// Decoded events reach a plugin exactly once and in order, whether or not
/// its window is open: each is numbered and kept for the run (`log`), the
/// window's host page replays the ones it has not delivered yet, and a run id
/// tells it when the numbering started over (the meter restarted).
pub struct PluginHost {
    loaded: RwLock<Vec<Loaded>>,
    errors: RwLock<Vec<PluginInfo>>,
    pending: Mutex<Vec<Pending>>,
    log: Mutex<HashMap<String, VecDeque<Value>>>,
    seq: Mutex<u64>,
    run: String,
    flusher: AtomicBool,
}

impl Default for PluginHost {
    fn default() -> Self {
        let run = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_default();
        PluginHost {
            loaded: RwLock::default(),
            errors: RwLock::default(),
            pending: Mutex::default(),
            log: Mutex::default(),
            seq: Mutex::new(0),
            run,
            flusher: AtomicBool::new(false),
        }
    }
}

/// Who is playing, as plugins with `events:character` see it.
fn character(app: &tauri::AppHandle) -> Option<Value> {
    app.try_state::<AppState>().map(|s| {
        let p = s.data_storage.local_profile();
        serde_json::json!({
            "name": p.name,
            "serverId": p.server_id,
            "level": p.level,
            "class": p.class.map(|c| c.class_name().to_string()),
        })
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Read and check a plugin folder's manifest.
fn load_folder(dir: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).map_err(|e| format!("no manifest.json: {e}"))?;
    let manifest: Manifest = serde_json::from_str(&text).map_err(|e| format!("manifest.json: {e}"))?;
    if !valid_id(&manifest.id) {
        return Err(format!("id {:?}: lowercase letters, digits and '-' only", manifest.id));
    }
    if manifest.api_version == 0 || manifest.api_version > API_VERSION {
        return Err(format!("apiVersion {} (this meter speaks up to {API_VERSION}): update the meter", manifest.api_version));
    }
    if !dir.join("index.html").is_file() {
        return Err("no index.html".into());
    }
    let mut decoders = Vec::new();
    for spec in &manifest.decoders {
        let decoder = Decoder::compile(spec)?;
        if !manifest.permissions.iter().any(|p| p == &format!("decoder:{}", decoder.name)) {
            return Err(format!("decoder {} needs the permission \"decoder:{}\"", decoder.name, decoder.name));
        }
        decoders.push(decoder);
    }
    Ok(Loaded { manifest, dir: dir.to_path_buf(), decoders, reviewed: false })
}

impl PluginHost {
    /// Load the installed plugins, and in developer mode the folders the
    /// settings list. A developer folder wins over an installed copy of the
    /// same plugin, so its author tests their own files.
    pub fn reload(self: &Arc<Self>, app: &tauri::AppHandle) {
        let Some(state) = app.try_state::<AppState>() else { return };
        let dev = state.settings.get(DEV_MODE_KEY).as_deref() == Some("true");
        let folders: Vec<String> = if dev {
            state.settings.get(DEV_FOLDERS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        for folder in folders {
            match load_folder(Path::new(&folder)) {
                Ok(l) if loaded.iter().any(|x: &Loaded| x.manifest.id == l.manifest.id) => errors.push(PluginInfo {
                    error: Some(format!("another folder already loads {:?}", l.manifest.id)),
                    ..info(&l)
                }),
                Ok(l) => loaded.push(l),
                Err(e) => errors.push(PluginInfo {
                    id: String::new(),
                    name: folder.clone(),
                    version: String::new(),
                    description: String::new(),
                    path: folder,
                    permissions: Vec::new(),
                    reviewed: false,
                    error: Some(e),
                }),
            }
        }
        let installed = plugins_dir(app)
            .ok()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.file_name().is_some_and(|n| !n.to_string_lossy().starts_with('.')));
        for dir in installed {
            match load_folder(&dir) {
                Ok(l) if loaded.iter().any(|x: &Loaded| x.manifest.id == l.manifest.id) => {
                    tracing::info!("Plugins: {} runs from its developer folder, not the installed copy", l.manifest.id);
                }
                Ok(l) => loaded.push(Loaded { reviewed: true, ..l }),
                Err(e) => tracing::warn!("Plugins: installed {} does not load: {e}", dir.display()),
            }
        }
        let any_decoders = loaded.iter().any(|l| !l.decoders.is_empty());
        tracing::info!("Plugins: {} loaded (developer mode {})", loaded.len(), if dev { "on" } else { "off" });
        *self.loaded.write().unwrap_or_else(|p| p.into_inner()) = loaded;
        *self.errors.write().unwrap_or_else(|p| p.into_inner()) = errors;

        if !self.flusher.swap(true, Ordering::SeqCst) {
            let host = Arc::clone(self);
            let app = app.clone();
            std::thread::Builder::new()
                .name("plugin-events".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(150));
                    host.flush(&app);
                })
                .ok();
        }

        // The tap is set only while some plugin has a decoder.
        if any_decoders {
            let host = Arc::clone(self);
            let app = app.clone();
            crate::capture::frame_tap::set(Some(Arc::new(move |packet: &[u8]| host.on_frame(&app, packet))));
        } else {
            crate::capture::frame_tap::set(None);
        }
    }

    fn on_frame(&self, app: &tauri::AppHandle, packet: &[u8]) {
        let len = crate::capture::stream_processor::read_varint(packet, 0).length;
        if len <= 0 {
            return;
        }
        let at = len as usize;
        let Some(opcode) = packet.get(at..at + 2) else { return };
        let body = &packet[at + 2..];
        let loaded = self.loaded.read().unwrap_or_else(|p| p.into_inner());
        for plugin in loaded.iter() {
            for d in plugin.decoders.iter().filter(|d| d.opcode == opcode) {
                let Some(data) = d.decode(body) else { continue };
                // Only the player's own doings, for a decoder that asks so.
                if let Some(field) = &d.self_field {
                    let me = app.try_state::<AppState>().and_then(|s| s.data_storage.local_player_id());
                    let theirs = data.get(field).and_then(Value::as_i64);
                    if me.is_none() || me != theirs {
                        continue;
                    }
                }
                self.pending.lock().unwrap_or_else(|p| p.into_inner()).push(Pending {
                    at: Instant::now(),
                    plugin: plugin.manifest.id.clone(),
                    event: format!("decoded:{}", d.name),
                    data,
                });
            }
        }
    }

    /// Send the events that have waited out `SETTLE`: number them, stamp them
    /// with the character (for plugins allowed to know it), keep them in the
    /// run's log, and pass them to the plugin's window if it is open.
    fn flush(&self, app: &tauri::AppHandle) {
        let ready: Vec<Pending> = {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.is_empty() {
                return;
            }
            let now = Instant::now();
            let split = pending.iter().position(|p| now.duration_since(p.at) < SETTLE).unwrap_or(pending.len());
            pending.drain(..split).collect()
        };
        if ready.is_empty() {
            return;
        }
        let who = character(app);
        for item in ready {
            let allowed = self
                .with(&item.plugin, |l| l.manifest.permissions.iter().any(|p| p == "events:character"))
                .unwrap_or(false);
            let seq = {
                let mut seq = self.seq.lock().unwrap_or_else(|p| p.into_inner());
                *seq += 1;
                *seq
            };
            let event = serde_json::json!({
                "seq": seq,
                "event": item.event,
                "data": item.data,
                "character": if allowed { who.clone() } else { None },
            });
            {
                let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
                let list = log.entry(item.plugin.clone()).or_default();
                list.push_back(event.clone());
                while list.len() > MAX_LOGGED {
                    list.pop_front();
                }
            }
            let _ = app.emit_to(format!("plugin-{}", item.plugin), "plugin-event", event);
        }
    }

    fn list(&self) -> Vec<PluginInfo> {
        let mut out: Vec<PluginInfo> = self.loaded.read().unwrap_or_else(|p| p.into_inner()).iter().map(info).collect();
        out.extend(self.errors.read().unwrap_or_else(|p| p.into_inner()).iter().cloned());
        out
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&Loaded) -> T) -> Option<T> {
        self.loaded.read().unwrap_or_else(|p| p.into_inner()).iter().find(|l| l.manifest.id == id).map(f)
    }
}

fn info(l: &Loaded) -> PluginInfo {
    PluginInfo {
        id: l.manifest.id.clone(),
        name: l.manifest.name.clone(),
        version: l.manifest.version.clone(),
        description: l.manifest.description.clone(),
        path: l.dir.to_string_lossy().into_owned(),
        permissions: l.manifest.permissions.clone(),
        reviewed: l.reviewed,
        error: None,
    }
}

/// The plugin a `plugin-<id>` window belongs to.
fn window_plugin(window: &tauri::WebviewWindow) -> Result<String, String> {
    window
        .label()
        .strip_prefix("plugin-")
        .filter(|id| valid_id(id))
        .map(str::to_string)
        .ok_or_else(|| "not a plugin window".to_string())
}

fn host(app: &tauri::AppHandle) -> Result<Arc<PluginHost>, String> {
    app.try_state::<Arc<PluginHost>>().map(|h| Arc::clone(&h)).ok_or_else(|| "plugins are not available".into())
}

fn data_file(app: &tauri::AppHandle, id: &str, key: &str) -> Result<PathBuf, String> {
    if key.is_empty() || key.len() > 64 || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("storage keys are letters, digits, '-' and '_'".into());
    }
    let state = app.try_state::<AppState>().ok_or("no app state")?;
    Ok(state.app_data_dir.join("plugin-data").join(id).join(format!("{key}.json")))
}

// ── Settings ───────────────────────────────────────────────────────────────

#[tauri::command]
pub fn plugin_list(app: tauri::AppHandle) -> Result<PluginsView, String> {
    Ok(view(&app, &*host(&app)?))
}

/// Turn developer mode on or off, and load or unload its plugins.
#[tauri::command]
pub fn plugin_set_dev_mode(app: tauri::AppHandle, enabled: bool) -> Result<PluginsView, String> {
    let state = app.try_state::<AppState>().ok_or("no app state")?;
    state.settings.set(DEV_MODE_KEY, if enabled { "true" } else { "false" });
    let host = host(&app)?;
    host.reload(&app);
    Ok(view(&app, &host))
}

/// Ask for a plugin folder and load it (developer mode).
/// Async, as the screenshot folder picker is: the dialog must not block the main thread.
#[tauri::command]
pub async fn plugin_add_folder(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<PluginsView, String> {
    let Some(folder) = crate::platform::screenshot::pick_folder(&window, None) else {
        return Ok(view(&app, &*host(&app)?));
    };
    load_folder(Path::new(&folder))?;
    let state = app.try_state::<AppState>().ok_or("no app state")?;
    let mut folders: Vec<String> =
        state.settings.get(DEV_FOLDERS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if !folders.contains(&folder) {
        folders.push(folder);
    }
    state.settings.set(DEV_FOLDERS_KEY, &serde_json::to_string(&folders).unwrap_or_default());
    let host = host(&app)?;
    host.reload(&app);
    Ok(view(&app, &host))
}

/// A plugin whose version changed when its folder was read again.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdate {
    pub id: String,
    pub name: String,
    /// None for a plugin that did not load before.
    pub from: Option<String>,
    pub to: String,
}

/// A newer reviewed version in the catalog, for an installed plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAvailable {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    /// Permissions the new version asks for that the installed one did not.
    pub new_permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub view: PluginsView,
    /// Versions that changed on disk (developer folders), now loaded.
    pub updated: Vec<PluginUpdate>,
    /// Newer reviewed versions waiting to be installed.
    pub available: Vec<PluginAvailable>,
    /// Why the catalog could not be checked, if it could not.
    pub catalog_error: Option<String>,
}

/// Reload open plugin windows, closing those whose plugin is gone.
fn refresh_windows(app: &tauri::AppHandle, host: &PluginHost) {
    for (label, window) in app.webview_windows() {
        let Some(id) = label.strip_prefix("plugin-") else { continue };
        if host.with(id, |_| ()).is_some() {
            let _ = window.eval("location.reload()");
        } else {
            let _ = window.close();
        }
    }
}

/// The catalog's newer versions of installed plugins.
#[cfg(feature = "online")]
async fn available_updates(app: &tauri::AppHandle, host: &PluginHost) -> Result<Vec<PluginAvailable>, String> {
    let http = app.try_state::<AppState>().ok_or("no app state")?.http.clone();
    let catalog = catalog::fetch(&http).await?;
    let installed: Vec<PluginInfo> = host.list().into_iter().filter(|p| p.reviewed && p.error.is_none()).collect();
    Ok(installed
        .into_iter()
        .filter_map(|p| {
            let entry = catalog.plugins.iter().find(|e| e.id == p.id)?;
            catalog::newer(&entry.version, &p.version).then(|| PluginAvailable {
                id: p.id.clone(),
                name: entry.name.clone(),
                from: p.version.clone(),
                to: entry.version.clone(),
                new_permissions: entry.permissions.iter().filter(|x| !p.permissions.contains(x)).cloned().collect(),
            })
        })
        .collect())
}

/// Check for plugin updates: read every plugin folder again (a developer
/// plugin's source is its folder) and reload open plugin windows so they run
/// the new files, then ask the signed catalog for newer reviewed versions.
#[tauri::command]
pub async fn plugin_check_updates(app: tauri::AppHandle) -> Result<UpdateCheck, String> {
    let host = host(&app)?;
    let before: HashMap<String, String> =
        host.list().into_iter().filter(|p| p.error.is_none()).map(|p| (p.id, p.version)).collect();
    host.reload(&app);
    let view = view(&app, &host);
    let updated = view
        .plugins
        .iter()
        .filter(|p| p.error.is_none() && before.get(&p.id) != Some(&p.version))
        .map(|p| PluginUpdate { id: p.id.clone(), name: p.name.clone(), from: before.get(&p.id).cloned(), to: p.version.clone() })
        .collect();
    refresh_windows(&app, &host);
    #[cfg(feature = "online")]
    let (available, catalog_error) = match available_updates(&app, &host).await {
        Ok(available) => (available, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    #[cfg(not(feature = "online"))]
    let (available, catalog_error) = (Vec::new(), None);
    Ok(UpdateCheck { view, updated, available, catalog_error })
}

/// A catalog entry as Settings lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub permissions: Vec<String>,
    pub source: String,
    /// The installed version, if any.
    pub installed: Option<String>,
}

/// The reviewed plugins on offer, from the signed catalog.
#[tauri::command]
pub async fn plugin_catalog(app: tauri::AppHandle) -> Result<Vec<CatalogItem>, String> {
    #[cfg(feature = "online")]
    {
        let host = host(&app)?;
        let http = app.try_state::<AppState>().ok_or("no app state")?.http.clone();
        let catalog = catalog::fetch(&http).await?;
        let installed: HashMap<String, String> =
            host.list().into_iter().filter(|p| p.reviewed && p.error.is_none()).map(|p| (p.id, p.version)).collect();
        Ok(catalog
            .plugins
            .into_iter()
            .map(|e| CatalogItem {
                installed: installed.get(&e.id).cloned(),
                id: e.id,
                name: e.name,
                version: e.version,
                description: e.description,
                author: e.author,
                permissions: e.permissions,
                source: e.source,
            })
            .collect())
    }
    #[cfg(not(feature = "online"))]
    {
        let _ = app;
        Err("The offline build does not download plugins.".into())
    }
}

/// Install a reviewed plugin from the catalog, or update it to the catalog's
/// version. Its saved data is kept.
#[tauri::command]
pub async fn plugin_install(app: tauri::AppHandle, id: String) -> Result<PluginsView, String> {
    #[cfg(feature = "online")]
    {
        let host = host(&app)?;
        let http = app.try_state::<AppState>().ok_or("no app state")?.http.clone();
        let catalog = catalog::fetch(&http).await?;
        let entry = catalog.plugins.iter().find(|e| e.id == id).ok_or("that plugin is not in the catalog")?;
        let dir = plugins_dir(&app)?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        catalog::install(&http, entry, &dir).await?;
        tracing::info!("Plugins: installed {} {}", entry.id, entry.version);
        host.reload(&app);
        refresh_windows(&app, &host);
        Ok(view(&app, &host))
    }
    #[cfg(not(feature = "online"))]
    {
        let _ = (app, id);
        Err("The offline build does not download plugins.".into())
    }
}

/// Remove an installed plugin. Its saved data stays, for a later reinstall.
#[tauri::command]
pub fn plugin_uninstall(app: tauri::AppHandle, id: String) -> Result<PluginsView, String> {
    if !valid_id(&id) {
        return Err("not a plugin id".into());
    }
    if let Some(window) = app.get_webview_window(&format!("plugin-{id}")) {
        let _ = window.close();
    }
    remember_window(&app, &id, None);
    let dir = plugins_dir(&app)?.join(&id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    let host = host(&app)?;
    host.reload(&app);
    Ok(view(&app, &host))
}

/// Stop loading a folder (its saved data stays).
#[tauri::command]
pub fn plugin_remove_folder(app: tauri::AppHandle, path: String) -> Result<PluginsView, String> {
    let state = app.try_state::<AppState>().ok_or("no app state")?;
    let mut folders: Vec<String> =
        state.settings.get(DEV_FOLDERS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    folders.retain(|f| f != &path);
    state.settings.set(DEV_FOLDERS_KEY, &serde_json::to_string(&folders).unwrap_or_default());
    let host = host(&app)?;
    if let Some(window) = app.get_webview_window(&format!("plugin-{}", host.list().iter().find(|p| p.path == path).map(|p| p.id.clone()).unwrap_or_default())) {
        let _ = window.close();
    }
    host.reload(&app);
    Ok(view(&app, &host))
}

/// Remember a plugin window's state for the next start (`None`: closed).
fn remember_window(app: &tauri::AppHandle, id: &str, state: Option<&str>) {
    let Some(settings) = app.try_state::<AppState>() else { return };
    let mut windows: serde_json::Map<String, Value> =
        settings.settings.get(WINDOWS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    match state {
        Some(state) => windows.insert(id.to_string(), Value::String(state.into())),
        None => windows.remove(id),
    };
    settings.settings.set(WINDOWS_KEY, &Value::Object(windows).to_string());
}

/// Build (or show) a plugin's window; `visible: false` builds it hidden, for a
/// plugin restored at start that was watching in the background.
fn open_window(app: &tauri::AppHandle, id: &str, visible: bool) -> Result<(), String> {
    let host = host(app)?;
    let (name, width, height) = host
        .with(id, |l| {
            let panel = l.manifest.ui.as_ref().and_then(|u| u.get("panel"));
            let dim = |key: &str, default: f64| {
                panel.and_then(|p| p.get(key)).and_then(Value::as_f64).unwrap_or(default).clamp(240.0, 1200.0)
            };
            (l.manifest.name.clone(), dim("width", 360.0), dim("height", 480.0))
        })
        .ok_or("no such plugin")?;
    let label = format!("plugin-{id}");
    if let Some(window) = app.get_webview_window(&label) {
        if visible {
            let _ = window.show();
            let _ = window.set_focus();
        }
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(app, &label, tauri::WebviewUrl::App("plugin-host.html".into()))
        .initialization_script(format!("window.__A2_PLUGIN_ID__ = {};", serde_json::to_string(id).unwrap_or_default()))
        .title(format!("{name} (plugin)"))
        .inner_size(width, height + 30.0)
        .min_inner_size(240.0, 200.0)
        .decorations(false)
        .always_on_top(true)
        .resizable(true)
        .visible(visible)
        // Transparent, so the panel's opacity slider shows the game through it.
        .transparent(true)
        // No system border or shadow (Windows 11 draws both on a frameless
        // window): the page draws its own edge, which fades with the panel.
        .shadow(false)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Open a plugin's window.
/// Async: on Windows, building a window from a synchronous command deadlocks.
#[tauri::command]
pub async fn plugin_open(app: tauri::AppHandle, id: String) -> Result<(), String> {
    open_window(&app, &id, true)?;
    remember_window(&app, &id, Some("shown"));
    Ok(())
}

/// Open again the plugin windows that were open when the meter last closed.
pub fn restore_windows(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let windows: serde_json::Map<String, Value> =
        state.settings.get(WINDOWS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for (id, how) in windows {
        let visible = how.as_str() != Some("hidden");
        if let Err(e) = open_window(app, &id, visible) {
            tracing::info!("Plugins: not reopening {id}: {e}");
            remember_window(app, &id, None);
        }
    }
}

/// A plugin shows or hides its own window (`window` permission), for one that
/// watches in the background and appears only when it has something to say.
/// Shown on top without taking focus from the game.
#[tauri::command]
pub fn plugin_window(app: tauri::AppHandle, window: tauri::WebviewWindow, visible: bool) -> Result<(), String> {
    let id = window_plugin(&window)?;
    require(&app, &id, "window")?;
    if visible {
        crate::platform::window::show_on_top_without_focus(&window);
    } else {
        let _ = window.hide();
    }
    remember_window(&app, &id, Some(if visible { "shown" } else { "hidden" }));
    Ok(())
}

/// The window's close button: close it, and don't reopen it at the next start.
#[tauri::command]
pub fn plugin_close(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    let id = window_plugin(&window)?;
    remember_window(&app, &id, None);
    let _ = window.close();
    Ok(())
}

// ── Plugin windows ─────────────────────────────────────────────────────────

/// What a plugin window's host page needs to start: the manifest, the run's
/// events so far (the page delivers those it has not yet), and the character
/// (for `events:character`).
#[tauri::command]
pub fn plugin_hello(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<Value, String> {
    let id = window_plugin(&window)?;
    let host = host(&app)?;
    let manifest = host.with(&id, |l| l.manifest.clone()).ok_or("plugin not loaded")?;
    let replay: Vec<Value> =
        host.log.lock().unwrap_or_else(|p| p.into_inner()).get(&id).map(|l| l.iter().cloned().collect()).unwrap_or_default();
    let character =
        if manifest.permissions.iter().any(|p| p == "events:character") { character(&app) } else { None };
    let reviewed = host.with(&id, |l| l.reviewed).unwrap_or(false);
    let language = app.try_state::<AppState>().and_then(|s| s.settings.get("dpsMeter.language")).unwrap_or_else(|| "en".into());
    Ok(serde_json::json!({
        "manifest": manifest,
        "replay": replay,
        "run": host.run,
        "character": character,
        "language": language,
        "reviewed": reviewed,
    }))
}

/// Debug builds only: feed a game packet (hex, with its length prefix) to the
/// plugins as if it had arrived from the network, to test them end to end.
#[tauri::command]
pub fn plugin_debug_frame(app: tauri::AppHandle, hex: String) -> Result<(), String> {
    if !cfg!(debug_assertions) {
        return Err("only in debug builds".into());
    }
    let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let bytes = (0..clean.len() / 2)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|e| e.to_string())?;
    host(&app)?.on_frame(&app, &bytes);
    Ok(())
}

/// Tell every open plugin window something every plugin may know, such as the
/// meter's language when the user changes it.
pub fn broadcast(app: &tauri::AppHandle, event: &str, data: Value) {
    for (label, window) in app.webview_windows() {
        if label.starts_with("plugin-") {
            let _ = window.emit("plugin-event", serde_json::json!({ "event": event, "data": data }));
        }
    }
}

fn require(app: &tauri::AppHandle, id: &str, permission: &str) -> Result<(), String> {
    let has = host(app)?.with(id, |l| l.manifest.permissions.iter().any(|p| p == permission)).unwrap_or(false);
    if has { Ok(()) } else { Err(format!("the plugin lacks the {permission:?} permission")) }
}

#[tauri::command]
pub fn plugin_storage_get(app: tauri::AppHandle, window: tauri::WebviewWindow, key: String) -> Result<Option<String>, String> {
    let id = window_plugin(&window)?;
    require(&app, &id, "storage")?;
    Ok(std::fs::read_to_string(data_file(&app, &id, &key)?).ok())
}

#[tauri::command]
pub fn plugin_storage_set(app: tauri::AppHandle, window: tauri::WebviewWindow, key: String, value: String) -> Result<(), String> {
    let id = window_plugin(&window)?;
    require(&app, &id, "storage")?;
    if value.len() > MAX_STORED {
        return Err("value larger than 1 MB".into());
    }
    let file = data_file(&app, &id, &key)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(file, value).map_err(|e| e.to_string())
}

// ── The plugin files ───────────────────────────────────────────────────────

/// The `a2` object plugins load from `/_sdk/a2.js`.
const SDK: &str = include_str!("a2.js");

/// Serve `/<id>/<path>` from that plugin's folder, and `/_sdk/a2.js`.
/// The content policy for a plugin's files: its own folder and the SDK, and
/// nothing from anywhere else (no network at all).
///
/// The sources are named outright. A plugin page runs in a sandboxed frame,
/// whose origin is opaque, and WebKit (the Linux webview) matches `'self'`
/// against that opaque origin, so with `'self'` alone it refused the page's
/// own scripts and styles and plugins showed up unstyled and empty (2.0.60).
/// Chromium (WebView2) matches `'self'` against the URL instead. On Linux the
/// scheme itself is allowed too, in case WebKit does not match a path on a
/// custom scheme: that lets a plugin load another plugin's files, which gives
/// it nothing (each runs in the same sandbox, with only its own permissions).
fn content_policy(id: &str) -> String {
    let base = if cfg!(windows) { "http://a2plugin.localhost" } else { "a2plugin://localhost" };
    let mut own = format!("'self' {base}/_sdk/a2.js");
    if valid_id(id) {
        own.push_str(&format!(" {base}/{id}/"));
    }
    if !cfg!(windows) {
        own.push_str(" a2plugin:");
    }
    format!(
        "default-src 'none'; script-src {own}; style-src {own} 'unsafe-inline'; img-src {own} data:;          font-src {own} data:; connect-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

pub fn serve(app: &tauri::AppHandle, path: &str) -> tauri::http::Response<Vec<u8>> {
    let path = percent_decode(path.trim_start_matches('/'));
    let policy = content_policy(path.split('/').next().unwrap_or(""));
    let respond = |status: u16, kind: &str, body: Vec<u8>| {
        tauri::http::Response::builder()
            .status(status)
            .header("Content-Type", kind)
            .header("Content-Security-Policy", policy.as_str())
            .header("X-Content-Type-Options", "nosniff")
            .body(body)
            .unwrap_or_default()
    };
    if path == "_sdk/a2.js" {
        return respond(200, "text/javascript; charset=utf-8", SDK.as_bytes().to_vec());
    }
    let Some((id, rest)) = path.split_once('/') else { return respond(404, "text/plain", Vec::new()) };
    let Ok(host) = host(app) else { return respond(404, "text/plain", Vec::new()) };
    let Some(dir) = host.with(id, |l| l.dir.clone()) else { return respond(404, "text/plain", Vec::new()) };
    // Inside the plugin's folder only.
    let (Ok(root), Ok(file)) = (dir.canonicalize(), dir.join(rest).canonicalize()) else {
        return respond(404, "text/plain", Vec::new());
    };
    if !file.starts_with(&root) || !file.is_file() {
        return respond(404, "text/plain", Vec::new());
    }
    // On Linux a plugin page gets its scripts, styles and images inlined:
    // WebKitGTK (2.54 at least) refuses every file a sandboxed frame (an
    // opaque origin) asks for from a custom scheme ("Unsafe attempt to load
    // URL … from origin null"), before the page's policy is even consulted,
    // so plugins showed their bare HTML and nothing ran (2.0.60, 2.0.61).
    // Inlining keeps the frame's opaque-origin sandbox, which
    // `allow-same-origin` (the other way out) would give up.
    if !cfg!(windows) && file.extension().is_some_and(|e| e == "html") {
        if let Ok(html) = std::fs::read_to_string(&file) {
            let (page, hashes) = inline_page(&html, &root, file.parent().unwrap_or(&root));
            let policy = inlined_policy(id, &hashes);
            return tauri::http::Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Content-Security-Policy", policy.as_str())
                .header("X-Content-Type-Options", "nosniff")
                .body(page.into_bytes())
                .unwrap_or_default();
        }
    }
    let kind = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    match std::fs::read(&file) {
        Ok(body) => respond(200, kind, body),
        Err(_) => respond(404, "text/plain", Vec::new()),
    }
}

/// A plugin page with its `<script src>`, stylesheet `<link>` and `<img src>`
/// replaced by the files themselves (images as `data:` URLs), and the SHA-256
/// of each inlined script for the policy. Only the SDK and files inside the
/// plugin's folder are inlined; any other tag is left as it is (and stays
/// refused).
fn inline_page(html: &str, root: &Path, page_dir: &Path) -> (String, Vec<String>) {
    let file_of = |src: &str| -> Option<PathBuf> {
        if src.contains(':') || src.starts_with('/') {
            return None;
        }
        let file = page_dir.join(percent_decode(src.split(['?', '#']).next()?)).canonicalize().ok()?;
        file.starts_with(root).then_some(file)
    };
    let read = |src: &str| -> Option<String> {
        if src.trim_start_matches('/') == "_sdk/a2.js" {
            return Some(SDK.to_string());
        }
        std::fs::read_to_string(file_of(src)?).ok()
    };
    let image = |src: &str| -> Option<String> {
        let file = file_of(src)?;
        let kind = match file.extension()?.to_str()? {
            "png" => "image/png",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "jpg" | "jpeg" => "image/jpeg",
            _ => return None,
        };
        Some(format!("data:{kind};base64,{}", base64(&std::fs::read(file).ok()?)))
    };
    let mut out = String::with_capacity(html.len() * 2);
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..=end];
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with("<script") && rest[end + 1..].to_ascii_lowercase().starts_with("</script>") {
            if let Some(code) = attribute(tag, "src").and_then(|src| read(&src)) {
                // "</script" inside the code would end the element early.
                let code = code.replace("</script", "<\\/script");
                hashes.push(base64(&sha2::Sha256::digest(code.as_bytes())));
                out.push_str("<script>");
                out.push_str(&code);
                out.push_str("</script>");
                rest = &rest[end + 1 + "</script>".len()..];
                continue;
            }
        } else if lower.starts_with("<link") && attribute(tag, "rel").is_some_and(|r| r.eq_ignore_ascii_case("stylesheet")) {
            if let Some(css) = attribute(tag, "href").and_then(|href| read(&href)) {
                out.push_str("<style>");
                out.push_str(&css.replace("</style", "<\\/style"));
                out.push_str("</style>");
                rest = &rest[end + 1..];
                continue;
            }
        } else if lower.starts_with("<img") {
            if let Some((src, data)) = attribute(tag, "src").and_then(|src| image(&src).map(|d| (src, d))) {
                out.push_str(&tag.replacen(&src, &data, 1));
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push_str(tag);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    (out, hashes)
}

/// An attribute's value in an opening tag, quoted or not.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        if !lower[..at].chars().last().is_some_and(char::is_whitespace) {
            continue;
        }
        let Some(after) = tag[from..].trim_start().strip_prefix('=') else { continue };
        let after = after.trim_start();
        let value = match after.chars().next()? {
            q @ ('"' | '\'') => after[1..].split(q).next()?,
            _ => after.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next()?,
        };
        return Some(value.to_string());
    }
    None
}

/// Standard base64, with padding.
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// The policy for an inlined page: only its own inlined scripts run (by
/// hash), its inlined styles apply, and nothing loads from anywhere.
fn inlined_policy(id: &str, hashes: &[String]) -> String {
    let scripts: String = hashes.iter().map(|h| format!(" 'sha256-{h}'")).collect();
    let own = if valid_id(id) { format!("a2plugin://localhost/{id}/") } else { "'none'".into() };
    format!(
        "default-src 'none'; script-src{}; style-src 'unsafe-inline'; img-src {own} data:; font-src {own} data:; \
         connect-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'",
        if scripts.is_empty() { " 'none'".to_string() } else { scripts }
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{base64, content_policy, inline_page, inlined_policy};

    #[test]
    fn a_page_gets_its_own_scripts_styles_and_images_inlined() {
        let dir = std::env::temp_dir().join(format!("a2plugin-inline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("plugin")).unwrap();
        std::fs::write(dir.join("plugin/plugin.js"), "go('</script>');").unwrap();
        std::fs::write(dir.join("plugin/style.css"), "body{color:red}").unwrap();
        std::fs::write(dir.join("plugin/dot.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join("secret.js"), "nope").unwrap();
        let root = dir.join("plugin").canonicalize().unwrap();
        let html = concat!(
            "<link rel=\"stylesheet\" href=\"style.css\" />",
            "<script src=\"/_sdk/a2.js\"></script>",
            "<script src=\"plugin.js\"></script>",
            "<script src=\"../secret.js\"></script>",
            "<script src=\"https://example.com/x.js\"></script>",
            "<img alt=\"\" src=\"dot.png\">",
        );
        let (page, hashes) = inline_page(html, &root, &root);
        assert!(page.contains("<style>body{color:red}</style>"), "{page}");
        assert!(page.contains("window.a2 = Object.freeze"), "the SDK");
        assert!(page.contains("<script>go('<\\/script>');</script>"), "{page}");
        assert!(page.contains("src=\"data:image/png;base64,AQID\""), "{page}");
        // Outside the folder or elsewhere: left as a tag, which the policy refuses.
        assert!(page.contains("<script src=\"../secret.js\"></script>"));
        assert!(page.contains("https://example.com/x.js"));
        assert_eq!(hashes.len(), 2);
        let policy = inlined_policy("plugin", &hashes);
        assert!(policy.contains(&format!("script-src 'sha256-{}' 'sha256-{}';", hashes[0], hashes[1])), "{policy}");
        assert!(policy.contains("connect-src 'none'"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_of_a_sha256() {
        use sha2::Digest;
        // printf abc | sha256sum | xxd -r -p | base64
        assert_eq!(base64(&sha2::Sha256::digest(b"abc")), "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn the_policy_names_the_plugins_own_files() {
        let base = if cfg!(windows) { "http://a2plugin.localhost" } else { "a2plugin://localhost" };
        let policy = content_policy("pet-souls");
        for directive in ["script-src", "style-src"] {
            let sources = policy.split(';').find(|d| d.trim().starts_with(directive)).unwrap();
            // Not 'self' alone: WebKit refuses everything in a sandboxed frame then.
            assert!(sources.contains(&format!("{base}/pet-souls/")), "{sources}");
            assert!(sources.contains(&format!("{base}/_sdk/a2.js")), "{sources}");
        }
        assert!(policy.contains("connect-src 'none'"));
        // A path that is not a plugin id adds no folder.
        assert!(!content_policy("../x").contains("/../x/"));
    }
}
