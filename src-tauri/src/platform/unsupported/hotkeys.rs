/// There are no hotkeys here to turn on.
pub const DEFAULT_ON: bool = false;

/// No global hotkeys on this platform.
pub struct HotkeyManager;

impl HotkeyManager {
    pub fn new() -> Self {
        Self
    }
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: impl Fn() + Send + 'static,
        _: impl Fn() + Send + 'static,
        _: impl Fn() + Send + 'static,
        _: impl Fn(&'static str, bool) + Send + 'static,
    ) {
    }
    pub fn stop(&self) {}
}

/// The hotkeys helper (`a2tools-hotkeys`) is for Linux, where the portal
/// cannot be asked from the meter itself; here it has nothing to do.
pub fn run_helper() -> i32 {
    println!("error the hotkeys helper is only used on Linux");
    2
}
