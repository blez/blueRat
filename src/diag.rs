//! Self-diagnosis for the "device is connected but audio still goes to the
//! speakers" case — by far the most common way Bluetooth audio fails, and the
//! hardest to reason about from the outside, because every layer reports
//! success: the device pairs, connects, and simply never grows a sink.
//!
//! The checks here mirror the order you'd debug it by hand: is the daemon up,
//! does the audio backend exist, can this kernel carry LE Audio at all, and
//! finally what does this one device actually offer.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::audio;
use crate::bt::{self, AddrKind, AudioKind};

/// Where distributions install PipeWire's bluez backend. Its absence is a
/// silent killer: PipeWire runs fine and simply never builds a bluez card.
const SPA_BLUEZ_PATHS: [&str; 5] = [
    "/usr/lib/x86_64-linux-gnu/spa-0.2/bluez5/libspa-bluez5.so",
    "/usr/lib/aarch64-linux-gnu/spa-0.2/bluez5/libspa-bluez5.so",
    "/usr/lib/spa-0.2/bluez5/libspa-bluez5.so",
    "/usr/lib64/spa-0.2/bluez5/libspa-bluez5.so",
    "/usr/local/lib/spa-0.2/bluez5/libspa-bluez5.so",
];

const AF_BLUETOOTH: i32 = 31;
const SOCK_SEQPACKET: i32 = 5;
/// BTPROTO_ISO — the isochronous transport every LE Audio stream rides on.
const BTPROTO_ISO: i32 = 6;

unsafe extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn close(fd: i32) -> i32;
}

/// Can this kernel open an ISO socket? Enabling LE Audio in
/// `/etc/bluetooth/main.conf` is not enough — the kernel must also accept the
/// socket family, and several shipping kernels enable the feature flag while
/// still refusing it. Probing is the only trustworthy answer.
pub fn iso_socket_available() -> bool {
    // Creating and immediately closing an unbound socket touches no hardware.
    unsafe {
        let fd = socket(AF_BLUETOOTH, SOCK_SEQPACKET, BTPROTO_ISO);
        if fd < 0 {
            return false;
        }
        close(fd);
        true
    }
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn spa_bluez_plugin() -> Option<&'static str> {
    SPA_BLUEZ_PATHS.into_iter().find(|p| Path::new(p).exists())
}

/// Does PulseAudio proper have its bluez module loaded?
fn pulse_bluez_module() -> bool {
    Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("bluez5"))
        .unwrap_or(false)
}

/// One line of the report: a status glyph, a label, a value, and an optional
/// indented remedy.
struct Check {
    ok: Status,
    label: &'static str,
    value: String,
    hint: Option<String>,
}

#[derive(PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Check {
    fn ok(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            ok: Status::Ok,
            label,
            value: value.into(),
            hint: None,
        }
    }
    fn warn(label: &'static str, value: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            ok: Status::Warn,
            label,
            value: value.into(),
            hint: Some(hint.into()),
        }
    }
    fn fail(label: &'static str, value: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            ok: Status::Fail,
            label,
            value: value.into(),
            hint: Some(hint.into()),
        }
    }

    fn render(&self, into: &mut String) {
        let glyph = match self.ok {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        };
        into.push_str(&format!("  {glyph} {:<20} {}\n", self.label, self.value));
        if let Some(hint) = &self.hint {
            for line in hint.lines() {
                into.push_str(&format!("      → {line}\n"));
            }
        }
    }
}

fn host_checks() -> Vec<Check> {
    let mut checks = Vec::new();

    match Command::new("bluetoothctl").arg("show").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            checks.push(Check::ok("bluetoothd", "running"));
            if text.contains("Powered: yes") {
                checks.push(Check::ok("adapter", "powered on"));
            } else {
                checks.push(Check::fail(
                    "adapter",
                    "powered off",
                    "Press r to power it on, or check `rfkill list`.",
                ));
            }
        }
        _ => checks.push(Check::fail(
            "bluetoothd",
            "not reachable",
            "Start it: systemctl start bluetooth",
        )),
    }

    if !have("pactl") {
        checks.push(Check::fail(
            "sound server",
            "pactl not found",
            "Install pulseaudio-utils (or pipewire-pulse). Without it\n\
             blueRat cannot route audio at all.",
        ));
        return checks;
    }

    let server = audio::server_name();
    match &server {
        Some(name) => checks.push(Check::ok("sound server", name.clone())),
        None => checks.push(Check::fail(
            "sound server",
            "not responding",
            "pactl is installed but no server answered. Is the user\n\
             session running (systemctl --user status pipewire)?",
        )),
    }

    // The bluez backend is what turns a connected device into a card+sink.
    let pipewire = server.as_deref().is_some_and(|s| s.contains("PipeWire"));
    match spa_bluez_plugin() {
        Some(path) => checks.push(Check::ok("bluez audio backend", path)),
        None if pipewire => checks.push(Check::warn(
            "bluez audio backend",
            "libspa-bluez5.so not found",
            "PipeWire is running but its Bluetooth plugin is missing from\n\
             the usual paths. Install libspa-0.2-bluetooth (Debian/Ubuntu),\n\
             pipewire-audio (Fedora) or pipewire (Arch).",
        )),
        None if pulse_bluez_module() => {
            checks.push(Check::ok("bluez audio backend", "module-bluez5 loaded"))
        }
        None => checks.push(Check::warn(
            "bluez audio backend",
            "not detected",
            "No bluez audio module found. Bluetooth devices will connect\n\
             but never produce a sink.",
        )),
    }

    if iso_socket_available() {
        checks.push(Check::ok("LE Audio (ISO)", "supported"));
    } else {
        checks.push(Check::warn(
            "LE Audio (ISO)",
            "kernel refuses ISO sockets",
            "Only affects LE-Audio-only devices; classic A2DP is unaffected.\n\
             Needs Experimental=true and KernelExperimental=true in\n\
             /etc/bluetooth/main.conf plus a kernel that allows ISO sockets\n\
             — many shipping kernels enable the flag and still refuse them.",
        ));
    }

    checks
}

