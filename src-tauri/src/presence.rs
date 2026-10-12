//! Discord activity: "Playing AION2 — Naicha · Cleric · Lv 29 — Kaisinel (Elyos) · Europe".
//!
//! Opt-in (`dpsMeter.discordActivity`), and shown only while AION2 is open.
//! It talks to the Discord apps on this computer over their local IPC sockets,
//! so it needs no a2tools.app account and sends nothing anywhere else: Discord
//! already knows who its own user is. Every Discord running is told (Discord
//! and Vesktop side by side each have a socket, and Discord shows the activity
//! of whichever one you use), not only the first one found.
//!
//! Everything shown is what the game has told the meter about the local
//! player (see `DataStorage::local_profile`): class and level from the self
//! record, the server from it or from loot. A field nobody has stated yet is
//! left out rather than guessed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::io::{Read, Write};

use discord_rich_presence::activity::{Activity, Assets, Timestamps};
use tauri::Manager;

use crate::app::AppState;
use crate::combat::data_storage::{DataStorage, LocalProfile};
use crate::entity::job_class::JobClass;
use crate::platform::window_detector;

/// The Discord application whose name ("AION2") the activity shows under, and
/// whose art assets hold the class icons (keys as `class_key` returns them).
const DISCORD_APPLICATION_ID: &str = "1556021373401432214";

pub const SETTING_KEY: &str = "dpsMeter.discordActivity";

const TICK: Duration = Duration::from_secs(10);
/// Discord not running: try again this often, not every tick.
const RECONNECT_EVERY: Duration = Duration::from_secs(30);
/// A boss counts as being fought while it took damage this recently.
const FIGHTING_WITHIN_MS: i64 = 20_000;

/// Server names in every language the game ships, and region names.
static SERVERS: std::sync::LazyLock<ServerTable> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../../src/data/servers.json")).unwrap_or_default()
});

#[derive(Default, serde::Deserialize)]
struct ServerTable {
    servers: HashMap<String, HashMap<String, String>>,
    regions: HashMap<String, HashMap<String, String>>,
}

/// What the activity says. Compared between ticks so Discord is only told
/// when something changed.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    details: String,
    /// Class and level alone, for the class image's hover text.
    class_line: String,
    state: String,
    class_key: Option<&'static str>,
    class_name: Option<String>,
}

/// Whether a Discord application is configured into this build at all.
pub fn available() -> bool {
    !DISCORD_APPLICATION_ID.is_empty()
}

pub fn spawn(app: tauri::AppHandle) {
    if !available() {
        return;
    }
    std::thread::Builder::new()
        .name("discord-activity".into())
        .spawn(move || run(app))
        .ok();
}

fn run(app: tauri::AppHandle) {
    let mut discords = Discords::default();
    let mut shown: Option<Shown> = None;
    let mut game_since: Option<i64> = None;
    let mut last_scan: Option<Instant> = None;
    // Gear score is only known while in a party: keep the last one seen for
    // the same character after the party ends.
    let mut last_gear: Option<(String, i32)> = None;
    loop {
        std::thread::sleep(TICK);
        let Some(state) = app.try_state::<AppState>() else { continue };
        let enabled = state.settings.get(SETTING_KEY).as_deref() == Some("true");
        let playing = window_detector::find_aion2_window();
        if !playing {
            game_since = None;
        }
        if !enabled || !playing {
            if !discords.is_empty() {
                discords.clear_all();
                tracing::info!("Discord activity cleared");
            }
            shown = None;
            continue;
        }
        let since = *game_since.get_or_insert_with(crate::clock::now_ms);

        let lang = state.settings.get("dpsMeter.language").unwrap_or_else(|| "en".into());
        let text = Texts::load(state.i18n_data_dir.as_ref(), &lang);
        let next = describe(&state.data_storage, &state, &text, &lang, &mut last_gear);

        // A Discord started after the game is picked up on a later scan, and
        // told the current activity even when it has not changed.
        let mut joined = false;
        if last_scan.is_none_or(|t| t.elapsed() >= RECONNECT_EVERY) {
            last_scan = Some(Instant::now());
            joined = discords.connect_new();
        }
        if discords.is_empty() || (shown.as_ref() == Some(&next) && !joined) {
            continue;
        }

        let mut assets = Assets::new().small_image("a2tools").small_text("A2Tools DPS Meter");
        // The image's hover text is the class with its level ("Cleric · Lv
        // 30"): the line beside it adds the name.
        if let Some(key) = next.class_key {
            assets = assets.large_image(key).large_text(next.class_line.as_str());
        }
        let mut activity = Activity::new()
            .details(next.details.as_str())
            .assets(assets)
            .timestamps(Timestamps::new().start(since));
        if !next.state.is_empty() {
            activity = activity.state(next.state.as_str());
        }
        let activity = serde_json::to_value(activity).unwrap_or(serde_json::Value::Null);
        discords.set_all(&activity);
        // Discord closed in the meantime: told again once it is back.
        shown = if discords.is_empty() { None } else { Some(next) };
    }
}

