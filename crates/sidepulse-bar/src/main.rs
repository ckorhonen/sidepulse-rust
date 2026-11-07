//! `sidepulse-bar`: native SidePulse menu-bar app.
//!
//! Fully event-driven: hook events arrive on the socket thread, the lid and power source through
//! IOKit notifications, volumes through `NSWorkspace` notifications and signals through dispatch
//! sources. The only timers are one-shots for known deadlines (see `App::schedule_next_refresh`)
//! and a coarse safety refresh.

mod app;
mod led;
mod server;
mod session;
mod settings;
mod ui;

use std::ptr::NonNull;

use block2::RcBlock;
use dispatch2::{DispatchObject, DispatchQueue, DispatchRetained, DispatchSource};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSWorkspace, NSWorkspaceDidMountNotification,
    NSWorkspaceDidRenameVolumeNotification, NSWorkspaceDidUnmountNotification,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSOperationQueue, NSTimer};

use sidepulse_core::battery::PowerSourceWatch;
use sidepulse_core::lid::{self, ClamshellWatch};

/// `log_status_bar`: `ISO-timestamp message` on stdout (launchd captures it).
pub fn log(message: &str) {
    println!(
        "{} {message}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
}

/// A repeating fallback timer, used only where no notification is available.
fn repeat(seconds: f64, tolerance: f64, f: fn(&mut app::App)) -> Retained<NSTimer> {
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| app::with_app(f));
    // SAFETY: called on the main thread; the timer copies the block and is scheduled on the main
    // run loop.
    let timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(seconds, true, &block) };
    timer.setTolerance(tolerance);
    timer
}

/// Fallback lid poll when clamshell notifications can't be registered (and a lid exists).
const LID_FALLBACK_POLL: f64 = 5.0;
/// Device poll under `SIDEPULSE_VOLUMES`, whose fake mounts post no notifications.
const VOLUMES_OVERRIDE_POLL: f64 = 5.0;
/// Second rescan after a mount notification, in case the device's files appear a moment later.
const MOUNT_RESCAN_DELAY: f64 = 2.0;

/// Things that must live as long as the app: dropping them would unregister the notification.
#[derive(Default)]
struct Watches {
    _lid: Option<ClamshellWatch>,
    _power: Option<PowerSourceWatch>,
    _volume_observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    _signals: Vec<DispatchRetained<DispatchSource>>,
    _timers: Vec<Retained<NSTimer>>,
}

/// Lid changes via `kIOPMMessageClamshellStateChange` instead of the 1 s `pollLid:` timer.
fn watch_lid(watches: &mut Watches) {
    app::with_app(app::App::poll_lid); // baseline reading
    watches._lid = ClamshellWatch::new(|| app::with_app(app::App::poll_lid));
    if watches._lid.is_none() && lid::clamshell_closed().is_some() {
        log("clamshell notifications unavailable; polling the lid");
        watches._timers.push(repeat(
            LID_FALLBACK_POLL,
            LID_FALLBACK_POLL * 0.2,
            app::App::poll_lid,
        ));
    }
}

/// Plug, unplug and charge changes refresh the battery display (was: the 15 s refresh).
fn watch_power(watches: &mut Watches) {
    watches._power = PowerSourceWatch::new(|| app::with_app(app::App::refresh));
    if watches._power.is_none() {
        log("power-source notifications unavailable; battery updates on the safety refresh");
    }
}

/// Volume mount, unmount and rename notifications trigger a rescan (was: a 2 s `/Volumes` poll).
fn watch_volumes(watches: &mut Watches) {
    if std::env::var_os("SIDEPULSE_VOLUMES").is_some() {
        watches._timers.push(repeat(
            VOLUMES_OVERRIDE_POLL,
            VOLUMES_OVERRIDE_POLL * 0.2,
            app::App::rescan_devices,
        ));
        return;
    }
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let main_queue = NSOperationQueue::mainQueue();
    let block = RcBlock::new(|_note: NonNull<NSNotification>| {
        app::with_app(app::App::rescan_devices);
        DispatchQueue::main()
            .after(app::dispatch_time(MOUNT_RESCAN_DELAY), || {
                app::with_app(app::App::rescan_devices)
            })
            .ok();
    });
    // SAFETY: the notification names are AppKit's static constants; the block is copied by the
    // center and runs on the main operation queue (main thread), where the app lives.
    for name in unsafe {
        [
            NSWorkspaceDidMountNotification,
            NSWorkspaceDidUnmountNotification,
            NSWorkspaceDidRenameVolumeNotification,
        ]
    } {
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(&main_queue),
                &block,
            )
        };
        watches._volume_observers.push(observer);
    }
}

/// Release power assertions and flush state on SIGTERM/SIGINT (launchd bootout, Ctrl-C), via a
/// dispatch signal source on the main queue (was: a 1 s timer checking a flag).
fn watch_signals(watches: &mut Watches) {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: ignoring the default disposition is required for a dispatch signal source to
        // receive the signal instead of the process terminating. The source type is libdispatch's
        // static, the handle is a valid signal number and the target queue is the main queue.
        let source = unsafe {
            libc::signal(sig, libc::SIG_IGN);
            DispatchSource::new(
                &raw const dispatch2::_dispatch_source_type_signal as *mut _,
                sig as usize,
                0,
                Some(DispatchQueue::main()),
            )
        };
        let handler = RcBlock::new(|| {
            app::with_app(app::App::shutdown);
            std::process::exit(0);
        });
        // SAFETY: libdispatch copies the block; the source is retained in `watches`.
        unsafe { source.set_event_handler_with_block(RcBlock::as_ptr(&handler)) };
        source.activate();
        watches._signals.push(source);
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    app::install(app::App::new(mtm));
    let socket = sidepulse_core::ipc::default_event_socket_path();
    match server::start(&socket, |provider, line| {
        DispatchQueue::main().exec_async(move || app::with_app(|a| a.on_event(&provider, &line)));
    }) {
        Ok((path, _)) => log(&format!("listening on {}", path.display())),
        Err(e) => log(&format!(
            "could not start event socket {}: {e}",
            socket.display()
        )),
    }

    let mut watches = Watches::default();
    watch_signals(&mut watches);
    app::with_app(|a| {
        a.refresh();
        a.schedule_flush();
    });
    watch_lid(&mut watches);
    watch_power(&mut watches);
    watch_volumes(&mut watches);

    application.run();
    drop(watches);
}
