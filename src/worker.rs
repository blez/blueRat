//! Background worker: runs all blocking bluetoothctl/pactl operations so the
//! UI thread never freezes.

use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::bt::{self, AudioKind, Device, Discovered, PairEvent};
use crate::{audio, diag};

pub enum Cmd {
    Refresh,
    /// Idle-cadence relist. Unlike `Refresh` it must never touch adapter
    /// power: it fires unattended, and re-powering a radio the user switched
    /// off elsewhere would override their intent every few seconds.
    AutoRefresh,
    ToggleConnect {
        mac: String,
        name: String,
    },
    Scan,
    PairConnect {
        /// Every address discovered under this name. Earbuds advertise
        /// several at once and only some carry audio, so the worker ranks
        /// them and works down the list instead of trusting the user to
        /// have picked the right row.
        candidates: Vec<String>,
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
    ToggleProfile {
        mac: String,
        name: String,
    },
    Details {
        mac: String,
        name: String,
    },
    /// Host-wide (and optionally per-device) audio-routing diagnostics.
    Doctor {
        device: Option<(String, String)>,
    },
    /// The user's answer to a pairing agent prompt ("yes"/"no"/PIN). Consumed
    /// inside a running PairConnect; meaningless on its own.
    PairReply(String),
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
    /// A scan began: open the (empty) picker; devices stream in as Found.
    ScanStarted,
    /// One device discovered mid-scan.
    Found(Discovered),
    /// Authoritative discovered list once the scan finished.
    ScanResults(Vec<Discovered>),
    /// A finished event for the log.
    Status(String, Tone),
    /// Live narration of the running operation ("Connecting X…",
    /// "Waiting for audio sink… (3/8)"). Shown on the spinner line and
    /// replaced by the next progress message — never logged.
    Progress(String),
    /// A pairing agent prompt: pin=true asks for text entry, otherwise it's
    /// a yes/no passkey confirmation.
    PairPrompt {
        pin: bool,
        passkey: String,
    },
    /// Raw device info for the details popup.
    Details {
        name: String,
        text: String,
    },
    OpDone,
}

pub fn spawn(rx: Receiver<Cmd>, tx: Sender<Msg>, audio_available: bool) {
    thread::spawn(move || {
        // Sends fail only when the UI is gone; then we just stop.
        while let Ok(cmd) = rx.recv() {
            // A stray reply with no pairing in progress is a no-op and must
            // not produce an OpDone (the app doesn't count it as pending).
            if matches!(cmd, Cmd::PairReply(_)) {
                continue;
            }
            let ok = run(cmd, &rx, &tx, audio_available);
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
fn run(cmd: Cmd, rx: &Receiver<Cmd>, tx: &Sender<Msg>, audio_available: bool) -> bool {
    let status = |s: String, t: Tone| tx.send(Msg::Status(s, t)).is_ok();
    let progress = |s: String| {
        let _ = tx.send(Msg::Progress(s));
    };
    // Routing failure used to be silent, which is exactly the confusing case:
    // the device connects, nothing is said, and sound keeps coming out of the
    // speakers. Always report the outcome, and explain a failure.
    let route = |mac: &str, kind: AudioKind| {
        if !audio_available {
            let _ = tx.send(Msg::Status(
                "Connected, but audio cannot be routed ('pactl' not found)".into(),
                Tone::Warn,
            ));
            return;
        }
        // An LE-Audio-only device cannot grow a sink on a host whose kernel
        // refuses ISO sockets. Say so now rather than after an 8s poll that
        // can only fail.
        if kind == AudioKind::LeAudioOnly && !diag::iso_socket_available() {
            let _ = tx.send(Msg::Status(
                diag::routing_failure_hint(mac, kind),
                Tone::Warn,
            ));
            return;
        }
        if audio::route_audio(mac, |s| {
            let _ = tx.send(Msg::Progress(s));
        }) {
            let _ = tx.send(Msg::Status("Audio routed".into(), Tone::Ok));
        } else {
            let _ = tx.send(Msg::Status(
                diag::routing_failure_hint(mac, kind),
                Tone::Warn,
            ));
        }
    };

    match cmd {
        Cmd::PairReply(_) => {} // handled inside PairConnect; ignore here
        Cmd::Refresh => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
            }
        }
        // The post-op relist in spawn() is the whole job.
        Cmd::AutoRefresh => {}
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
                    // Only audio devices grow a sink — don't make mice
                    // and keyboards wait through the 8s sink poll.
                    if info.audio.routable() {
                        route(&mac, info.audio);
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
            if tx.send(Msg::ScanStarted).is_err() {
                return false;
            }
            progress("Scanning ~10s — put the device in pairing mode…".into());
            // Seed the picker with devices already in bluetoothd's cache,
            // then stream new finds as the scan reports them. `seen` mirrors
            // what the picker shows, so a failed post-scan listing can still
            // hand back a usable list instead of wedging the picker.
            let mut seen: Vec<Discovered> = bt::discovered_unpaired().unwrap_or_default();
            // Address kinds decide which addresses belong to one physical
            // device, so they must be known before the picker groups rows.
            bt::fill_addr_kinds(&mut seen);
            for d in &seen {
                let _ = tx.send(Msg::Found(d.clone()));
            }
            bt::scan_live(10, |mut d| {
                match seen.iter_mut().find(|s| s.mac == d.mac) {
                    Some(s) => {
                        s.name = d.name.clone();
                        d.addr = s.addr;
                    }
                    None => {
                        // One info call per newly seen address, not per event.
                        bt::fill_addr_kinds(std::slice::from_mut(&mut d));
                        seen.push(d.clone());
                    }
                }
                let _ = tx.send(Msg::Found(d));
            });
            match bt::discovered_unpaired() {
                Ok(mut found) => {
                    if found.is_empty() {
                        status("Nothing new found".into(), Tone::Info);
                    }
                    bt::fill_addr_kinds(&mut found);
                    return tx.send(Msg::ScanResults(found)).is_ok();
                }
                Err(e) => {
                    status(format!("Scan listing failed: {e}"), Tone::Err);
                    return tx.send(Msg::ScanResults(seen)).is_ok();
                }
            }
        }
        Cmd::PairConnect {
            candidates,
            name,
            others,
        } => {
            if let Err(e) = bt::adapter_up() {
                status(e, Tone::Err);
                return true;
            }
            progress(format!("Pairing {name}…"));
            // Best address first: a classic A2DP one before an LE-only one.
            // Ranking costs an `info` call each, so it happens here and not
            // during the scan.
            let ranked = bt::rank_candidates(&candidates);
            let multi = ranked.len() > 1;

            // Set when the *user* declines a prompt (or the UI goes away), as
            // opposed to an address refusing us. Without the distinction, one
            // Esc would re-raise the same prompt for every remaining address.
            let mut aborted = false;
            let mut paired_mac = None;
            for (i, mac) in ranked.iter().enumerate() {
                if multi {
                    progress(format!(
                        "Pairing {name} — address {}/{}…",
                        i + 1,
                        ranked.len()
                    ));
                }
                // Interactive session so agent prompts (passkey/PIN) reach the
                // user; fall back to the one-shot pair if it can't start.
                let ok = match bt::pair_interactive(mac, |ev| {
                    let msg = match ev {
                        PairEvent::ConfirmPasskey(passkey) => Msg::PairPrompt {
                            pin: false,
                            passkey,
                        },
                        PairEvent::RequestPin => Msg::PairPrompt {
                            pin: true,
                            passkey: String::new(),
                        },
                    };
                    if tx.send(msg).is_err() {
                        aborted = true;
                        return None;
                    }
                    loop {
                        match rx.recv() {
                            Ok(Cmd::PairReply(ans)) => {
                                // Esc sends "" for a PIN and "no" for a
                                // yes/no prompt; either means "stop", not
                                // "this address said no".
                                if ans.is_empty() || ans == "no" {
                                    aborted = true;
                                }
                                return Some(ans);
                            }
                            Ok(_) => continue, // gated by busy; nothing else expected
                            Err(_) => {
                                aborted = true;
                                return None;
                            }
                        }
                    }
                }) {
                    Ok(ok) => ok,
                    Err(_) => bt::pair(mac),
                };
                if ok {
                    paired_mac = Some(mac.clone());
                    break;
                }
                if aborted {
                    break;
                }
                // A device that advertises several addresses usually only
                // accepts pairing on one of them; a rejection is expected, so
                // keep going instead of reporting failure.
                if multi && i + 1 < ranked.len() {
                    let _ = tx.send(Msg::Progress(format!(
                        "{name}: address {} refused, trying the next…",
                        i + 1
                    )));
                }
            }

            let Some(mac) = paired_mac else {
                if aborted {
                    status(format!("Pairing cancelled: {name}"), Tone::Info);
                } else {
                    status(
                        format!("Pairing failed: {name} — pick a device to retry, Esc to leave"),
                        Tone::Err,
                    );
                }
                // Reopen the picker with the same scan results so the user
                // doesn't have to sit through another 10s scan.
                return tx.send(Msg::ScanResults(others)).is_ok();
            };
            bt::trust(&mac);
            progress(format!("Connecting {name}…"));
            if bt::connect(&mac) {
                let kind = bt::info(&mac).map(|i| i.audio).unwrap_or_default();
                if kind.routable() {
                    route(&mac, kind);
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
        Cmd::ToggleProfile { mac, name } => {
            if !audio_available {
                status(
                    "pactl not available — cannot switch profiles".into(),
                    Tone::Err,
                );
                return true;
            }
            progress(format!("Switching audio profile of {name}…"));
            match audio::toggle_profile(&mac) {
                Ok(profile) => {
                    // Label from the profile that actually took effect —
                    // "a2dp" alone would call a BAP switch a headset switch.
                    let mode = audio::profile_label(&profile);
                    status(format!("{name}: switched to {mode}"), Tone::Ok);
                }
                Err(e) => {
                    status(format!("Profile switch failed: {e}"), Tone::Err);
                }
            }
        }
        Cmd::Details { mac, name } => match bt::info_raw(&mac) {
            Ok(text) => {
                let _ = tx.send(Msg::Details { name, text });
            }
            Err(e) => {
                status(format!("Cannot read info of {name}: {e}"), Tone::Err);
            }
        },
        Cmd::Doctor { device } => {
            progress("Running diagnostics…".into());
            let text = diag::report(
                device
                    .as_ref()
                    .map(|(mac, name)| (mac.as_str(), name.as_str())),
            );
            let _ = tx.send(Msg::Details {
                name: "Audio diagnostics".into(),
                text,
            });
        }
    }
    true
}