fn device_checks(mac: &str) -> Vec<Check> {
    let mut checks = Vec::new();
    let info = match bt::info(mac) {
        Ok(i) => i,
        Err(e) => {
            checks.push(Check::fail("device", "cannot read state", e));
            return checks;
        }
    };

    match info.addr {
        AddrKind::Public => checks.push(Check::ok("address", "public")),
        AddrKind::Random => checks.push(Check::warn(
            "address",
            "random (LE only)",
            "A random address never carries classic audio. If this device\n\
             also has a classic address, pair that one instead.",
        )),
        AddrKind::Unknown => checks.push(Check::ok("address", "unknown")),
    }

    match info.audio {
        AudioKind::Classic => checks.push(Check::ok("audio profile", "A2DP (classic)")),
        AudioKind::LeAudioOnly if iso_socket_available() => {
            checks.push(Check::ok("audio profile", "LE Audio (BAP)"))
        }
        AudioKind::LeAudioOnly => checks.push(Check::fail(
            "audio profile",
            "LE Audio only",
            "This device offers no classic A2DP, and this system cannot\n\
             carry LE Audio — so it will connect but never play sound.\n\
             Put it in pairing mode and pair its classic address instead\n\
             (many earbuds expose a separate BR/EDR address).",
        )),
        AudioKind::None => checks.push(Check::warn(
            "audio profile",
            "none advertised",
            "Not an audio device — or its services haven't been probed yet.",
        )),
    }

    if info.connected {
        checks.push(Check::ok("connected", "yes"));
    } else {
        checks.push(Check::warn(
            "connected",
            "no",
            "Connect it first; cards and sinks only exist while connected.",
        ));
        return checks;
    }

    match audio::card_for(mac) {
        Some(card) => checks.push(Check::ok("audio card", card)),
        None if info.audio.routable() => checks.push(Check::fail(
            "audio card",
            "missing",
            "Connected, but the audio backend built no card for it. This is\n\
             the bluez-backend or LE-Audio problem above, not the device.",
        )),
        None => checks.push(Check::ok("audio card", "n/a (not an audio device)")),
    }

    match audio::sink_for(mac) {
        Some(sink) => checks.push(Check::ok("sink", sink)),
        None if info.audio.routable() => checks.push(Check::fail(
            "sink",
            "missing",
            "Nothing to route audio to. If a card exists but no sink does,\n\
             the selected profile failed to start — try `a` to switch it.",
        )),
        None => checks.push(Check::ok("sink", "n/a")),
    }

    if let Some(profile) = audio::active_profile(mac) {
        checks.push(Check::ok("active profile", profile));
    }

    checks
}

/// Full report. `device` is an optional (mac, name) to inspect on top of the
/// host-wide checks.
pub fn report(device: Option<(&str, &str)>) -> String {
    let mut out = String::from("Host\n");
    for c in host_checks() {
        c.render(&mut out);
    }
    if let Some((mac, name)) = device {
        out.push_str(&format!("\nDevice — {name} ({mac})\n"));
        for c in device_checks(mac) {
            c.render(&mut out);
        }
    }
    out
}

/// One-line explanation for a device that connected but produced no sink.
/// This is what turns the silent failure into something the user can act on.
pub fn routing_failure_hint(mac: &str, audio: AudioKind) -> String {
    if audio == AudioKind::LeAudioOnly && !iso_socket_available() {
        return "no audio sink: device is LE-Audio-only and this system has no \
                ISO support — pair its classic address instead (d for details)"
            .into();
    }
    if audio.routable() && audio::card_for(mac).is_none() {
        return "no audio sink: the audio backend built no card — bluez plugin \
                missing? (d for details)"
            .into();
    }
    "no audio sink appeared — audio stays on the current output (d for details)".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_probe_does_not_panic() {
        // Either answer is legitimate; the point is that the FFI call is safe
        // to make on any machine, including one with no adapter at all.
        let _ = iso_socket_available();
    }

    #[test]
    fn report_includes_host_section() {
        let text = report(None);
        assert!(text.starts_with("Host\n"));
        assert!(text.contains("LE Audio (ISO)"));
    }

    #[test]
    fn hint_calls_out_le_audio_devices() {
        // On a host with ISO support this device would be fine, so only assert
        // the branch that does not depend on the machine running the test.
        if !iso_socket_available() {
            let hint = routing_failure_hint("AA:BB:CC:DD:EE:FF", AudioKind::LeAudioOnly);
            assert!(hint.contains("classic address"));
        }
    }

    #[test]
    fn checks_render_with_hints() {
        let mut s = String::new();
        Check::fail("sink", "missing", "do a thing").render(&mut s);
        assert!(s.contains("✗ sink"));
        assert!(s.contains("→ do a thing"));
    }
}
