//! Reviewed plugins: the signed catalog and installing from it.
//!
//! The plugins repo (taengu/A2Tools-Plugins) publishes, on each merge, every
//! plugin's files and an `index.json` listing them with their SHA-256 hashes,
//! signed with an Ed25519 key that only the repo's release workflow holds.
//! The meter checks that signature against the key built in below, and
//! installs a file only if its hash matches the signed index, so nothing
//! from anywhere else installs.
//!
//!   https://cdn.a2tools.app/plugins/index.json       the catalog
//!   https://cdn.a2tools.app/plugins/index.json.sig   its signature, hex
//!   https://cdn.a2tools.app/plugins/<id>/<version>/<file>

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BASE_URL: &str = "https://cdn.a2tools.app/plugins";
/// The catalog signing key's public half (hex). Its private half is the
/// `PLUGIN_SIGNING_KEY` secret of the plugins repo, and nowhere else.
const PUBLIC_KEY_HEX: &str = "d71e888cd5530b57254a4f87bf8f1861d8ddd8f7ddf45ecdba0771bb8f7d54ae";
/// Where the catalog lives, and the key that signs it. A debug build takes
/// both from `A2_PLUGIN_CATALOG` (a base URL) and `A2_PLUGIN_KEY` (hex) when
/// set, to test publishing against a local catalog; a release build never does.
fn base_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("A2_PLUGIN_CATALOG") {
        return url.trim_end_matches('/').to_string();
    }
    BASE_URL.to_string()
}

fn public_key() -> String {
    #[cfg(debug_assertions)]
    if let Ok(key) = std::env::var("A2_PLUGIN_KEY") {
        return key;
    }
    PUBLIC_KEY_HEX.to_string()
}

/// Largest catalog, and largest plugin (all its files), the meter downloads.
const MAX_CATALOG: usize = 1024 * 1024;
const MAX_PLUGIN: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub format: u32,
    pub plugins: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
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
    /// Where its source can be read.
    #[serde(default)]
    pub source: String,
    /// Each file's path in the plugin folder → its SHA-256 (hex).
    pub files: BTreeMap<String, String>,
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Check the catalog's signature with `key` (hex) and read it.
fn verify_with(key_hex: &str, body: &[u8], signature_hex: &str) -> Result<Catalog, String> {
    let key: [u8; 32] = hex_bytes(key_hex)
        .and_then(|k| k.try_into().ok())
        .ok_or("this build has no plugin catalog key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "the plugin catalog key is not valid")?;
    let signature: [u8; 64] = hex_bytes(signature_hex)
        .and_then(|s| s.try_into().ok())
        .ok_or("the plugin catalog's signature is malformed")?;
    key.verify_strict(body, &Signature::from_bytes(&signature))
        .map_err(|_| "the plugin catalog's signature does not match: not installing from it")?;
    let catalog: Catalog = serde_json::from_slice(body).map_err(|e| format!("plugin catalog: {e}"))?;
    if catalog.format != 1 {
        return Err(format!("plugin catalog format {} (this meter reads 1): update the meter", catalog.format));
    }
    Ok(catalog)
}

/// A path inside a plugin folder: plain names separated by `/`, nothing that
/// could climb out of it.
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 128
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        })
}

async fn get(http: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let response = http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() > cap {
        return Err(format!("{url}: larger than {cap} bytes"));
    }
    Ok(bytes.to_vec())
}

/// Download the catalog and check its signature.
pub async fn fetch(http: &reqwest::Client) -> Result<Catalog, String> {
    let base = base_url();
    let body = get(http, &format!("{base}/index.json"), MAX_CATALOG).await?;
    let signature = get(http, &format!("{base}/index.json.sig"), 1024).await?;
    verify_with(&public_key(), &body, &String::from_utf8_lossy(&signature))
}

/// Download a catalog entry's files into `plugins_dir/<id>`, replacing what
/// was there. Every file must match its hash in the signed catalog; nothing
/// is replaced unless all of them do.
pub async fn install(http: &reqwest::Client, entry: &Entry, plugins_dir: &Path) -> Result<PathBuf, String> {
    if !super::valid_id(&entry.id) {
        return Err(format!("plugin id {:?} is not valid", entry.id));
    }
    if entry.api_version == 0 || entry.api_version > super::API_VERSION {
        return Err(format!("{} needs a newer meter (plugin API {})", entry.name, entry.api_version));
    }
    if !entry.files.contains_key("manifest.json") || !entry.files.contains_key("index.html") {
        return Err(format!("{} is missing its manifest or page", entry.name));
    }
    let mut files = Vec::new();
    let mut total = 0;
    for (path, hash) in &entry.files {
        if !safe_path(path) {
            return Err(format!("{}: file {path:?} is not allowed", entry.name));
        }
        let url = format!("{}/{}/{}/{path}", base_url(), entry.id, entry.version);
        let bytes = get(http, &url, MAX_PLUGIN).await?;
        total += bytes.len();
        if total > MAX_PLUGIN {
            return Err(format!("{} is larger than {MAX_PLUGIN} bytes", entry.name));
        }
        if !sha256_hex(&bytes).eq_ignore_ascii_case(hash) {
            return Err(format!("{}: {path} does not match the catalog, not installing", entry.name));
        }
        files.push((path.clone(), bytes));
    }

    let staging = plugins_dir.join(format!(".staging-{}", entry.id));
    let target = plugins_dir.join(&entry.id);
    let _ = std::fs::remove_dir_all(&staging);
    for (path, bytes) in &files {
        let file = staging.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    }
    let loaded = super::load_folder(&staging)?;
    if loaded.manifest.id != entry.id || loaded.manifest.version != entry.version {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("{}: its manifest does not match the catalog", entry.name));
    }
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

/// Whether `candidate` is a later version than `current` ("1.2.10" > "1.2.9").
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(candidate) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(catalog: &str) -> (String, String) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public: String = key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let signature: String = key.sign(catalog.as_bytes()).to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        (public, signature)
    }

    const CATALOG: &str = r#"{"format":1,"plugins":[{"id":"pet-souls","name":"Pet Soul Tracker","version":"0.2.0","apiVersion":1,"files":{"manifest.json":"00","index.html":"00"}}]}"#;

    #[test]
    fn a_signed_catalog_reads() {
        let (key, signature) = signed(CATALOG);
        let catalog = verify_with(&key, CATALOG.as_bytes(), &signature).unwrap();
        assert_eq!(catalog.plugins[0].id, "pet-souls");
    }

    #[test]
    fn a_changed_catalog_is_refused() {
        let (key, signature) = signed(CATALOG);
        let tampered = CATALOG.replace("0.2.0", "0.2.1");
        assert!(verify_with(&key, tampered.as_bytes(), &signature).is_err());
        assert!(verify_with("", CATALOG.as_bytes(), &signature).is_err());
        assert!(verify_with(&key, CATALOG.as_bytes(), "abcd").is_err());
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        assert!(safe_path("index.html"));
        assert!(safe_path("img/pet-1.png"));
        for bad in ["", "../x", "a/../b", "/abs", "a//b", "a\\b", "C:x", "./a"] {
            assert!(!safe_path(bad), "{bad}");
        }
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.2.10", "0.2.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
    }
}
