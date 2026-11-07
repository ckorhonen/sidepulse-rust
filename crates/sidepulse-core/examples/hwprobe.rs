//! Read-only hardware probe: battery, lid, mounted devices and model watts, with per-read timing.
//! Writes nothing. Run with `cargo run -p sidepulse-core --example hwprobe`.

use std::time::Instant;

use sidepulse_core::{battery, devices, lid};

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    println!("[{label}: {} us]", start.elapsed().as_micros());
    value
}

fn main() {
    let watts = timed(
        "default_full_charge_watts (first)",
        battery::default_full_charge_watts,
    );
    println!("model full-charge watts: {watts}");
    timed(
        "default_full_charge_watts (cached)",
        battery::default_full_charge_watts,
    );

    let snapshot = timed("battery::read (first)", battery::read);
    timed("battery::read (warm)", battery::read);
    match &snapshot {
        Some(s) => {
            println!(
                "battery: {}% {} plugged={} charged={} {:.2}V {:.3}A ({:+.1}W) cycles={} health={}%",
                s.percent,
                s.state_label(),
                s.is_plugged,
                s.is_charged,
                s.voltage,
                s.amperage,
                s.battery_watts,
                s.cycle_count,
                s.health_percent
            );
            println!(
                "adapter: {:.1}W (rated {}W, {:.1}V x {:.2}A, {} PD profiles) ratio={:.3}",
                s.adapter_power(),
                s.adapter_watts,
                s.adapter_voltage,
                s.adapter_current,
                s.pd_profiles.len(),
                s.charge_speed_ratio(None)
            );
            println!(
                "battery program (8 LEDs):\n{}",
                battery::battery_program(s, 8, None, 255)
            );
        }
        None => println!("battery: none"),
    }

    let closed = timed("lid::clamshell_closed (first)", lid::clamshell_closed);
    timed("lid::clamshell_closed (warm)", lid::clamshell_closed);
    println!("lid closed: {closed:?}");
    let disabled = timed("lid::sleep_disabled", lid::sleep_disabled);
    println!(
        "sleep disabled: {disabled:?}; helper installed: {}",
        lid::sleep_helper_installed()
    );

    let found = timed("devices::discover", devices::discover);
    if found.is_empty() {
        println!("devices: none");
    }
    for d in &found {
        println!(
            "device: {} ({} LEDs, {:?}) -> {}",
            d.name,
            d.led_count,
            d.reason,
            d.target().display()
        );
    }
}
