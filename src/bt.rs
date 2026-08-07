//! Thin wrappers around `bluetoothctl`, mirroring blue-tui.sh.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub connected: bool,
    pub trusted: bool,
    /// Battery percentage, when the device reports one (usually only while
    /// connected).
    pub battery: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub mac: String,
    pub name: String,
    /// Public or random, which decides whether this address may be folded
    /// into a same-named group. Filled in by `fill_addr_kinds`; `Unknown`
    /// until then, and treated as stable (its own row) when it stays that way.
    pub addr: AddrKind,
}

/// One physical device as offered in the scan picker. Modern earbuds announce
/// themselves on several addresses at once (a classic BR/EDR one plus rotating
/// LE private ones), all under the same name; pairing the wrong one yields a
/// connection with no audio sink. The picker shows one row per name and the
/// worker picks the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredGroup {
    pub name: String,
    pub macs: Vec<String>,
}

/// How a device's address was assigned. Random addresses are LE-only; a
/// classic (audio-capable) radio always uses a public one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AddrKind {
    Public,
    Random,
    #[default]
    Unknown,
}

/// Which audio transport a device offers, as advertised in its UUID list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    /// No audio profile at all (mouse, keyboard, phone).
    #[default]
    None,
    /// A2DP Audio Sink (0000110b) — classic audio. Works with any
    /// PipeWire/PulseAudio bluez backend.
    Classic,
    /// Only LE Audio (BAP) services, no A2DP. Routing these needs a kernel
    /// ISO socket *and* a BAP-capable session manager; without both, the
    /// device connects happily and no sink ever appears.
    LeAudioOnly,
}

impl AudioKind {
    /// Worth waiting on a sink for after connecting.
    pub fn routable(self) -> bool {
        !matches!(self, AudioKind::None)
    }
}

/// Live state of one device, from `bluetoothctl info`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Info {
    pub connected: bool,
    pub trusted: bool,
    /// Which audio transport the device offers, if any.
    pub audio: AudioKind,
    pub addr: AddrKind,
    pub battery: Option<u8>,
}

/// An agent prompt raised during interactive pairing.
pub enum PairEvent {
    /// "Confirm passkey NNNNNN (yes/no)" — the string is the passkey, empty
    /// for generic yes/no authorization prompts.
    ConfirmPasskey(String),
    /// "Enter PIN code:" / "Enter passkey:".
    RequestPin,
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
    Ok(parse_info(&info_raw(mac)?))
}

/// Raw `bluetoothctl info` output for the details popup.
pub fn info_raw(mac: &str) -> Result<String, String> {
    let (ok, out, err) = btctl(&["info", mac]);
    if !ok {
        return Err(error_text(&[&err, &out]));
    }
    Ok(out)
}

fn parse_info(out: &str) -> Info {
    Info {
        connected: out.contains("Connected: yes"),
        trusted: out.contains("Trusted: yes"),
        audio: parse_audio_kind(out),
        addr: parse_addr_kind(out),
        battery: parse_battery(out),
    }
}

/// Services an LE Audio *sink* hosts. A Unicast Server (earbuds, speaker)
/// publishes both: ASCS to carry the stream and PACS to describe what it can
/// play. bluetoothctl truncates long names to a fixed column
/// ("Published Audio Capabil.."), so these are matched as prefixes.
///
/// Requiring both matters: phones and watches advertise LE Audio *client*
/// services (Common Audio, Call Control, Media Control) without being able to
/// play anything, and treating those as audio sinks makes every connect wait
/// out the sink poll for nothing.
const LE_AUDIO_SINK_UUIDS: [&str; 2] = ["Audio Stream Control", "Published Audio Capabil"];

/// A2DP beats LE Audio when a device offers both: classic audio needs no
/// experimental kernel support, so it is the transport that actually works.
fn parse_audio_kind(out: &str) -> AudioKind {
    if out.contains("Audio Sink") {
        return AudioKind::Classic;
    }
    if LE_AUDIO_SINK_UUIDS.iter().all(|u| out.contains(u)) {
        return AudioKind::LeAudioOnly;
    }
    AudioKind::None
}

