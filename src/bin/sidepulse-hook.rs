//! Drop-in replacement for `python3 sidepulse/hook_entry.py --provider P --log PATH`.
//! Reads one hook payload from stdin and always exits 0 so agents are never blocked.

use std::io::Read;

fn main() {
    // Hooks must stay silent: a panic message on stderr would surface in the agent UI.
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _ = std::panic::catch_unwind(|| {
        let Some((provider, log_path)) = sidepulse::hook::parse_args(&args) else {
            return;
        };
        let mut input = Vec::new();
        if std::io::stdin().read_to_end(&mut input).is_err() {
            return;
        }
        sidepulse::hook::hook_log_main(&provider, &log_path, &input);
    });
    std::process::exit(0);
}
