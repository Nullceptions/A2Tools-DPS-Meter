//! `a2tools-hotkeys`: the meter's global hotkeys on Linux, in a process of
//! their own (see `platform::linux::hotkeys`). The meter starts it; it is not
//! meant to be run by hand.

fn main() {
    std::process::exit(a2tools_dps_meter_lib::platform::hotkeys::run_helper());
}