/// One connection per Discord socket found (`discord-ipc-0` … `-9`, also
/// inside Flatpak and Snap sandboxes), by the socket's path.
#[derive(Default)]
struct Discords {
    open: Vec<(PathBuf, IpcStream)>,
    nonce: u64,
}

#[cfg(unix)]
type IpcStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type IpcStream = std::fs::File;

impl Discords {
    fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Connects to every Discord not connected yet; true when one was added.
    fn connect_new(&mut self) -> bool {
        let mut added = false;
        for path in ipc_paths() {
            if self.open.iter().any(|(p, _)| *p == path) {
                continue;
            }
            match handshake(&path) {
                Ok((stream, who)) => {
                    tracing::info!("Discord activity: connected to {} ({who})", path.display());
                    self.open.push((path, stream));
                    added = true;
                }
                // A socket left behind by a Discord that crashed.
                Err(e) => tracing::debug!("Discord activity: {} not reachable ({e})", path.display()),
            }
        }
        added
    }

    fn set_all(&mut self, activity: &serde_json::Value) {
        let mut keep = Vec::with_capacity(self.open.len());
        for (path, mut stream) in std::mem::take(&mut self.open) {
            self.nonce += 1;
            let request = serde_json::json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": activity },
                "nonce": format!("a2tools-{}", self.nonce),
            });
            match send(&mut stream, 1, &request).and_then(|()| recv(&mut stream)) {
                Ok(reply) => {
                    if reply.get("evt").and_then(|e| e.as_str()) == Some("ERROR") {
                        tracing::warn!("Discord activity: {} refused it: {}", path.display(), reply["data"]);
                    }
                    keep.push((path, stream));
                }
                // Discord closed or restarted: connect again on a later scan.
                Err(e) => tracing::info!("Discord activity: lost {} ({e}); will reconnect", path.display()),
            }
        }
        self.open = keep;
    }

    fn clear_all(&mut self) {
        for (_, mut stream) in std::mem::take(&mut self.open) {
            self.nonce += 1;
            let request = serde_json::json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id() },
                "nonce": format!("a2tools-{}", self.nonce),
            });
            let _ = send(&mut stream, 1, &request).and_then(|()| recv(&mut stream));
        }
    }
}

/// Where Discord apps put their sockets, as the `discord-rich-presence`
/// crate searches them, but every one that exists rather than the first.
#[cfg(unix)]
fn ipc_paths() -> Vec<PathBuf> {
    const SUBPATHS: [&str; 7] = [
        "",
        "app/com.discordapp.Discord/",
        "app/dev.vencord.Vesktop/",
        ".flatpak/com.discordapp.Discord/xdg-run/",
        ".flatpak/dev.vencord.Vesktop/xdg-run/",
        "snap.discord-canary/",
        "snap.discord/",
    ];
    let mut found = Vec::new();
    for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        let Ok(base) = std::env::var(key) else { continue };
        let base = PathBuf::from(base);
        for sub in SUBPATHS {
            for i in 0..10 {
                let path = base.join(sub).join(format!("discord-ipc-{i}"));
                if path.exists() && !found.contains(&path) {
                    found.push(path);
                }
            }
        }
    }
    found
}

