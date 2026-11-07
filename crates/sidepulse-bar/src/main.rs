//! `sidepulse-bar`: native SidePulse menu-bar app.

mod app;
mod led;
mod server;
mod session;
mod settings;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;

/// `log_status_bar`: `ISO-timestamp message` on stdout (launchd captures it).
pub fn log(message: &str) {
    println!(
        "{} {message}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
}

fn repeat(seconds: f64, f: fn(&mut app::App)) {
    let block = RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| app::with_app(f));
    let timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(seconds, true, &block) };
    // Allow the system to coalesce wakeups.
    timer.setTolerance(seconds * 0.1);
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    app::install(app::App::new(mtm));
    let socket = sidepulse_core::ipc::default_event_socket_path();
    match server::start(&socket, |provider, line| {
        dispatch2::DispatchQueue::main()
            .exec_async(move || app::with_app(|a| a.on_event(&provider, &line)));
    }) {
        Ok((path, _)) => log(&format!("listening on {}", path.display())),
        Err(e) => log(&format!(
            "could not start event socket {}: {e}",
            socket.display()
        )),
    }

    app::with_app(|a| {
        a.refresh();
        a.poll_lid();
    });
    repeat(15.0, app::App::refresh);
    repeat(1.0, app::App::poll_lid);
    repeat(2.0, app::App::poll_devices);
    repeat(2.0, app::App::flush_state);

    // Release power assertions and flush state on SIGTERM/SIGINT (launchd bootout, Ctrl-C).
    for sig in [libc::SIGTERM, libc::SIGINT] {
        unsafe { libc::signal(sig, on_signal as libc::sighandler_t) };
    }
    repeat(1.0, |a| {
        if STOP.load(Ordering::Relaxed) {
            a.shutdown();
            std::process::exit(0);
        }
    });
    application.run();
}
