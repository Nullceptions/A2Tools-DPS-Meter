//! A tap on every decoded game frame, for plugin decoders (`crate::plugins`).
//!
//! The parser calls `frame` for each frame it reads, whole frames and those
//! inside compressed bundles alike. With no tap set it does nothing, which is
//! the normal case: no plugin loaded, and always on the log service.

use std::sync::{Arc, OnceLock, RwLock};

/// Called with each frame: its length varint, its 2-byte type, its body.
pub type Tap = Arc<dyn Fn(&[u8]) + Send + Sync>;

static TAP: OnceLock<RwLock<Option<Tap>>> = OnceLock::new();

/// Set (or, with `None`, clear) the tap.
pub fn set(tap: Option<Tap>) {
    let lock = TAP.get_or_init(|| RwLock::new(None));
    if let Ok(mut slot) = lock.write() {
        *slot = tap;
    }
}

/// Hand one frame to the tap, if one is set.
#[inline]
pub fn frame(packet: &[u8]) {
    let Some(lock) = TAP.get() else { return };
    let tap = match lock.read() {
        Ok(slot) => slot.clone(),
        Err(_) => return,
    };
    if let Some(tap) = tap {
        tap(packet);
    }
}