/// "Device AA:BB:CC:DD:EE:FF (public)" → Public.
fn parse_addr_kind(out: &str) -> AddrKind {
    let Some(line) = out.lines().find(|l| l.starts_with("Device ")) else {
        return AddrKind::Unknown;
    };
    if line.contains("(public)") {
        AddrKind::Public
    } else if line.contains("(random)") {
        AddrKind::Random
    } else {
        AddrKind::Unknown
    }
}

/// "Battery Percentage: 0x50 (80)" → 80.
fn parse_battery(out: &str) -> Option<u8> {
    let line = out.lines().find(|l| l.contains("Battery Percentage"))?;
    let inside = line.rsplit('(').next()?.split(')').next()?;
    inside.trim().parse().ok()
}

pub fn paired_devices() -> Result<Vec<Device>, String> {
    let pairs: Vec<(String, String)> = list_paired_output()?
        .lines()
        .filter_map(parse_device_line)
        .collect();

    // Two whole-list queries beat one `info` subprocess per device.
    let (okc, conn, _) = btctl(&["devices", "Connected"]);
    let (okt, trust, _) = btctl(&["devices", "Trusted"]);
    let mut devices: Vec<Device> = if okc && okt {
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
        pairs
            .into_iter()
            .map(|(mac, name)| Device {
                connected: connected.contains(&mac),
                trusted: trusted.contains(&mac),
                mac,
                name,
                battery: None,
            })
            .collect()
    } else {
        // Older bluez without `devices <filter>`: one info call per device.
        pairs
            .into_iter()
            .map(|(mac, name)| {
                let i = info(&mac).unwrap_or_default();
                Device {
                    mac,
                    name,
                    connected: i.connected,
                    trusted: i.trusted,
                    battery: i.battery,
                }
            })
            .collect()
    };

    // Battery is only reported while connected; one extra info call per
    // connected device (usually 0-2) is cheap.
    for d in devices.iter_mut().filter(|d| d.connected) {
        if d.battery.is_none()
            && let Ok(i) = info(&d.mac)
        {
            d.battery = i.battery;
        }
    }
    Ok(devices)
}

