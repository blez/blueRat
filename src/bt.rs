//! Thin wrappers around `bluetoothctl`, mirroring blue-tui.sh.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub connected: bool,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub mac: String,
    pub name: String,
}

/// Live state of one device, from `bluetoothctl info`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub connected: bool,
    pub trusted: bool,
    /// Offers an A2DP Audio Sink profile (headphones/speakers) — the only
    /// devices worth waiting on for audio routing.
    pub audio: bool,
}

/// PID of the bluetoothctl invocation currently blocking the worker, if any.
static CHILD_PID: AtomicU32 = AtomicU32::new(0);

/// Run bluetoothctl with args; returns (exit success, stdout, stderr).
fn btctl(args: &[&str]) -> (bool, String, String) {
    let child = Command::new("bluetoothctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match child {
        Ok(child) => {
            CHILD_PID.store(child.id(), Ordering::SeqCst);
            let out = child.wait_with_output();
            CHILD_PID.store(0, Ordering::SeqCst);
            match out {
                Ok(out) => (
                    out.status.success(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ),
                Err(e) => (false, String::new(), e.to_string()),
            }
        }
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// Kill the bluetoothctl call currently blocking the worker, if any. Called
/// on exit so e.g. a 10s scan doesn't keep the adapter in discovery mode
/// after the app is gone.
pub fn kill_running_child() {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid != 0 {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

/// First non-empty diagnostic out of stderr/stdout, else a generic hint.
fn error_text(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("bluetoothctl failed (is bluetoothd running?)")
        .to_string()
}

fn has_device_lines(out: &str) -> bool {
    out.lines().any(|l| parse_device_line(l).is_some())
}

/// Paired-device listing with a fallback for older bluez, where the command
/// is `paired-devices` instead of `devices Paired`.
fn list_paired_output() -> Result<String, String> {
    let (ok, out, err) = btctl(&["devices", "Paired"]);
    if ok {
        // Some older bluetoothctl versions exit 0 while not understanding the
        // filter; if we got no device lines, cross-check the legacy command.
        if !has_device_lines(&out) {
            let (ok2, out2, _) = btctl(&["paired-devices"]);
            if ok2 && has_device_lines(&out2) {
                return Ok(out2);
            }
        }
        return Ok(out);
    }
    let (ok2, out2, err2) = btctl(&["paired-devices"]);
    if ok2 {
        return Ok(out2);
    }
    Err(error_text(&[&err, &err2, &out, &out2]))
}

/// Power the adapter on if it isn't already. Cheap; run before radio ops.
pub fn adapter_up() -> Result<(), String> {
    let (ok, show, err) = btctl(&["show"]);
    if !ok {
        return Err(format!(
            "Bluetooth adapter unavailable: {}",
            error_text(&[&err, &show])
        ));
    }
    if show.contains("Powered: yes") {
        return Ok(());
    }
    let (ok2, out2, err2) = btctl(&["power", "on"]);
    if !ok2 {
        return Err(format!(
            "Failed to power on adapter (rfkill blocked?): {}",
            error_text(&[&err2, &out2])
        ));
    }
    Ok(())
}

/// Live state for a MAC. Err means the query itself failed — callers must
/// not treat that as "disconnected".
pub fn info(mac: &str) -> Result<Info, String> {
    let (ok, out, err) = btctl(&["info", mac]);
    if !ok {
        return Err(error_text(&[&err, &out]));
    }
    Ok(parse_info(&out))
}

fn parse_info(out: &str) -> Info {
    Info {
        connected: out.contains("Connected: yes"),
        trusted: out.contains("Trusted: yes"),
        audio: out.contains("Audio Sink"),
    }
}

pub fn paired_devices() -> Result<Vec<Device>, String> {
    let pairs: Vec<(String, String)> = list_paired_output()?
        .lines()
        .filter_map(parse_device_line)
        .collect();

    // Two whole-list queries beat one `info` subprocess per device.
    let (okc, conn, _) = btctl(&["devices", "Connected"]);
    let (okt, trust, _) = btctl(&["devices", "Trusted"]);
    if okc && okt {
        let connected: Vec<String> = conn
            .lines()
            .filter_map(parse_device_line)
            .map(|(mac, _)| mac)
            .collect();
        let trusted: Vec<String> = trust
            .lines()
            .filter_map(parse_device_line)
            .map(|(mac, _)| mac)
            .collect();
        return Ok(pairs
            .into_iter()
            .map(|(mac, name)| Device {
                connected: connected.contains(&mac),
                trusted: trusted.contains(&mac),
                mac,
                name,
            })
            .collect());
    }

    // Older bluez without `devices <filter>`: one info call per device.
    Ok(pairs
        .into_iter()
        .map(|(mac, name)| {
            let i = info(&mac).unwrap_or_default();
            Device {
                mac,
                name,
                connected: i.connected,
                trusted: i.trusted,
            }
        })
        .collect())
}

/// Blocking scan for ~`secs` seconds. Exit code is ignored (the script does `|| true`).
pub fn scan(secs: u32) {
    btctl(&["--timeout", &secs.to_string(), "scan", "on"]);
}

/// Discovered devices that aren't paired and have a real name.
pub fn discovered_unpaired() -> Result<Vec<Discovered>, String> {
    let paired: Vec<String> = list_paired_output()?
        .lines()
        .filter_map(parse_device_line)
        .map(|(mac, _)| mac)
        .collect();

    let (ok, all, err) = btctl(&["devices"]);
    if !ok {
        return Err(error_text(&[&err, &all]));
    }
    Ok(all
        .lines()
        .filter_map(parse_device_line)
        .filter(|(mac, name)| !paired.contains(mac) && !name_is_mac(name, mac))
        .map(|(mac, name)| Discovered { mac, name })
        .collect())
}

pub fn connect(mac: &str) -> bool {
    btctl(&["connect", mac]).0
}

pub fn disconnect(mac: &str) -> bool {
    btctl(&["disconnect", mac]).0
}

pub fn pair(mac: &str) -> bool {
    btctl(&["pair", mac]).0
}

pub fn trust(mac: &str) -> bool {
    btctl(&["trust", mac]).0
}

pub fn untrust(mac: &str) -> bool {
    btctl(&["untrust", mac]).0
}

pub fn remove(mac: &str) -> bool {
    btctl(&["remove", mac]).0
}

/// "Device <MAC> <name>" → (mac, name). Other lines → None.
fn parse_device_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("Device ")?;
    let (mac, name) = rest.split_once(' ')?;
    if mac.is_empty() || name.is_empty() {
        return None;
    }
    Some((mac.to_string(), name.to_string()))
}

/// Nameless devices are reported with their MAC as the name (sometimes
/// dash-separated). Mirrors the rofi script's `grep -v "^Device .. ..$"` filter.
fn name_is_mac(name: &str, mac: &str) -> bool {
    name.replace('-', ":").eq_ignore_ascii_case(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_line() {
        assert_eq!(
            parse_device_line("Device AA:BB:CC:DD:EE:FF WH-1000XM4"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "WH-1000XM4".into()))
        );
    }

    #[test]
    fn keeps_spaces_in_name() {
        assert_eq!(
            parse_device_line("Device 11:22:33:44:55:66 My Cool Speaker"),
            Some(("11:22:33:44:55:66".into(), "My Cool Speaker".into()))
        );
    }

    #[test]
    fn rejects_non_device_lines() {
        assert_eq!(parse_device_line(""), None);
        assert_eq!(
            parse_device_line("Controller 00:11:22:33:44:55 laptop"),
            None
        );
        assert_eq!(parse_device_line("Device AA:BB:CC:DD:EE:FF"), None);
    }

    #[test]
    fn detects_mac_as_name() {
        assert!(name_is_mac("AA:BB:CC:DD:EE:FF", "AA:BB:CC:DD:EE:FF"));
        assert!(name_is_mac("aa-bb-cc-dd-ee-ff", "AA:BB:CC:DD:EE:FF"));
        assert!(!name_is_mac("WH-1000XM4", "AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn error_text_prefers_first_diagnostic() {
        assert_eq!(
            error_text(&["", "No default controller available"]),
            "No default controller available"
        );
        assert_eq!(
            error_text(&["", "  "]),
            "bluetoothctl failed (is bluetoothd running?)"
        );
    }

    #[test]
    fn detects_device_lines() {
        assert!(has_device_lines("Device AA:BB:CC:DD:EE:FF Buds"));
        assert!(!has_device_lines("Too many arguments"));
    }

    #[test]
    fn parses_info_output() {
        let out = "Device AA:BB:CC:DD:EE:FF (public)\n\
                   \tName: WH-1000XM4\n\
                   \tPaired: yes\n\
                   \tTrusted: yes\n\
                   \tConnected: no\n\
                   \tUUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)\n";
        assert_eq!(
            parse_info(out),
            Info {
                connected: false,
                trusted: true,
                audio: true
            }
        );
        assert_eq!(parse_info(""), Info::default());
    }
}
