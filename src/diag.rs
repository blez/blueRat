//! Self-diagnosis for the "device is connected but audio still goes to the
//! speakers" case — by far the most common way Bluetooth audio fails, and the
//! hardest to reason about from the outside, because every layer reports
//! success: the device pairs, connects, and simply never grows a sink.
//!
//! The checks here mirror the order you'd debug it by hand: is the daemon up,
//! does the audio backend exist, can this kernel carry LE Audio at all, and
//! finally what does this one device actually offer.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio;
use crate::bt::{self, AddrKind, AudioKind};

/// Library roots to search for PipeWire's bluez backend. Every immediate
/// subdirectory is searched too, which covers all multiarch triplets without
/// enumerating them. Its absence is a silent killer: PipeWire runs fine and
/// simply never builds a bluez card.
const LIB_ROOTS: [&str; 5] = [
    "/usr/lib",
    "/usr/lib64",
    "/usr/local/lib",
    "/usr/local/lib64",
    "/run/current-system/sw/lib",
];

/// Path of the plugin relative to a library root.
const SPA_BLUEZ_SUFFIX: &str = "spa-0.2/bluez5/libspa-bluez5.so";

/// A wedged bluetoothd (D-Bus not answering) makes `bluetoothctl show` block
/// for ~20 minutes. A diagnostic that hangs is worse than one that reports the
/// hang, so every probe here is bounded.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Run a command, killing it if it outlives `PROBE_TIMEOUT`. Returns None if
/// it could not be started, timed out, or exited non-zero. Output is small
/// enough here that the pipe cannot fill while we poll.
fn probe(bin: &str, args: &[&str]) -> Option<String> {
    probe_within(bin, args, PROBE_TIMEOUT)
}

