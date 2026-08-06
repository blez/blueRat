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
    fn parses_tab_separated_fields() {
        let line =
            "57\tbluez_output.AA_BB_CC_DD_EE_FF.1\tmodule-bluez5-device.c\ts16le 2ch 48000Hz\tIDLE";
        assert_eq!(field(line, 0), Some("57"));
        assert_eq!(field(line, 1), Some("bluez_output.AA_BB_CC_DD_EE_FF.1"));
    }
}
