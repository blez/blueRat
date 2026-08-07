//! bluerat — a small ratatui Bluetooth manager wrapping bluetoothctl/pactl.

mod app;
mod audio;
mod bt;
mod diag;
mod ui;
mod worker;

use std::io::ErrorKind;
use std::process::{Command, Stdio, exit};
use std::sync::mpsc;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::App;
use worker::{Cmd, Msg};

fn have(bin: &str) -> bool {
    !matches!(
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Err(e) if e.kind() == ErrorKind::NotFound
    )
}

/// Six colon- or dash-separated hex octets.
fn looks_like_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

const USAGE: &str = "\
bluerat — TUI Bluetooth manager for Linux

USAGE:
    bluerat [OPTIONS]

OPTIONS:
    --doctor [MAC]  print Bluetooth audio diagnostics and exit; with a MAC,
                    also check that one device
    -h, --help      show this help
    -V, --version   show the version
";

fn main() {
    // Flags are handled before anything else so --help/--doctor work even
    // when the environment is too broken to start the UI. Every flag exits,
    // so only the first one is meaningful.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--doctor" => {
                if !have("bluetoothctl") {
                    eprintln!("error: 'bluetoothctl' not found — install the 'bluez' package");
                    exit(1);
                }
                // Without this, `--doctor --help` diagnoses a device called
                // "--help" instead of printing usage.
                let mac = match std::env::args().nth(2) {
                    Some(a) if a.starts_with('-') => {
                        eprintln!("error: --doctor takes a MAC address, not '{a}'\n\n{USAGE}");
                        exit(2);
                    }
                    Some(a) if !looks_like_mac(&a) => {
                        eprintln!("error: '{a}' is not a MAC address (expected AA:BB:CC:DD:EE:FF)");
                        exit(2);
                    }
                    other => other,
                };
                print!(
                    "{}",
                    diag::report(mac.as_deref().map(|m| (m, "selected device")))
                );
                exit(0);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                exit(0);
            }
            "-V" | "--version" => {
                println!(concat!("bluerat ", env!("CARGO_PKG_VERSION")));
                exit(0);
            }
            other => {
                eprintln!("error: unknown argument '{other}'\n\n{USAGE}");
                exit(2);
            }
        }
    }

    // Check external tools before touching the terminal, so messages stay readable.
    if !have("bluetoothctl") {
        eprintln!("error: 'bluetoothctl' not found — install the 'bluez' package");
        exit(1);
    }
    let audio_available = have("pactl");
    if !audio_available {
        eprintln!(
            "warning: audio auto-routing disabled: 'pactl' not found — install 'pulseaudio-utils' or pipewire-pulse"
        );
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    worker::spawn(cmd_rx, msg_tx, audio_available);

    let mut app = App::new();
    app.push_log("Initializing Bluetooth…".into(), worker::Tone::Info);
    if !audio_available {
        app.push_log(
            "audio auto-routing disabled ('pactl' not found)".into(),
            worker::Tone::Warn,
        );
    }

    // Initial load also powers the adapter on if needed.
    app.begin("Loading…");
    app.pending = 1;
    let _ = cmd_tx.send(Cmd::Refresh);

    // Silent list refresh cadence while idle, so external connects and
    // disconnects (headphones taken out of the case) show up on their own.
    const AUTO_REFRESH: Duration = Duration::from_secs(8);

    let mut terminal = ratatui::init();
    while !app.quit {
        if event::poll(Duration::from_millis(100)).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
            && let Some(cmd) = app.handle_key(key)
        {
            // Pair replies are consumed inside the running pairing op and
            // don't produce their own OpDone.
            if !matches!(cmd, worker::Cmd::PairReply(_)) {
                app.pending += 1;
            }
            let _ = cmd_tx.send(cmd);
        }
        while let Ok(msg) = msg_rx.try_recv() {
            app.handle_msg(msg);
        }
        app.on_tick();
        if app.pending == 0 && app.last_done.elapsed() >= AUTO_REFRESH {
            // Silent relist: no busy label, keys stay live, and — unlike a
            // user Refresh — no adapter power-on, so a radio the user turned
            // off elsewhere stays off.
            app.pending += 1;
            let _ = cmd_tx.send(Cmd::AutoRefresh);
        }
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            bt::kill_running_child();
            ratatui::restore();
            eprintln!("draw error: {e}");
            exit(1);
        }
    }
    // Don't orphan a blocking bluetoothctl call (e.g. a 10s scan that would
    // keep the adapter in discovery mode after we're gone).
    bt::kill_running_child();
    ratatui::restore();
}
