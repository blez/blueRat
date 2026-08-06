//! Background worker: runs all blocking bluetoothctl/pactl operations so the
//! UI thread never freezes.

use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::audio;
use crate::bt::{self, Device, Discovered};

pub enum Cmd {
    Refresh,
    ToggleConnect {
        mac: String,
        name: String,
    },
    Scan,
    PairConnect {
        mac: String,
        name: String,
        /// The full scan-result list, so a pair failure can reopen the picker
        /// instead of forcing another 10s scan.
        others: Vec<Discovered>,
    },
    ToggleTrust {
        mac: String,
    },
    Remove {
        mac: String,
    },
}

/// Severity of a status message, so the UI colors by meaning instead of
/// sniffing message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Ok,
    Warn,
    Err,
}

pub enum Msg {
    Devices(Vec<Device>),
    ScanResults(Vec<Discovered>),
    /// A finished event for the log.
    Status(String, Tone),
    /// Live narration of the running operation ("Connecting X…",
    /// "Waiting for audio sink… (3/8)"). Shown on the spinner line and
    /// replaced by the next progress message — never logged.
    Progress(String),
    OpDone,
}

pub fn spawn(rx: Receiver<Cmd>, tx: Sender<Msg>, audio_available: bool) {
    thread::spawn(move || {
        // Sends fail only when the UI is gone; then we just stop.
        while let Ok(cmd) = rx.recv() {
            let ok = run(cmd, &tx, audio_available);
            // Refresh the list after every op; retry once so a transient
            // bluetoothd hiccup doesn't leave a stale list behind a
            // just-successful operation.
            let devices = bt::paired_devices().or_else(|_| {
                thread::sleep(Duration::from_millis(300));
                bt::paired_devices()
            });
            match devices {
                Ok(devices) => {
                    let _ = tx.send(Msg::Devices(devices));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Status(
                        format!("Device list may be stale: {e}"),
                        Tone::Warn,
                    ));
                }
            }
            let _ = tx.send(Msg::OpDone);
            if !ok {
                break;
            }
        }
    });
}

/// Returns false when the UI side hung up.
fn run(cmd: Cmd, tx: &Sender<Msg>, audio_available: bool) -> bool {
    let status = |s: String, t: Tone| tx.send(Msg::Status(s, t)).is_ok();
    let progress = |s: String| {
        let _ = tx.send(Msg::Progress(s));
    };
    let route = |mac: &str| {
        if audio_available
            && audio::route_audio(mac, |s| {
                let _ = tx.send(Msg::Progress(s));
            })
        {
            let _ = tx.send(Msg::Status("Audio routed".into(), Tone::Ok));
        }
    };

    match cmd {
        Cmd::Refresh => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
            }
        }
        Cmd::ToggleConnect { mac, name } => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
                return true;
            }
            // Query live state; an error must NOT be read as "disconnected",
            // or the toggle would run the opposite action.
            let info = match bt::info(&mac) {
                Ok(i) => i,
                Err(e) => {
                    status(format!("Cannot read state of {name}: {e}"), Tone::Err);
                    return true;
                }
            };
            if info.connected {
                progress(format!("Disconnecting {name}…"));
                if bt::disconnect(&mac) {
                    status(format!("Disconnected {name}"), Tone::Ok);
                } else {
                    status(format!("Failed to disconnect {name}"), Tone::Err);
                }
            } else {
                progress(format!("Connecting {name}…"));
                if bt::connect(&mac) {
                    // Only audio-sink devices grow a sink — don't make mice
                    // and keyboards wait through the 8s sink poll.
                    if info.audio {
                        route(&mac);
                    }
                    status(format!("Connected {name}"), Tone::Ok);
                } else {
                    status(
                        "Connect failed (is the device on / out of the case?)".into(),
                        Tone::Err,
                    );
                }
            }
        }
        Cmd::Scan => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
                return true;
            }
            progress("Scanning ~10s — put the device in pairing mode…".into());
            bt::scan(10);
            match bt::discovered_unpaired() {
                Ok(found) => {
                    if found.is_empty() {
                        status("Nothing new found".into(), Tone::Info);
                    }
                    return tx.send(Msg::ScanResults(found)).is_ok();
                }
                Err(e) => {
                    status(format!("Scan failed: {e}"), Tone::Err);
                }
            }
        }
        Cmd::PairConnect { mac, name, others } => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
                return true;
            }
            progress(format!("Pairing {name}…"));
            if !bt::pair(&mac) {
                status(
                    format!("Pairing failed: {name} — pick a device to retry, Esc to leave"),
                    Tone::Err,
                );
                // Reopen the picker with the same scan results so the user
                // doesn't have to sit through another 10s scan.
                return tx.send(Msg::ScanResults(others)).is_ok();
            }
            bt::trust(&mac);
            progress(format!("Connecting {name}…"));
            if bt::connect(&mac) {
                if bt::info(&mac).map(|i| i.audio).unwrap_or(false) {
                    route(&mac);
                }
                status(format!("Paired & connected {name}"), Tone::Ok);
            } else {
                status(
                    format!("Paired {name}, but connect failed — press Enter on it to retry"),
                    Tone::Warn,
                );
            }
        }
        Cmd::ToggleTrust { mac } => {
            // Same live-state rule as ToggleConnect.
            let info = match bt::info(&mac) {
                Ok(i) => i,
                Err(e) => {
                    status(format!("Cannot read state of {mac}: {e}"), Tone::Err);
                    return true;
                }
            };
            if info.trusted {
                if bt::untrust(&mac) {
                    status(format!("Untrusted {mac}"), Tone::Ok);
                } else {
                    status(format!("Failed to untrust {mac}"), Tone::Err);
                }
            } else if bt::trust(&mac) {
                status(format!("Trusted {mac}"), Tone::Ok);
            } else {
                status(format!("Failed to trust {mac}"), Tone::Err);
            }
        }
        Cmd::Remove { mac } => {
            if bt::remove(&mac) {
                status("Removed".into(), Tone::Ok);
            } else {
                status(format!("Failed to remove {mac}"), Tone::Err);
            }
        }
    }
    true
}