#[cfg(windows)]
fn ipc_paths() -> Vec<PathBuf> {
    (0..10)
        .map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}")))
        .filter(|p| std::fs::metadata(p).is_ok())
        .collect()
}

#[cfg(unix)]
fn open_ipc(path: &PathBuf) -> std::io::Result<IpcStream> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    // Discord answers at once; a hung one must not stall the activity thread.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

#[cfg(windows)]
fn open_ipc(path: &PathBuf) -> std::io::Result<IpcStream> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

/// Opens the socket and says hello as the AION2 application; returns the
/// stream and the Discord user it answered for.
fn handshake(path: &PathBuf) -> std::io::Result<(IpcStream, String)> {
    let mut stream = open_ipc(path)?;
    send(&mut stream, 0, &serde_json::json!({ "v": 1, "client_id": DISCORD_APPLICATION_ID }))?;
    let reply = recv(&mut stream)?;
    if reply.get("evt").and_then(|e| e.as_str()) != Some("READY") {
        return Err(std::io::Error::other(format!("handshake refused: {reply}")));
    }
    let who = reply["data"]["user"]["username"].as_str().unwrap_or("?").to_string();
    Ok((stream, who))
}

/// One frame: opcode and length (little-endian u32 each), then the JSON.
fn send(stream: &mut IpcStream, opcode: u32, body: &serde_json::Value) -> std::io::Result<()> {
    let body = body.to_string();
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body.as_bytes());
    stream.write_all(&frame)
}

