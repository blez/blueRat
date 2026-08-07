//! Application state and key/message handling.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bt::{self, Device, Discovered, DiscoveredGroup};
use crate::worker::{Cmd, Msg, Tone};

/// Info/Ok log entries disappear this long after being pushed. Warn/Err
/// entries stay until the next operation starts (their instructions remain
/// actionable until the user acts again).
const LOG_TTL: Duration = Duration::from_secs(8);
/// At most this many event-log lines are kept (pub: the UI derives the
/// bottom-log area height from it).
pub const LOG_MAX: usize = 4;

pub enum View {
    DeviceList,
    ScanResults {
        /// Every discovered address, kept raw so a pair failure can reopen
        /// the picker unchanged. The rows the user sees are
        /// `bt::group_discovered(items)` — one per device name.
        items: Vec<Discovered>,
        selected: usize,
    },
    ConfirmRemove {
        mac: String,
        name: String,
    },
    Details {
        name: String,
        text: String,
        /// First wrapped row shown; the renderer clamps it to the content.
        scroll: u16,
    },
    PairPrompt {
        pin: bool,
        passkey: String,
        input: String,
    },
}

pub struct App {
    pub devices: Vec<Device>,
    /// Selection index into the *visible* (filtered) device list.
    pub selected: usize,
    pub view: View,
    pub busy: Option<String>,
    /// Live narration of the running operation, shown on the spinner line.
    pub progress: Option<String>,
    /// Banner-console-style event log: newest last.
    pub log: Vec<(String, Tone, Instant)>,
    /// Fuzzy filter over device names; empty = show all.
    pub filter: String,
    /// True while the user is typing the filter query.
    pub filtering: bool,
    /// Operations sent to the worker that haven't OpDone'd yet. Includes
    /// silent auto-refreshes, which carry no busy label.
    pub pending: u32,
    /// When the worker last went idle — drives the auto-refresh cadence.
    pub last_done: Instant,
    /// One-shot: drop the next `Msg::ScanResults`. Set when the user closes
    /// the picker (or queues a pair) mid-scan, so the scan's final list can't
    /// yank them back into a view they already left.
    pub ignore_scan_results: bool,
    /// The key-binding popup, shown by `?`. It is an overlay rather than a
    /// `View`: it can appear over any of them, and replacing the view would
    /// throw away its state — a running scan's results, loaded device info,
    /// a pending remove confirmation.
    pub help: bool,
    /// First row of the help list on screen; the renderer clamps it.
    pub help_scroll: u16,
    pub tick: u64,
    pub quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            selected: 0,
            view: View::DeviceList,
            busy: None,
            progress: None,
            log: Vec::new(),
            filter: String::new(),
            filtering: false,
            pending: 0,
            last_done: Instant::now(),
            ignore_scan_results: false,
            help: false,
            help_scroll: 0,
            tick: 0,
            quit: false,
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // Warn/Err survive the TTL; they expire in begin() instead.
        self.log
            .retain(|(_, t, at)| matches!(t, Tone::Warn | Tone::Err) || at.elapsed() <= LOG_TTL);
    }

    /// Append a finished event to the log. Live progress goes through
    /// `Msg::Progress` instead and never lands here. A repeat of the newest
    /// entry (e.g. the same error from consecutive auto-refreshes) only
    /// refreshes its timestamp.
    pub fn push_log(&mut self, s: String, tone: Tone) {
        if let Some(last) = self.log.last_mut()
            && last.0 == s
            && last.1 == tone
        {
            last.2 = Instant::now();
            return;
        }
        self.log.push((s, tone, Instant::now()));
        if self.log.len() > LOG_MAX {
            self.log.remove(0);
        }
    }

    /// Mark a user-visible operation as started. The user is acting again,
    /// so warnings and errors that outlived their TTL stop being pinned.
    pub fn begin(&mut self, label: &str) {
        self.busy = Some(label.to_string());
        self.progress = None;
        self.log.retain(|(_, _, at)| at.elapsed() <= LOG_TTL);
    }

    /// Indices into `devices` that pass the current filter.
    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.devices.len()).collect();
        }
        self.devices
            .iter()
            .enumerate()
            .filter(|(_, d)| fuzzy_match(&d.name, &self.filter))
            .map(|(i, _)| i)
            .collect()
    }

    /// The device under the cursor, honoring the filter.
    pub fn current(&self) -> Option<&Device> {
        self.visible().get(self.selected).map(|&i| &self.devices[i])
    }

    /// Rows shown in the scan picker: one per device name, however many
    /// addresses it advertises.
    pub fn scan_groups(&self) -> Vec<DiscoveredGroup> {
        match &self.view {
            View::ScanResults { items, .. } => bt::group_discovered(items),
            _ => Vec::new(),
        }
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Devices(mut devices) => {
                // Connected devices first, then alphabetical; keep the cursor
                // on the same device across refreshes.
                devices.sort_by_cached_key(|d| (!d.connected, d.name.to_lowercase()));
                let anchor = self.current().map(|d| d.mac.clone());
                self.devices = devices;
                let vis = self.visible();
                self.selected = anchor
                    .and_then(|mac| vis.iter().position(|&i| self.devices[i].mac == mac))
                    .unwrap_or(0)
                    .min(vis.len().saturating_sub(1));
            }
            Msg::ScanStarted => {
                self.ignore_scan_results = false;
                self.view = View::ScanResults {
                    items: Vec::new(),
                    selected: 0,
                };
            }
            Msg::Found(d) => {
                if let View::ScanResults { items, selected } = &mut self.view {
                    match items.iter_mut().find(|i| i.mac == d.mac) {
                        Some(existing) => {
                            existing.name = d.name;
                            existing.addr = d.addr;
                        }
                        None => items.push(d),
                    }
                    // A rename can merge two rows, shrinking the grouped list
                    // under the cursor; an out-of-range selection renders as a
                    // "3/2" counter with no highlight and a dead Enter.
                    let rows = bt::group_discovered(items).len();
                    *selected = (*selected).min(rows.saturating_sub(1));
                }
            }
            Msg::ScanResults(new_items) => {
                // The user closed the picker (or already picked a device)
                // mid-scan — the scan's final list must not reopen it.
                if self.ignore_scan_results {
                    self.ignore_scan_results = false;
                    return;
                }
                if !new_items.is_empty() {
                    // The final list arrives in bluetoothd cache order, not
                    // stream order — re-anchor the cursor by name (the rows
                    // are grouped by it) so it stays on the device the user
                    // highlighted.
                    let anchor = self
                        .scan_groups()
                        .get(match &self.view {
                            View::ScanResults { selected, .. } => *selected,
                            _ => 0,
                        })
                        .map(|g| g.name.clone());
                    let selected = anchor
                        .and_then(|name| {
                            bt::group_discovered(&new_items)
                                .iter()
                                .position(|g| g.name == name)
                        })
                        .unwrap_or(0);
                    self.view = View::ScanResults {
                        items: new_items,
                        selected,
                    };
                } else if matches!(&self.view, View::ScanResults { items, .. } if items.is_empty())
                {
                    // Empty scan, empty picker: nothing to choose from.
                    self.view = View::DeviceList;
                }
            }
            Msg::Status(s, tone) => self.push_log(s, tone),
            Msg::Progress(s) => self.progress = Some(s),
            Msg::PairPrompt { pin, passkey } => {
                // The prompt blocks the pairing until it is answered, so it
                // takes the screen back from the help overlay.
                self.help = false;
                self.view = View::PairPrompt {
                    pin,
                    passkey,
                    input: String::new(),
                };
            }
            Msg::Details { name, text } => {
                self.view = View::Details {
                    name,
                    text,
                    scroll: 0,
                };
            }
            Msg::OpDone => {
                self.pending = self.pending.saturating_sub(1);
                if self.pending == 0 {
                    self.busy = None;
                    self.progress = None;
                    self.last_done = Instant::now();
                }
            }
        }
    }

    /// Returns a command for the worker, if the key triggers one.
    /// The Ctrl- variants from the script (Ctrl-s/t/x/r) arrive as the same
    /// `KeyCode::Char` with a modifier, so the plain-letter arms cover both.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        // Ctrl-C always quits — raw mode delivers it as a plain 'c' keypress.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }

        // Pairing prompts answer while busy (the pairing op is running).
        if let View::PairPrompt { pin, input, .. } = &mut self.view {
            let pin = *pin;
            match key.code {
                KeyCode::Esc => {
                    self.view = View::DeviceList;
                    // "no" is a valid decline for yes/no prompts, but a PIN
                    // prompt would take it as the literal PIN — an empty
                    // reply tells the worker to abort the session instead.
                    let ans = if pin { String::new() } else { "no".into() };
                    return Some(Cmd::PairReply(ans));
                }
                KeyCode::Char('y') | KeyCode::Char('Y') if !pin => {
                    self.view = View::DeviceList;
                    return Some(Cmd::PairReply("yes".into()));
                }
                KeyCode::Char('n') | KeyCode::Char('N') if !pin => {
                    self.view = View::DeviceList;
                    return Some(Cmd::PairReply("no".into()));
                }
                KeyCode::Enter if pin => {
                    // An empty submit would read as a cancel — require input.
                    if input.is_empty() {
                        return None;
                    }
                    let ans = input.clone();
                    self.view = View::DeviceList;
                    return Some(Cmd::PairReply(ans));
                }
                KeyCode::Backspace if pin => {
                    input.pop();
                }
                KeyCode::Char(c) if pin && c.is_ascii_alphanumeric() => {
                    input.push(c);
                }
                _ => {}
            }
            return None;
        }

        // The help overlay swallows keys while it is up, ahead of the busy
        // gate below — otherwise j/k would scroll nothing and quietly move the
        // device cursor hidden behind the popup instead.
        if self.help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    self.help = false;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::Char('G') => self.help_scroll = u16::MAX,
                _ => {}
            }
            return None;
        }

        // It answers "what were the other keys?", so it opens from anywhere
        // except text entry — including mid-operation, where the border row
        // has the fewest chips left.
        if !self.filtering && key.code == KeyCode::Char('?') {
            self.help = true;
            self.help_scroll = 0;
            return None;
        }

        // Filter input mode captures typing.
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.selected = 0;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.selected = 0;
                }
                _ => {}
            }
            return None;
        }

        // While an operation runs, allow navigation, back, quit — and Enter
        // in the scan picker, so streamed-in devices are pairable the moment
        // they appear instead of only after the 10s scan ends.
        if self.busy.is_some() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => match self.view {
                    View::DeviceList => {
                        // Same semantics as when idle: Esc peels the filter
                        // before it means quit.
                        if key.code == KeyCode::Esc && !self.filter.is_empty() {
                            self.filter.clear();
                            self.selected = 0;
                        } else {
                            self.quit = true;
                        }
                    }
                    View::ScanResults { .. } => {
                        self.ignore_scan_results = true;
                        self.view = View::DeviceList;
                    }
                    _ => self.view = View::DeviceList,
                },
                KeyCode::Enter => {
                    let groups = self.scan_groups();
                    if let View::ScanResults { items, selected } = &self.view
                        && let Some(g) = groups.get(*selected)
                    {
                        let cmd = Cmd::PairConnect {
                            candidates: g.macs.clone(),
                            name: g.name.clone(),
                            others: items.clone(),
                        };
                        self.ignore_scan_results = true;
                        self.view = View::DeviceList;
                        self.begin("Pairing…");
                        // Cut the running scan short; the worker then moves
                        // straight on to the queued pair.
                        bt::kill_running_child();
                        return Some(cmd);
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
                _ => {}
            }
            return None;
        }

        match &mut self.view {
            View::DeviceList => match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc => {
                    // Esc peels the filter first, then quits.
                    if self.filter.is_empty() {
                        self.quit = true;
                    } else {
                        self.filter.clear();
                        self.selected = 0;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
                KeyCode::Char('g') => self.selected = 0,
                KeyCode::Char('G') => {
                    self.selected = self.visible().len().saturating_sub(1);
                }
                KeyCode::Char('/') => self.filtering = true,
                KeyCode::Enter => {
                    let d = self.current()?;
                    let cmd = Cmd::ToggleConnect {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                    };
                    // The worker re-checks live state and picks the action.
                    self.begin("Toggling connection…");
                    return Some(cmd);
                }
                KeyCode::Char('s') => {
                    self.begin("Scanning ~10s…");
                    return Some(Cmd::Scan);
                }
                KeyCode::Char('t') => {
                    let d = self.current()?;
                    let cmd = Cmd::ToggleTrust { mac: d.mac.clone() };
                    self.begin("Updating trust…");
                    return Some(cmd);
                }
                KeyCode::Char('a') => {
                    let d = self.current()?;
                    let cmd = Cmd::ToggleProfile {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                    };
                    self.begin("Switching audio profile…");
                    return Some(cmd);
                }
                KeyCode::Char('i') => {
                    let d = self.current()?;
                    let cmd = Cmd::Details {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                    };
                    self.begin("Loading info…");
                    return Some(cmd);
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    let d = self.current()?;
                    self.view = View::ConfirmRemove {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                    };
                }
                KeyCode::Char('r') => {
                    self.begin("Refreshing…");
                    return Some(Cmd::Refresh);
                }
                KeyCode::Char('d') => {
                    // Diagnostics work with no device selected too — a missing
                    // audio backend is a host problem, not a device one.
                    let device = self.current().map(|d| (d.mac.clone(), d.name.clone()));
                    self.begin("Running diagnostics…");
                    return Some(Cmd::Doctor { device });
                }
                _ => {}
            },
            View::ScanResults { .. } => {
                let groups = self.scan_groups();
                let View::ScanResults { items, selected } = &mut self.view else {
                    unreachable!("matched above")
                };
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        // A straggling final scan list must not reopen the
                        // picker the user just closed.
                        self.ignore_scan_results = true;
                        self.view = View::DeviceList;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        *selected = (*selected + 1).min(groups.len().saturating_sub(1));
                    }
                    KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Char('g') => *selected = 0,
                    KeyCode::Char('G') => *selected = groups.len().saturating_sub(1),
                    KeyCode::Enter => {
                        let g = groups.get(*selected)?;
                        let cmd = Cmd::PairConnect {
                            candidates: g.macs.clone(),
                            name: g.name.clone(),
                            // Hand the list to the worker so a pair failure can
                            // reopen the picker without another 10s scan.
                            others: items.clone(),
                        };
                        self.view = View::DeviceList;
                        self.begin("Pairing…");
                        return Some(cmd);
                    }
                    _ => {}
                }
            }
            View::ConfirmRemove { mac, .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let cmd = Cmd::Remove { mac: mac.clone() };
                    self.view = View::DeviceList;
                    self.begin("Removing…");
                    return Some(cmd);
                }
                // Default is No, like the script's `[y/N]`.
                _ => self.view = View::DeviceList,
            },
            View::Details { scroll, .. } => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('i') | KeyCode::Enter => {
                    self.view = View::DeviceList;
                }
                // The renderer clamps to the actual wrapped content, which
                // only it knows (depends on the popup width).
                KeyCode::Char('j') | KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Char('g') => *scroll = 0,
                KeyCode::Char('G') => *scroll = u16::MAX,
                _ => {}
            },
            View::PairPrompt { .. } => unreachable!("handled above"),
        }
        None
    }

    fn move_sel(&mut self, delta: i64) {
        let len = match &self.view {
            // The picker shows one row per group, not per address.
            View::ScanResults { .. } => self.scan_groups().len(),
            _ => self.visible().len(),
        };
        if len == 0 {
            return;
        }
        let sel = match &mut self.view {
            View::ScanResults { selected, .. } => selected,
            _ => &mut self.selected,
        };
        *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
    }
}