fn probe_within(bin: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out: Output = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            // Timed out, or the wait itself failed.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Search the library roots (and their immediate subdirectories, which is
/// where multiarch triplets live) for PipeWire's bluez plugin. A hardcoded
/// path list reports "not installed" on any layout it doesn't know about,
/// which is exactly the wrong answer from a diagnostic.
fn spa_bluez_plugin() -> Option<String> {
    // PipeWire's own override wins when it is set.
    if let Ok(dir) = std::env::var("SPA_PLUGIN_DIR") {
        let p = Path::new(&dir).join("bluez5/libspa-bluez5.so");
        if p.exists() {
            return Some(p.display().to_string());
        }
    }
    for root in LIB_ROOTS {
        let root = Path::new(root);
        let direct = root.join(SPA_BLUEZ_SUFFIX);
        if direct.exists() {
            return Some(direct.display().to_string());
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut found: Option<PathBuf> = None;
        for e in entries.flatten() {
            let candidate = e.path().join(SPA_BLUEZ_SUFFIX);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        if let Some(p) = found {
            return Some(p.display().to_string());
        }
    }
    None
}

/// Does PulseAudio proper have its bluez module loaded?
fn pulse_bluez_module() -> bool {
    probe("pactl", &["list", "short", "modules"]).is_some_and(|o| o.contains("bluez5"))
}

/// Is a bluez card present right now? The one unambiguous positive: whatever
/// the file layout, a card can only exist if the backend is working.
fn bluez_card_present() -> bool {
    probe("pactl", &["list", "short", "cards"]).is_some_and(|o| o.contains("bluez"))
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

/// Everything the host report needs from the outside world. Gathering is
/// separated from judging so the report can be exercised without a Bluetooth
/// stack, a sound server, or a 20-minute wait on a wedged daemon.
struct HostProbe {
    /// `bluetoothctl show` output; None when it failed or timed out.
    show: Option<String>,
    pactl: bool,
    server: Option<String>,
    bluez_card: bool,
    spa_plugin: Option<String>,
    pulse_module: bool,
    iso: bool,
}

fn probe_host() -> HostProbe {
    let show = probe("bluetoothctl", &["show"]);
    let pactl = have("pactl");
    // Skip the sound-server questions entirely when pactl is missing: every
    // one of them would just be a slower way of saying so.
    let (server, bluez_card, pulse_module) = if pactl {
        (audio::server_name(), bluez_card_present(), {
            // Only meaningful as a fallback; probing costs a subprocess.
            pulse_bluez_module()
        })
    } else {
        (None, false, false)
    };
    HostProbe {
        show,
        pactl,
        server,
        bluez_card,
        spa_plugin: spa_bluez_plugin(),
        pulse_module,
        iso: iso_socket_available(),
    }
}

fn host_checks(p: &HostProbe) -> Vec<Check> {
    let mut checks = Vec::new();

    match &p.show {
        Some(text) => {
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
        None => checks.push(Check::fail(
            "bluetoothd",
            "not reachable",
            "Start it: systemctl start bluetooth\n\
             (a daemon that is up but not answering D-Bus looks the same here)",
        )),
    }

    if !p.pactl {
        checks.push(Check::fail(
            "sound server",
            "pactl not found",
            "Install pulseaudio-utils (or pipewire-pulse). Without it\n\
             blueRat cannot route audio at all.",
        ));
    } else {
        match &p.server {
            Some(name) => checks.push(Check::ok("sound server", name.clone())),
            None => checks.push(Check::fail(
                "sound server",
                "not responding",
                "pactl is installed but no server answered. Is the user\n\
                 session running (systemctl --user status pipewire)?",
            )),
        }

        // The bluez backend is what turns a connected device into a card+sink.
        // An existing card proves it works whatever the file layout says.
        let pipewire = p.server.as_deref().is_some_and(|s| s.contains("PipeWire"));
        if p.bluez_card {
            checks.push(Check::ok(
                "bluez audio backend",
                "active (bluez card present)",
            ));
        } else if p.pulse_module {
            checks.push(Check::ok("bluez audio backend", "module-bluez5 loaded"));
        } else if let Some(path) = &p.spa_plugin {
            checks.push(Check::ok("bluez audio backend", path.clone()));
        } else if pipewire {
            checks.push(Check::warn(
                "bluez audio backend",
                "libspa-bluez5.so not found",
                "PipeWire is running but its Bluetooth plugin was not found.\n\
                 Install libspa-0.2-bluetooth (Debian/Ubuntu),\n\
                 pipewire-audio (Fedora) or pipewire (Arch).\n\
                 If it is installed somewhere unusual, set SPA_PLUGIN_DIR.",
            ));
        } else {
            checks.push(Check::warn(
                "bluez audio backend",
                "not detected",
                "No bluez audio module found. Bluetooth devices will connect\n\
                 but never produce a sink.",
            ));
        }
    }

    if p.iso {
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
    for c in host_checks(&probe_host()) {
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
    // The sink exists, so routing failed at the last step rather than never
    // having anything to route to — a very different thing to go fix.
    if audio::sink_for(mac).is_some() {
        return "sink exists but could not be made the default output — another \
                tool may be managing it (d for details)"
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

    fn healthy_probe() -> HostProbe {
        HostProbe {
            show: Some("Controller 00:11:22:33:44:55\n\tPowered: yes\n".into()),
            pactl: true,
            server: Some("PulseAudio (on PipeWire 1.0.5)".into()),
            bluez_card: true,
            spa_plugin: None,
            pulse_module: false,
            iso: true,
        }
    }

    fn render(checks: Vec<Check>) -> String {
        let mut s = String::new();
        for c in checks {
            c.render(&mut s);
        }
        s
    }

    #[test]
    fn healthy_host_reports_every_section() {
        let text = render(host_checks(&healthy_probe()));
        assert!(text.contains("✓ bluetoothd"));
        assert!(text.contains("✓ adapter"));
        assert!(text.contains("✓ sound server"));
        assert!(text.contains("✓ bluez audio backend"));
        assert!(text.contains("✓ LE Audio (ISO)"));
    }

    #[test]
    fn missing_pactl_still_reports_the_kernel_check() {
        // The early return this replaced skipped every later check, so a host
        // without pactl never learned anything about its kernel.
        let probe = HostProbe {
            pactl: false,
            server: None,
            bluez_card: false,
            iso: false,
            ..healthy_probe()
        };
        let text = render(host_checks(&probe));
        assert!(text.contains("✗ sound server"));
        assert!(text.contains("pactl not found"));
        assert!(text.contains("LE Audio (ISO)"));
    }

    #[test]
    fn unreachable_daemon_is_a_failure_not_a_hang() {
        let probe = HostProbe {
            show: None,
            ..healthy_probe()
        };
        let text = render(host_checks(&probe));
        assert!(text.contains("✗ bluetoothd"));
        assert!(text.contains("not reachable"));
    }

    #[test]
    fn an_existing_card_outranks_a_missing_plugin_file() {
        // The plugin lives somewhere the path search doesn't know, but a card
        // exists — the backend demonstrably works, so don't tell the user to
        // install what they already have.
        let probe = HostProbe {
            spa_plugin: None,
            bluez_card: true,
            ..healthy_probe()
        };
        let text = render(host_checks(&probe));
        assert!(text.contains("✓ bluez audio backend"));
        assert!(!text.contains("libspa-0.2-bluetooth"));
    }

    #[test]
    fn pipewire_without_any_backend_says_what_to_install() {
        let probe = HostProbe {
            bluez_card: false,
            spa_plugin: None,
            pulse_module: false,
            ..healthy_probe()
        };
        let text = render(host_checks(&probe));
        assert!(text.contains("! bluez audio backend"));
        assert!(text.contains("libspa-0.2-bluetooth"));
    }

    #[test]
    fn probe_gives_up_instead_of_blocking() {
        // `sleep 30` stands in for a wedged bluetoothd, which really does
        // block `bluetoothctl show` for ~20 minutes.
        let start = Instant::now();
        assert_eq!(
            probe_within("sleep", &["30"], Duration::from_millis(200)),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn probe_returns_output_of_a_fast_command() {
        let out = probe("echo", &["hello"]).expect("echo should succeed");
        assert_eq!(out.trim(), "hello");
        // A non-zero exit is "no answer", not an empty answer.
        assert_eq!(probe("false", &[]), None);
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