fn recv(stream: &mut IpcStream) -> std::io::Result<serde_json::Value> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let opcode = u32::from_le_bytes(header[..4].try_into().unwrap());
    let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
    if len > 1 << 20 {
        return Err(std::io::Error::other("reply too large"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    // Opcode 2: Discord is closing this connection.
    if opcode == 2 {
        return Err(std::io::Error::other(String::from_utf8_lossy(&body).into_owned()));
    }
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

/// The class's art-asset key in the Discord application.
fn class_key(class: JobClass) -> &'static str {
    match class {
        JobClass::Gladiator => "gladiator",
        JobClass::Templar => "templar",
        JobClass::Ranger => "ranger",
        JobClass::Assassin => "assassin",
        JobClass::Sorcerer => "sorcerer",
        JobClass::Cleric => "cleric",
        // The asset is named for the class as the game calls it.
        JobClass::Elementalist => "spiritmaster",
        JobClass::Chanter => "chanter",
        JobClass::Fighter => "brawler",
    }
}

fn describe(
    storage: &Arc<DataStorage>,
    state: &AppState,
    text: &Texts,
    lang: &str,
    last_gear: &mut Option<(String, i32)>,
) -> Shown {
    let profile = storage.local_profile();
    let gear_score = remembered_gear_score(&profile, last_gear);
    let class_name = profile.class.map(|c| text.class(c));
    let mut details: Vec<String> = Vec::new();
    if let Some(name) = &class_name {
        details.push(name.clone());
    }
    if let Some(level) = profile.level {
        details.push(text.get("level", "Lv {level}").replace("{level}", &level.to_string()));
    }
    if let Some(gs) = gear_score {
        details.push(text.get("gearScore", "GS {gs}").replace("{gs}", &gs.to_string()));
    }
    let class_line = if details.is_empty() { text.get("inGame", "In game") } else { details.join(" · ") };
    // The character's name leads: "Naicha · Cleric · Lv 30". With a gear
    // score the class gives way to it ("Naicha · Lv 30 · GS 1520"): the
    // profile popout cuts the line at about 30 characters, and the class
    // icon beside it (hover: class, level and gear score) already says it.
    let short_line = match (&class_name, gear_score) {
        (Some(_), Some(_)) if details.len() > 1 => details[1..].join(" · "),
        _ => class_line.clone(),
    };
    let details = match profile.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{name} · {short_line}"),
        None => class_line.clone(),
    };

    let state_line = fighting(storage, state)
        .map(|boss| text.get("fighting", "Fighting {boss}").replace("{boss}", &boss))
        .or_else(|| in_dungeon(storage.current_dungeon_id(), state.i18n_data_dir.as_ref(), text, lang))
        .unwrap_or_else(|| where_from(&profile, text, lang));

    Shown {
        details,
        class_line,
        state: state_line,
        class_key: profile.class.map(class_key),
        class_name,
    }
}

/// This character's gear score: the roster's when in a party, else the last
/// one the roster gave for the same character.
fn remembered_gear_score(profile: &LocalProfile, last: &mut Option<(String, i32)>) -> Option<i32> {
    let name = profile.name.as_deref().map(str::trim).filter(|n| !n.is_empty())?;
    if let Some(gs) = profile.gear_score {
        *last = Some((name.to_string(), gs));
        return Some(gs);
    }
    last.as_ref().filter(|(n, _)| n == name).map(|(_, gs)| *gs)
}

/// The boss being fought right now, by name, if any.
fn fighting(storage: &Arc<DataStorage>, state: &AppState) -> Option<String> {
    let target = storage.current_target();
    if target == 0 {
        return None;
    }
    let code = *storage.get_mob_data().get(&target)?;
    if !state.npc_lookup.is_boss(code) || state.npc_lookup.is_training_dummy(code) {
        return None;
    }
    let last_hit = storage.get_combat_snapshot_light().get(&target)?.last_damage_time;
    if crate::clock::now_ms() - last_hit > FIGHTING_WITHIN_MS {
        return None;
    }
    let name = state.npc_lookup.get_npc_name(code);
    (!name.is_empty()).then_some(name)
}

/// "Urugugu Canyon · Conquest (Normal)", for the instance the party roster
/// says you are in. Its last digit is the difficulty; dungeons with nine ids
/// number levels instead, and are named without one.
fn in_dungeon(dungeon_id: i32, dir: Option<&PathBuf>, text: &Texts, lang: &str) -> Option<String> {
    if dungeon_id <= 0 {
        return None;
    }
    let read = |l: &str| -> Option<serde_json::Value> {
        let path = dir?.join("dungeons").join(format!("{l}.json"));
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let table = read(lang).or_else(|| read("en"))?;
    let table = table.as_object()?;
    let name = table.get(&dungeon_id.to_string())?.get("name")?.as_str()?.to_string();
    let group = dungeon_id - dungeon_id % 10;
    let ids_in_group = table
        .keys()
        .filter_map(|k| k.parse::<i32>().ok())
        .filter(|id| id - id % 10 == group)
        .count();
    let tier = if ids_in_group >= 9 {
        None
    } else {
        match dungeon_id % 10 {
            1 => Some(text.get("tierExpedition", "Expedition")),
            2 => Some(text.get("tierConquestNormal", "Conquest (Normal)")),
            3 => Some(text.get("tierConquestHard", "Conquest (Hard)")),
            _ => None,
        }
    };
    Some(match tier {
        Some(t) => format!("{name} · {t}"),
        None => name,
    })
}

/// "Kaisinel (Elyos) · Europe".
fn where_from(profile: &LocalProfile, text: &Texts, lang: &str) -> String {
    let sid = profile.server_id;
    if !(1000..3000).contains(&sid) {
        return String::new();
    }
    let table_lang = match lang {
        "zh-Hans" | "zh-Hant" => "zh-Hant",
        other => other,
    };
    let server = SERVERS
        .servers
        .get(&sid.to_string())
        .and_then(|names| names.get(table_lang).or_else(|| names.get("en")))
        .cloned();
    let faction = if sid / 1000 == 1 { text.get("elyos", "Elyos") } else { text.get("asmodian", "Asmodian") };
    let region = match sid / 100 % 10 {
        // Korea and Taiwan share these numbers; the meter's language and the
        // computer's time zone tell them apart, as for uploaded logs.
        0 => {
            let korean = lang == "ko" || chrono::Local::now().offset().local_minus_utc() == 9 * 3600;
            Some(if korean { text.get("korea", "Korea") } else { text.get("taiwan", "Taiwan") })
        }
        d => {
            let code = match d {
                1 => "NAE",
                2 => "NAW",
                3 => "EU",
                4 => "LA",
                5 => "AS",
                _ => "",
            };
            SERVERS
                .regions
                .get(table_lang)
                .and_then(|r| r.get(code))
                .or_else(|| SERVERS.regions.get("en").and_then(|r| r.get(code)))
                .cloned()
        }
    };
    let mut out = match server {
        Some(s) => format!("{s} ({faction})"),
        None => faction,
    };
    if let Some(r) = region {
        out = format!("{out} · {r}");
    }
    out
}

/// The `presence` and `classes` sections of the meter's own UI strings.
struct Texts {
    presence: HashMap<String, String>,
    classes: HashMap<String, String>,
}

impl Texts {
    fn load(dir: Option<&PathBuf>, lang: &str) -> Self {
        let read = |l: &str| -> Option<serde_json::Value> {
            let path = dir?.join("ui").join(format!("{l}.json"));
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
        };
        let doc = read(lang).or_else(|| read("en")).unwrap_or_default();
        let section = |name: &str| -> HashMap<String, String> {
            doc.get(name)
                .and_then(|v| v.as_object())
                .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default()
        };
        Self { presence: section("presence"), classes: section("classes") }
    }

    fn get(&self, key: &str, fallback: &str) -> String {
        self.presence.get(key).cloned().unwrap_or_else(|| fallback.to_string())
    }

    fn class(&self, class: JobClass) -> String {
        let key = match class {
            JobClass::Fighter => "FIGHTER".to_string(),
            JobClass::Elementalist => "ELEMENTALIST".to_string(),
            other => class_key(other).to_uppercase(),
        };
        self.classes.get(&key).cloned().unwrap_or_else(|| format!("{class:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts() -> Texts {
        Texts { presence: HashMap::new(), classes: HashMap::new() }
    }

    #[test]
    fn a_global_server_reads_as_name_faction_and_region() {
        let p = LocalProfile { server_id: 1304, ..Default::default() };
        assert_eq!(where_from(&p, &texts(), "en"), "Kaisinel (Elyos) · Europe");
        let p = LocalProfile { server_id: 2304, ..Default::default() };
        assert_eq!(where_from(&p, &texts(), "en"), "Lumiel (Asmodian) · Europe");
        assert_eq!(where_from(&LocalProfile::default(), &texts(), "en"), "");
    }

    #[test]
    fn a_dungeon_is_named_with_its_difficulty() {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/data/i18n"));
        assert_eq!(in_dungeon(600012, Some(&dir), &texts(), "en").as_deref(), Some("Urugugu Canyon · Conquest (Normal)"));
        assert_eq!(in_dungeon(600091, Some(&dir), &texts(), "en").as_deref(), Some("Ferocious Horn Den · Expedition"));
        assert_eq!(in_dungeon(0, Some(&dir), &texts(), "en"), None);
    }

    #[test]
    fn the_gear_score_outlives_the_party_for_the_same_character() {
        let mut last = None;
        let in_party = LocalProfile { name: Some("Naicha".into()), gear_score: Some(1520), ..Default::default() };
        assert_eq!(remembered_gear_score(&in_party, &mut last), Some(1520));
        let solo = LocalProfile { name: Some("Naicha".into()), ..Default::default() };
        assert_eq!(remembered_gear_score(&solo, &mut last), Some(1520));
        let other = LocalProfile { name: Some("Alt".into()), ..Default::default() };
        assert_eq!(remembered_gear_score(&other, &mut last), None);
    }

    #[test]
    fn the_server_name_follows_the_language() {
        let p = LocalProfile { server_id: 1304, ..Default::default() };
        assert!(where_from(&p, &texts(), "ko").starts_with("카이시넬"));
    }
}