/// Case-insensitive subsequence match, fzf-style: every needle char must
/// appear in the haystack in order.
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let hay = haystack.to_lowercase();
    let mut it = hay.chars();
    needle
        .to_lowercase()
        .chars()
        .all(|n| it.by_ref().any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_devices() -> App {
        let mut app = App::new();
        app.handle_msg(Msg::Devices(vec![Device {
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: "Fake Headphones".into(),
            connected: true,
            trusted: true,
            battery: Some(80),
        }]));
        app
    }

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_match("WH-1000XM4", "wh4"));
        assert!(fuzzy_match("JBL Flip 5", "jblf5"));
        assert!(fuzzy_match("Anything", ""));
        assert!(!fuzzy_match("WH-1000XM4", "xm5"));
        assert!(!fuzzy_match("abc", "cba"));
    }

    #[test]
    fn slash_enters_filter_mode_and_enter_leaves_it() {
        let mut app = app_with_devices();
        assert!(app.handle_key(key(KeyCode::Char('/'))).is_none());
        assert!(app.filtering);
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.filter, "f");
        assert_eq!(app.visible().len(), 1);
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.filtering);
        assert_eq!(app.filter, "f");
        // Esc clears the filter before quitting.
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.quit);
        assert!(app.filter.is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn filter_with_no_matches_blocks_actions_safely() {
        let mut app = app_with_devices();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible().len(), 0);
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert!(app.busy.is_none());
    }

    /// A device's stable (classic) address.
    fn disc(mac: &str, name: &str) -> Discovered {
        Discovered {
            mac: mac.into(),
            name: name.into(),
            addr: bt::AddrKind::Public,
        }
    }

    /// One of the rotating LE addresses the same unit also advertises.
    fn disc_le(mac: &str, name: &str) -> Discovered {
        Discovered {
            mac: mac.into(),
            name: name.into(),
            addr: bt::AddrKind::Random,
        }
    }

    fn scanning_app() -> App {
        let mut app = app_with_devices();
        app.begin("Scanning ~10s…");
        app.pending = 1;
        app.handle_msg(Msg::ScanStarted);
        app.handle_msg(Msg::Found(disc("11:11:11:11:11:11", "Buds A")));
        app.handle_msg(Msg::Found(disc("22:22:22:22:22:22", "Buds B")));
        app
    }

    #[test]
    fn scan_results_reanchor_selection_by_mac() {
        let mut app = scanning_app();
        // Cursor onto "Buds B" (index 1 in stream order).
        app.handle_key(key(KeyCode::Char('j')));
        // Authoritative list arrives reordered: B is now index 0.
        app.handle_msg(Msg::ScanResults(vec![
            disc("22:22:22:22:22:22", "Buds B"),
            disc("11:11:11:11:11:11", "Buds A"),
        ]));
        match &app.view {
            View::ScanResults { items, selected } => {
                assert_eq!(items[*selected].mac, "22:22:22:22:22:22");
            }
            _ => panic!("picker should stay open"),
        }
    }

    #[test]
    fn dismissed_picker_is_not_reopened_by_final_results() {
        let mut app = scanning_app();
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.view, View::DeviceList));
        app.handle_msg(Msg::ScanResults(vec![disc("11:11:11:11:11:11", "Buds A")]));
        assert!(matches!(app.view, View::DeviceList));
        // The suppression is one-shot: a pair-failure reopen still works.
        app.handle_msg(Msg::ScanResults(vec![disc("11:11:11:11:11:11", "Buds A")]));
        assert!(matches!(app.view, View::ScanResults { .. }));
    }

    #[test]
    fn enter_pairs_mid_scan_and_suppresses_the_final_list() {
        let mut app = scanning_app();
        let cmd = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(cmd, Some(Cmd::PairConnect { ref candidates, .. })
                if candidates == &["11:11:11:11:11:11"]));
        assert!(matches!(app.view, View::DeviceList));
        // The scan's own final list is dropped; the pairing runs next.
        app.handle_msg(Msg::ScanResults(vec![disc("11:11:11:11:11:11", "Buds A")]));
        assert!(matches!(app.view, View::DeviceList));
    }

    /// One pair of earbuds advertising three addresses must be one row, and
    /// pairing it must hand the worker every address to try.
    #[test]
    fn picker_collapses_one_device_advertising_many_addresses() {
        let mut app = app_with_devices();
        app.begin("Scanning ~10s…");
        app.pending = 1;
        app.handle_msg(Msg::ScanStarted);
        app.handle_msg(Msg::Found(disc("40:7E:72:67:25:64", "Buds3 Pro")));
        app.handle_msg(Msg::Found(disc_le("7C:AF:C1:52:DC:A8", "Buds3 Pro")));
        app.handle_msg(Msg::Found(disc_le("A0:B0:BD:F3:BA:41", "Buds3 Pro")));
        app.handle_msg(Msg::Found(disc("78:C1:1D:12:D4:96", "Phone")));

        let groups = app.scan_groups();
        assert_eq!(groups.len(), 2, "three addresses, one device");
        assert_eq!(groups[0].macs.len(), 3);

        match app.handle_key(key(KeyCode::Enter)) {
            Some(Cmd::PairConnect {
                candidates, name, ..
            }) => {
                assert_eq!(name, "Buds3 Pro");
                assert_eq!(candidates.len(), 3, "worker gets every address to try");
            }
            _ => panic!("Enter should pair the highlighted group"),
        }
    }

    /// Navigation must step over rows, not over raw addresses — otherwise
    /// `j` appears to do nothing while the cursor walks a collapsed group.
    #[test]
    fn picker_navigation_steps_over_groups() {
        let mut app = app_with_devices();
        app.begin("Scanning ~10s…");
        app.pending = 1;
        app.handle_msg(Msg::ScanStarted);
        app.handle_msg(Msg::Found(disc("40:7E:72:67:25:64", "Buds3 Pro")));
        app.handle_msg(Msg::Found(disc_le("7C:AF:C1:52:DC:A8", "Buds3 Pro")));
        app.handle_msg(Msg::Found(disc("78:C1:1D:12:D4:96", "Phone")));

        app.handle_key(key(KeyCode::Char('j')));
        match app.handle_key(key(KeyCode::Enter)) {
            Some(Cmd::PairConnect { name, .. }) => assert_eq!(name, "Phone"),
            _ => panic!("one j should land on the second group"),
        }
    }

    #[test]
    fn doctor_key_works_with_no_device_selected() {
        let mut app = App::new();
        match app.handle_key(key(KeyCode::Char('d'))) {
            Some(Cmd::Doctor { device }) => assert!(device.is_none()),
            _ => panic!("d should run host-only diagnostics"),
        }
    }

    #[test]
    fn question_mark_toggles_the_help_popup() {
        let mut app = app_with_devices();
        assert!(app.handle_key(key(KeyCode::Char('?'))).is_none());
        assert!(app.help);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.help);
    }

    #[test]
    fn help_opens_while_busy() {
        // Exactly when it is needed most: mid-operation the border row is at
        // its shortest, so the dropped keys have to be reachable some way.
        let mut app = app_with_devices();
        app.begin("Scanning ~10s…");
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.help);
        assert!(!app.quit, "Esc should close the popup, not quit");
    }

    #[test]
    fn help_scrolls_while_busy_without_moving_the_hidden_cursor() {
        // j/k must reach the overlay, not the device list underneath it.
        let mut app = app_with_devices();
        app.handle_msg(Msg::Devices(vec![
            Device {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: "A".into(),
                connected: false,
                trusted: false,
                battery: None,
            },
            Device {
                mac: "11:22:33:44:55:66".into(),
                name: "B".into(),
                connected: false,
                trusted: false,
                battery: None,
            },
        ]));
        app.begin("Scanning ~10s…");
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.help_scroll, 1);
        assert_eq!(app.selected, 0, "the list behind the popup must not move");
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.help_scroll, 0);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.help_scroll, u16::MAX, "renderer clamps this");
    }

    #[test]
    fn help_preserves_the_view_underneath() {
        // Opening help over a running scan must not drop the picker: its
        // items would stop accumulating and closing help would strand the
        // user on the device list.
        let mut app = scanning_app();
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help);
        assert!(matches!(app.view, View::ScanResults { .. }));
        // Devices found while help is up still land in the picker.
        app.handle_msg(Msg::Found(disc("33:33:33:33:33:33", "Buds C")));
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.help);
        match &app.view {
            View::ScanResults { items, .. } => assert_eq!(items.len(), 3),
            _ => panic!("should return to the picker it was opened over"),
        }
    }

    #[test]
    fn help_over_a_confirmation_does_not_answer_it() {
        // y/n belong to the overlay while it is up; the pending remove must
        // survive untouched.
        let mut app = app_with_devices();
        app.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(app.view, View::ConfirmRemove { .. }));
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
        assert!(matches!(app.view, View::ConfirmRemove { .. }));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.help);
        assert!(matches!(app.view, View::ConfirmRemove { .. }));
    }

    #[test]
    fn a_pairing_prompt_takes_the_screen_back_from_help() {
        let mut app = app_with_devices();
        app.begin("Pairing…");
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help);
        app.handle_msg(Msg::PairPrompt {
            pin: false,
            passkey: "461829".into(),
        });
        assert!(!app.help, "the prompt blocks pairing until answered");
        assert!(matches!(app.view, View::PairPrompt { .. }));
    }

    #[test]
    fn question_mark_is_filter_text_not_a_shortcut() {
        let mut app = app_with_devices();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.filter, "?");
        assert!(matches!(app.view, View::DeviceList));
    }

    #[test]
    fn help_does_not_hijack_a_pairing_prompt() {
        // A PIN can legitimately contain '?'; answering the prompt wins.
        let mut app = app_with_devices();
        app.begin("Pairing…");
        app.handle_msg(Msg::PairPrompt {
            pin: true,
            passkey: String::new(),
        });
        app.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(app.view, View::PairPrompt { .. }));
        assert!(!app.help);
    }

    #[test]
    fn esc_while_busy_clears_filter_before_quitting() {
        let mut app = app_with_devices();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('f')));
        app.handle_key(key(KeyCode::Enter));
        app.begin("Updating trust…");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.quit);
        assert!(app.filter.is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn esc_on_pin_prompt_sends_cancel_not_a_pin() {
        let mut app = app_with_devices();
        app.begin("Pairing…");
        app.handle_msg(Msg::PairPrompt {
            pin: true,
            passkey: String::new(),
        });
        // Empty submit is a no-op, not an accidental cancel.
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert!(matches!(app.view, View::PairPrompt { .. }));
        match app.handle_key(key(KeyCode::Esc)) {
            Some(Cmd::PairReply(ans)) => assert!(ans.is_empty()),
            other => panic!("expected empty PairReply, got {:?}", other.is_some()),
        }
    }
}