/// Live scan: spawns `bluetoothctl --timeout <secs> scan on` and reports
/// discovered unpaired named devices as their lines stream in. Blocks until
/// the scan ends.
pub fn scan_live(secs: u32, mut on_found: impl FnMut(Discovered)) {
    let paired = paired_macs().unwrap_or_default();

    let child = Command::new("bluetoothctl")
        .args(["--timeout", &secs.to_string(), "scan", "on"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    CHILD_PID.store(child.id(), Ordering::SeqCst);
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines() {
            // Device names are arbitrary bytes; one undecodable line must
            // not end the whole scan.
            let Ok(line) = line else { continue };
            if let Some((mac, name)) = parse_scan_event(&line)
                && discoverable(&paired, &mac, &name)
            {
                on_found(Discovered {
                    mac,
                    name,
                    addr: AddrKind::Unknown,
                });
            }
        }
    }
    let _ = child.wait();
    CHILD_PID.store(0, Ordering::SeqCst);
}

/// "[NEW] Device <mac> <name>" or "[CHG] Device <mac> Name: <name>" →
/// (mac, name). ANSI escapes and control characters are stripped first.
fn parse_scan_event(line: &str) -> Option<(String, String)> {
    let clean = strip_ansi(line);
    let clean = clean.trim();
    let is_new = clean.contains("[NEW]");
    let is_chg = clean.contains("[CHG]");
    if !is_new && !is_chg {
        return None;
    }
    // Anchor on the "] Device " that follows the [NEW]/[CHG] tag — a plain
    // split on "Device " would also fire inside names like "My Device Pro".
    let rest = &clean[clean.find("] Device ")? + "] Device ".len()..];
    let (mac, tail) = rest.split_once(' ')?;
    if is_chg {
        let name = tail.strip_prefix("Name: ")?;
        return Some((mac.to_string(), name.to_string()));
    }
    Some((mac.to_string(), tail.to_string()))
}

/// Remove ANSI escape sequences and stray control characters.
fn strip_ansi(s: &str) -> String {
    let mut carry = s.to_string();
    strip_ansi_stream(&mut carry)
}

/// Streaming `strip_ansi`: consumes `carry`, returns the cleaned text, and
/// leaves a trailing incomplete escape sequence back in `carry` so a code
/// split across two reads still gets stripped once the rest arrives.
fn strip_ansi_stream(carry: &mut String) -> String {
    let s = std::mem::take(carry);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // Skip CSI-style sequences up to the final letter.
                Some((_, '[')) => {
                    chars.next();
                    let mut terminated = false;
                    for (_, e) in chars.by_ref() {
                        if e.is_ascii_alphabetic() {
                            terminated = true;
                            break;
                        }
                    }
                    if !terminated {
                        *carry = s[i..].to_string();
                        break;
                    }
                }
                // Input ends right at the ESC — sequence may continue in the
                // next chunk.
                None => {
                    *carry = s[i..].to_string();
                    break;
                }
                Some(_) => {} // lone ESC before ordinary text: drop it
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Interactive pairing session that can answer BlueZ agent prompts (passkey
/// confirmation, PIN entry) via the `respond` callback. Returns Ok(success);
/// Err means the session could not be started (caller may fall back to the
/// one-shot `pair`).
pub fn pair_interactive(
    mac: &str,
    mut respond: impl FnMut(PairEvent) -> Option<String>,
) -> Result<bool, String> {
    let mut child = Command::new("bluetoothctl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    CHILD_PID.store(child.id(), Ordering::SeqCst);
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let _ = writeln!(stdin, "pair {mac}");

    // `clean` accumulates ANSI-stripped output; each chunk is stripped once
    // (a code split across reads waits in `carry`) instead of re-stripping
    // the whole session on every read. Answered prompts clear it so the same
    // prompt can't match twice.
    let mut clean = String::new();
    let mut carry = String::new();
    let mut buf = [0u8; 512];
    let mut success = false;
    let mut cancelled = false;
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.push_str(&String::from_utf8_lossy(&buf[..n]));
        clean.push_str(&strip_ansi_stream(&mut carry));
        if clean.contains("Pairing successful") || clean.contains("AlreadyExists") {
            success = true;
            break;
        }
        if ["Failed to pair", "Authentication", "not available"]
            .iter()
            .any(|m| clean.contains(m))
        {
            break;
        }
        if clean.contains("Confirm passkey") && clean.contains("(yes/no)") {
            let pk = extract_passkey(&clean).unwrap_or_default();
            let ans = respond(PairEvent::ConfirmPasskey(pk)).unwrap_or_else(|| "no".into());
            let _ = writeln!(stdin, "{ans}");
            clean.clear();
        } else if clean.contains("(yes/no)") {
            let ans =
                respond(PairEvent::ConfirmPasskey(String::new())).unwrap_or_else(|| "no".into());
            let _ = writeln!(stdin, "{ans}");
            clean.clear();
        } else if clean.contains("Enter PIN code") || clean.contains("Enter passkey") {
            match respond(PairEvent::RequestPin) {
                Some(ans) if !ans.is_empty() => {
                    let _ = writeln!(stdin, "{ans}");
                    clean.clear();
                }
                // Cancelled. bluetoothctl is sitting at an agent prompt that
                // would swallow "quit" as the PIN, so end the session by
                // force instead.
                _ => {
                    cancelled = true;
                    break;
                }
            }
        }
        // A pathological session (endless [CHG] chatter, never a prompt)
        // must not grow the buffer forever; the markers we look for always
        // sit in the recent tail.
        if clean.len() > 16 * 1024 {
            let target = clean.len() - 4096;
            let cut = match clean[..target].rfind('\n') {
                Some(i) => i + 1,
                None => {
                    let mut c = target;
                    while !clean.is_char_boundary(c) {
                        c -= 1;
                    }
                    c
                }
            };
            clean.drain(..cut);
        }
    }
    if cancelled {
        let _ = child.kill();
    } else {
        let _ = writeln!(stdin, "quit");
    }
    // Close both pipes before reaping, so a still-chatty bluetoothctl can't
    // block forever on a full stdout pipe we no longer read.
    drop(stdin);
    drop(stdout);
    let _ = child.wait();
    CHILD_PID.store(0, Ordering::SeqCst);
    Ok(success)
}

/// Digits following "Confirm passkey".
fn extract_passkey(text: &str) -> Option<String> {
    let after = text.split("Confirm passkey").nth(1)?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then_some(digits)
}

/// MACs of all paired devices.
fn paired_macs() -> Result<Vec<String>, String> {
    Ok(list_paired_output()?
        .lines()
        .filter_map(parse_device_line)
        .map(|(mac, _)| mac)
        .collect())
}

/// Worth offering in the scan picker: not already paired, and carrying a
/// real name rather than its own MAC.
fn discoverable(paired: &[String], mac: &str, name: &str) -> bool {
    !paired.iter().any(|p| p == mac) && !name_is_mac(name, mac)
}

/// Discovered devices that aren't paired and have a real name.
pub fn discovered_unpaired() -> Result<Vec<Discovered>, String> {
    let paired = paired_macs()?;
    let (ok, all, err) = btctl(&["devices"]);
    if !ok {
        return Err(error_text(&[&err, &all]));
    }
    Ok(all
        .lines()
        .filter_map(parse_device_line)
        .filter(|(mac, name)| discoverable(&paired, mac, name))
        .map(|(mac, name)| Discovered {
            mac,
            name,
            addr: AddrKind::Unknown,
        })
        .collect())
}

/// Collapse the addresses of one physical device into a single pickable row,
/// keeping first-seen order. One pair of earbuds routinely advertises three or
/// four addresses at once; showing them all invites pairing the one that
/// carries no audio.
///
/// Sharing a name is not enough to be the same device — two identical headsets
/// in one room announce the same name — so only rotating LE (random) addresses
/// fold in. A group holds at most one stable address; a second one starts its
/// own row, and stays pairable in its own right.
pub fn group_discovered(items: &[Discovered]) -> Vec<DiscoveredGroup> {
    let mut groups: Vec<DiscoveredGroup> = Vec::new();
    // Parallel to `groups`: does this row already own a stable address?
    let mut has_stable: Vec<bool> = Vec::new();
    for d in items {
        // An address already listed is the same address, whatever else it
        // would qualify for — check that before anything can split it off.
        if groups.iter().any(|g| g.macs.contains(&d.mac)) {
            continue;
        }
        let stable = d.addr != AddrKind::Random;
        let key = d.name.trim();
        let slot = groups
            .iter()
            .enumerate()
            .find(|(i, g)| g.name.trim().eq_ignore_ascii_case(key) && !(stable && has_stable[*i]))
            .map(|(i, _)| i);
        match slot {
            Some(i) => {
                groups[i].macs.push(d.mac.clone());
                has_stable[i] |= stable;
            }
            None => {
                groups.push(DiscoveredGroup {
                    name: d.name.clone(),
                    macs: vec![d.mac.clone()],
                });
                has_stable.push(stable);
            }
        }
    }
    groups
}

/// Look up each entry's address kind — one `bluetoothctl info` per address,
/// so it runs on the worker when a discovered list is assembled, never on the
/// UI thread. Without it every address looks stable and nothing groups.
pub fn fill_addr_kinds(items: &mut [Discovered]) {
    for d in items.iter_mut() {
        if d.addr == AddrKind::Unknown
            && let Ok(i) = info(&d.mac)
        {
            d.addr = i.addr;
        }
    }
}

/// Order one group's addresses best-first for pairing. Costs one `info` call
/// per address, so it runs at pair time on a single group — never during the
/// scan.
pub fn rank_candidates(macs: &[String]) -> Vec<String> {
    let mut scored: Vec<(u8, usize, String)> = macs
        .iter()
        .enumerate()
        .map(|(i, mac)| {
            let info = info(mac).unwrap_or_default();
            (candidate_rank(info.audio, info.addr), i, mac.clone())
        })
        .collect();
    // Stable on rank ties: keep discovery order as the tiebreak.
    scored.sort_by_key(|(rank, i, _)| (*rank, *i));
    scored.into_iter().map(|(_, _, mac)| mac).collect()
}

/// Lower is better. An unpaired classic device often hasn't been SDP-probed
/// yet and so advertises no UUIDs at all — a public address with no known
/// audio profile still outranks a confirmed LE-only one, because LE Audio
/// needs host support that classic audio does not.
fn candidate_rank(audio: AudioKind, addr: AddrKind) -> u8 {
    match (audio, addr) {
        (AudioKind::Classic, _) => 0,
        (AudioKind::None, AddrKind::Public) => 1,
        (AudioKind::LeAudioOnly, AddrKind::Public) => 2,
        (AudioKind::LeAudioOnly, _) => 3,
        (AudioKind::None, _) => 4,
    }
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
                   \tUUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)\n\
                   \tBattery Percentage: 0x50 (80)\n";
        assert_eq!(
            parse_info(out),
            Info {
                connected: false,
                trusted: true,
                audio: AudioKind::Classic,
                addr: AddrKind::Public,
                battery: Some(80),
            }
        );
        assert_eq!(parse_info(""), Info::default());
    }

    #[test]
    fn detects_le_audio_only_device() {
        // Real Buds3 Pro advertisement: BAP services, no A2DP Audio Sink.
        let out = "Device A0:B0:BD:F3:BA:7E (public)\n\
                   \tName: Buds3 Pro\n\
                   \tUUID: Volume Control           (00001844-0000-1000-8000-00805f9b34fb)\n\
                   \tUUID: Audio Stream Control     (0000184e-0000-1000-8000-00805f9b34fb)\n\
                   \tUUID: Published Audio Capabil.. (00001850-0000-1000-8000-00805f9b34fb)\n\
                   \tUUID: Common Audio             (00001853-0000-1000-8000-00805f9b34fb)\n";
        let info = parse_info(out);
        assert_eq!(info.audio, AudioKind::LeAudioOnly);
        assert!(info.audio.routable());
    }

    #[test]
    fn a2dp_wins_when_a_device_offers_both() {
        let out = "Device AA:BB:CC:DD:EE:FF (public)\n\
                   \tUUID: Audio Sink               (0000110b-0000-1000-8000-00805f9b34fb)\n\
                   \tUUID: Audio Stream Control     (0000184e-0000-1000-8000-00805f9b34fb)\n";
        assert_eq!(parse_info(out).audio, AudioKind::Classic);
    }

    #[test]
    fn le_audio_client_is_not_a_sink() {
        // A phone advertises LE Audio control services without being able to
        // play anything; treating it as a sink costs an 8s poll per connect
        // and produces a misleading warning.
        let phone = "Device AA:BB:CC:DD:EE:FF (public)\n\
                     \tUUID: Common Audio            (00001853-0000-1000-8000-00805f9b34fb)\n\
                     \tUUID: Call Control            (00001852-0000-1000-8000-00805f9b34fb)\n";
        assert_eq!(parse_audio_kind(phone), AudioKind::None);

        let earbuds = "Device AA:BB:CC:DD:EE:FF (random)\n\
                       \tUUID: Audio Stream Control    (0000184e-0000-1000-8000-00805f9b34fb)\n\
                       \tUUID: Published Audio Capabil (00001850-0000-1000-8000-00805f9b34fb)\n";
        assert_eq!(parse_audio_kind(earbuds), AudioKind::LeAudioOnly);
    }

    #[test]
    fn non_audio_device_is_not_routable() {
        let out = "Device AA:BB:CC:DD:EE:FF (random)\n\
                   \tUUID: Human Interface Device   (00001812-0000-1000-8000-00805f9b34fb)\n";
        let info = parse_info(out);
        assert_eq!(info.audio, AudioKind::None);
        assert_eq!(info.addr, AddrKind::Random);
        assert!(!info.audio.routable());
    }

    fn disc(mac: &str, name: &str, addr: AddrKind) -> Discovered {
        Discovered {
            mac: mac.into(),
            name: name.into(),
            addr,
        }
    }

    #[test]
    fn groups_discovered_by_name() {
        let items = vec![
            disc("40:7E:72:67:25:64", "Pavel's Buds3 Pro", AddrKind::Public),
            disc("7C:AF:C1:52:DC:A8", "Pavel's Buds3 Pro", AddrKind::Random),
            disc("78:C1:1D:12:D4:96", "S26 Ultra", AddrKind::Public),
            // A duplicate address must not be listed twice.
            disc("40:7E:72:67:25:64", "Pavel's Buds3 Pro", AddrKind::Public),
        ];
        let groups = group_discovered(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Pavel's Buds3 Pro");
        assert_eq!(groups[0].macs.len(), 2);
        assert_eq!(groups[1].macs, vec!["78:C1:1D:12:D4:96"]);
    }

    #[test]
    fn two_identical_devices_stay_separately_pairable() {
        // Two people with the same earbuds in one room: both announce the same
        // name on their own stable address. Merging them would hide one device
        // and aim a pairing attempt at a stranger's.
        let items = vec![
            disc("40:7E:72:67:25:64", "Galaxy Buds3 Pro", AddrKind::Public),
            disc("52:11:22:33:44:55", "Galaxy Buds3 Pro", AddrKind::Random),
            disc("AA:BB:CC:DD:EE:FF", "Galaxy Buds3 Pro", AddrKind::Public),
        ];
        let groups = group_discovered(&items);
        assert_eq!(groups.len(), 2);
        // The rotating LE address folds into the first unit, not the second.
        assert_eq!(groups[0].macs.len(), 2);
        assert_eq!(groups[1].macs, vec!["AA:BB:CC:DD:EE:FF"]);
    }

    #[test]
    fn unknown_address_kinds_do_not_merge() {
        // Before `fill_addr_kinds` runs, nothing is known to rotate — stay on
        // the safe side and keep every address pickable.
        let items = vec![
            disc("40:7E:72:67:25:64", "Buds", AddrKind::Unknown),
            disc("7C:AF:C1:52:DC:A8", "Buds", AddrKind::Unknown),
        ];
        assert_eq!(group_discovered(&items).len(), 2);
    }

    #[test]
    fn ranks_classic_audio_above_le_only() {
        // The exact trap: an LE-only address must never be tried before a
        // classic one, and an unprobed public address beats a known LE one.
        assert!(
            candidate_rank(AudioKind::Classic, AddrKind::Public)
                < candidate_rank(AudioKind::LeAudioOnly, AddrKind::Public)
        );
        assert!(
            candidate_rank(AudioKind::None, AddrKind::Public)
                < candidate_rank(AudioKind::LeAudioOnly, AddrKind::Public)
        );
        assert!(
            candidate_rank(AudioKind::LeAudioOnly, AddrKind::Public)
                < candidate_rank(AudioKind::LeAudioOnly, AddrKind::Random)
        );
    }

    #[test]
    fn parses_scan_events() {
        assert_eq!(
            parse_scan_event("[NEW] Device AA:BB:CC:DD:EE:FF JBL Flip 5"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "JBL Flip 5".into()))
        );
        assert_eq!(
            parse_scan_event("\x1b[0;92m[NEW]\x1b[0m Device AA:BB:CC:DD:EE:FF Buds"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "Buds".into()))
        );
        assert_eq!(
            parse_scan_event("[CHG] Device AA:BB:CC:DD:EE:FF Name: Real Name"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "Real Name".into()))
        );
        assert_eq!(
            parse_scan_event("[CHG] Device AA:BB:CC:DD:EE:FF RSSI: -60"),
            None
        );
        assert_eq!(parse_scan_event("Discovery started"), None);
    }

    #[test]
    fn scan_event_keeps_device_in_name() {
        assert_eq!(
            parse_scan_event("[NEW] Device AA:BB:CC:DD:EE:FF My Device Pro"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "My Device Pro".into()))
        );
        assert_eq!(
            parse_scan_event("[CHG] Device AA:BB:CC:DD:EE:FF Name: My Device Pro"),
            Some(("AA:BB:CC:DD:EE:FF".into(), "My Device Pro".into()))
        );
    }

    #[test]
    fn strip_ansi_stream_carries_split_escape() {
        let mut carry = String::new();
        carry.push_str("foo\x1b[0");
        let mut out = strip_ansi_stream(&mut carry);
        assert_eq!(out, "foo");
        assert_eq!(carry, "\x1b[0");
        carry.push_str("1mbar");
        out.push_str(&strip_ansi_stream(&mut carry));
        assert_eq!(out, "foobar");
        assert!(carry.is_empty());
    }

    #[test]
    fn extracts_passkey() {
        assert_eq!(
            extract_passkey("[agent] Confirm passkey 461829 (yes/no):"),
            Some("461829".into())
        );
        assert_eq!(extract_passkey("Confirm passkey (yes/no)"), None);
    }
}
