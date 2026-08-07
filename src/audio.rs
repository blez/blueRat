//! Audio routing via `pactl`, mirroring the rofi script's route_audio().

use std::process::Command;
use std::thread;
use std::time::Duration;

fn pactl(args: &[&str]) -> (bool, String) {
    match Command::new("pactl").args(args).output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// Does this sink belong to the device? Matches the underscored MAC anywhere
/// in the sink name, so both PipeWire (`bluez_output.<MAC>.1`) and PulseAudio
/// (`bluez_sink.<MAC>.a2dp_sink`) naming schemes work.
fn sink_matches(sink: &str, mac_underscored: &str) -> bool {
    sink.to_ascii_lowercase()
        .contains(&mac_underscored.to_ascii_lowercase())
}

/// `pactl list short ...` is tab-separated: field 0 = index, field 1 = name.
fn field(line: &str, idx: usize) -> Option<&str> {
    line.split('\t').nth(idx)
}

fn find_sink(mac_underscored: &str) -> Option<String> {
    let (_, out) = pactl(&["list", "short", "sinks"]);
    out.lines()
        .filter_map(|l| field(l, 1))
        .find(|s| sink_matches(s, mac_underscored))
        .map(str::to_string)
}

/// The bluez card for a device, if the audio backend built one. A card
/// without a sink is the tell-tale of a profile that failed to start.
pub fn card_for(mac: &str) -> Option<String> {
    let mac_us = mac.replace(':', "_");
    let (_, cards) = pactl(&["list", "short", "cards"]);
    cards
        .lines()
        .filter_map(|l| field(l, 1))
        .find(|c| sink_matches(c, &mac_us))
        .map(str::to_string)
}

/// The sink for a device, if one exists right now.
pub fn sink_for(mac: &str) -> Option<String> {
    find_sink(&mac.replace(':', "_"))
}

/// Active profile of a device's bluez card, e.g. "a2dp-sink".
pub fn active_profile(mac: &str) -> Option<String> {
    let card = card_for(mac)?;
    let (_, out) = pactl(&["list", "cards"]);
    parse_card_profiles(&out, &card).0
}

/// Name of the running sound server ("PulseAudio (on PipeWire 1.0.5)"), for
/// diagnostics.
pub fn server_name() -> Option<String> {
    let (ok, out) = pactl(&["info"]);
    if !ok {
        return None;
    }
    out.lines()
        .find_map(|l| l.strip_prefix("Server Name: "))
        .map(str::to_string)
}

fn short_sink_input_ids() -> Vec<String> {
    let (_, out) = pactl(&["list", "short", "sink-inputs"]);
    out.lines()
        .filter_map(|l| field(l, 0))
        .map(str::to_string)
        .collect()
}

/// Make a freshly-connected audio device the default output and move all
/// existing streams to it. Waits up to 8s for the sink to appear; if it never
/// does (e.g. a mouse), silently gives up — that matches the script.
/// Returns true when audio was actually routed.
pub fn route_audio(mac: &str, progress: impl Fn(String)) -> bool {
    let mac_us = mac.replace(':', "_");
    let mut sink = None;
    // One immediate check, then up to 8 sleep-and-recheck rounds, so the full
    // 8s window is observed and no trailing sleep is wasted.
    for attempt in 0..=8u32 {
        if attempt > 0 {
            thread::sleep(Duration::from_secs(1));
        }
        sink = find_sink(&mac_us);
        if sink.is_some() {
            break;
        }
        if attempt < 8 {
            progress(format!("Waiting for audio sink… ({}/8)", attempt + 1));
        }
    }
    let Some(sink) = sink else { return false };
    if !pactl(&["set-default-sink", &sink]).0 {
        return false;
    }
    for id in short_sink_input_ids() {
        pactl(&["move-sink-input", &id, &sink]);
    }
    true
}

/// A playback-quality profile: A2DP for classic audio, BAP for LE Audio.
fn is_media_profile(p: &str) -> bool {
    p.contains("a2dp") || p.contains("bap")
}

/// A profile that also opens a microphone, at reduced playback quality.
fn is_headset_profile(p: &str) -> bool {
    p.contains("headset") || p.contains("handsfree") || p.contains("hfp")
}

/// Toggle the device's card between its high-quality playback profile
/// (A2DP or LE Audio BAP) and its headset (mic-enabled) profile. Returns the
/// name of the newly active profile.
pub fn toggle_profile(mac: &str) -> Result<String, String> {
    let card = card_for(mac).ok_or("no audio card for this device (is it connected?)")?;

    let (_, out) = pactl(&["list", "cards"]);
    let (active, available) = parse_card_profiles(&out, &card);
    let active = active.ok_or("could not determine active profile")?;

    let want_headset = is_media_profile(&active);
    let target = available
        .iter()
        .find(|p| {
            if want_headset {
                is_headset_profile(p)
            } else {
                is_media_profile(p)
            }
        })
        .ok_or_else(|| {
            if want_headset {
                "device has no headset/mic profile".to_string()
            } else {
                "device has no high-quality playback profile".to_string()
            }
        })?;

    if !pactl(&["set-card-profile", &card, target]).0 {
        return Err(format!("failed to switch profile to {target}"));
    }
    Ok(target.clone())
}

/// From `pactl list cards` output, extract (active profile, available
/// profiles) for the named card.
fn parse_card_profiles(out: &str, card: &str) -> (Option<String>, Vec<String>) {
    let mut in_card = false;
    let mut in_profiles = false;
    let mut active = None;
    let mut available = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("Name: ") {
            in_card = name == card;
            in_profiles = false;
            continue;
        }
        if !in_card {
            continue;
        }
        if t.starts_with("Profiles:") {
            in_profiles = true;
            continue;
        }
        if let Some(p) = t.strip_prefix("Active Profile: ") {
            active = Some(p.to_string());
            in_profiles = false;
            continue;
        }
        if in_profiles
            && let Some((name, rest)) = t.split_once(':')
            && rest.contains("available: yes")
            && name != "off"
        {
            available.push(name.to_string());
        }
    }
    (active, available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_pipewire_sink_name() {
        assert!(sink_matches(
            "bluez_output.AA_BB_CC_DD_EE_FF.1",
            "AA_BB_CC_DD_EE_FF"
        ));
    }

    #[test]
    fn matches_pulseaudio_sink_name() {
        assert!(sink_matches(
            "bluez_sink.aa_bb_cc_dd_ee_ff.a2dp_sink",
            "AA_BB_CC_DD_EE_FF"
        ));
    }

    #[test]
    fn rejects_other_sinks() {
        assert!(!sink_matches(
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "AA_BB_CC_DD_EE_FF"
        ));
    }

    #[test]
    fn parses_card_profiles() {
        let out = "Card #52\n\
                   \tName: bluez_card.AA_BB_CC_DD_EE_FF\n\
                   \tDriver: module-bluez5-device.c\n\
                   \tProfiles:\n\
                   \t\ta2dp-sink: High Fidelity Playback (A2DP Sink) (sinks: 1, sources: 0, priority: 40, available: yes)\n\
                   \t\theadset-head-unit: Headset Head Unit (HSP/HFP) (sinks: 1, sources: 1, priority: 30, available: yes)\n\
                   \t\toff: Off (sinks: 0, sources: 0, priority: 0, available: yes)\n\
                   \tActive Profile: a2dp-sink\n\
                   Card #53\n\
                   \tName: alsa_card.pci-0000_00_1f.3\n";
        let (active, avail) = parse_card_profiles(out, "bluez_card.AA_BB_CC_DD_EE_FF");
        assert_eq!(active.as_deref(), Some("a2dp-sink"));
        assert_eq!(avail, vec!["a2dp-sink", "headset-head-unit"]);
        let (none, empty) = parse_card_profiles(out, "bluez_card.other");
        assert_eq!(none, None);
        assert!(empty.is_empty());
    }

    #[test]
    fn recognizes_le_audio_profiles() {
        assert!(is_media_profile("a2dp-sink"));
        assert!(is_media_profile("a2dp-sink-sbc_xq"));
        assert!(is_media_profile("bap-sink"));
        assert!(!is_media_profile("headset-head-unit"));
        assert!(is_headset_profile("headset-head-unit-msbc"));
        assert!(!is_headset_profile("bap-sink"));
    }

    #[test]
    fn toggles_away_from_an_le_audio_profile() {
        // A BAP card must offer the headset direction, not error out.
        let out = "Card #9\n\
                   \tName: bluez_card.AA_BB_CC_DD_EE_FF\n\
                   \tProfiles:\n\
                   \t\tbap-sink: High Fidelity Playback (BAP Sink) (sinks: 1, sources: 0, priority: 40, available: yes)\n\
                   \t\theadset-head-unit: Headset Head Unit (HSP/HFP) (sinks: 1, sources: 1, priority: 30, available: yes)\n\
                   \tActive Profile: bap-sink\n";
        let (active, avail) = parse_card_profiles(out, "bluez_card.AA_BB_CC_DD_EE_FF");
        assert_eq!(active.as_deref(), Some("bap-sink"));
        assert!(is_media_profile(active.as_deref().unwrap()));
        assert!(avail.iter().any(|p| is_headset_profile(p)));
    }

    #[test]
    fn parses_tab_separated_fields() {
        let line =
            "57\tbluez_output.AA_BB_CC_DD_EE_FF.1\tmodule-bluez5-device.c\ts16le 2ch 48000Hz\tIDLE";
        assert_eq!(field(line, 0), Some("57"));
        assert_eq!(field(line, 1), Some("bluez_output.AA_BB_CC_DD_EE_FF.1"));
    }
}
