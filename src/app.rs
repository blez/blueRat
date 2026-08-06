//! Application state and key/message handling.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bt::{Device, Discovered};
use crate::worker::{Cmd, Msg, Tone};

const STATUS_TTL: Duration = Duration::from_secs(4);

pub enum View {
    DeviceList,
    ScanResults {
        items: Vec<Discovered>,
        selected: usize,
    },
    ConfirmRemove {
        mac: String,
        name: String,
    },
}

pub struct App {
    pub devices: Vec<Device>,
    pub selected: usize,
    pub view: View,
    pub busy: Option<String>,
    pub status: Option<(String, Tone, Instant)>,
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
            status: None,
            tick: 0,
            quit: false,
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Some((_, _, at)) = &self.status
            && at.elapsed() > STATUS_TTL
        {
            self.status = None;
        }
    }

    pub fn set_status(&mut self, s: String, tone: Tone) {
        self.status = Some((s, tone, Instant::now()));
    }

    /// Mark an operation as started: set the busy label and drop any leftover
    /// status so the previous op's result isn't shown as live progress.
    fn begin(&mut self, label: &str) {
        self.busy = Some(label.to_string());
        self.status = None;
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Devices(devices) => {
                // Keep the cursor on the same device across refreshes.
                let anchor = self.devices.get(self.selected).map(|d| d.mac.clone());
                self.devices = devices;
                self.selected = anchor
                    .and_then(|mac| self.devices.iter().position(|d| d.mac == mac))
                    .unwrap_or(0);
                if !self.devices.is_empty() {
                    self.selected = self.selected.min(self.devices.len() - 1);
                }
            }
            Msg::ScanResults(items) => {
                if !items.is_empty() {
                    self.view = View::ScanResults { items, selected: 0 };
                }
            }
            Msg::Status(s, tone) => self.set_status(s, tone),
            Msg::OpDone => self.busy = None,
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

        // While an operation runs, allow only navigation, back, and quit.
        // q/Esc must mean "back" in sub-views even here — the scan picker is
        // shown while the post-scan refresh is still running, and quitting
        // the whole app from it would contradict the visible hints.
        if self.busy.is_some() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => match self.view {
                    View::DeviceList => self.quit = true,
                    _ => self.view = View::DeviceList,
                },
                KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
                _ => {}
            }
            return None;
        }

        match &mut self.view {
            View::DeviceList => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
                KeyCode::Char('g') => self.selected = 0,
                KeyCode::Char('G') => {
                    self.selected = self.devices.len().saturating_sub(1);
                }
                KeyCode::Enter => {
                    let d = self.devices.get(self.selected)?;
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
                    let d = self.devices.get(self.selected)?;
                    let cmd = Cmd::ToggleTrust { mac: d.mac.clone() };
                    self.begin("Updating trust…");
                    return Some(cmd);
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    let d = self.devices.get(self.selected)?;
                    self.view = View::ConfirmRemove {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                    };
                }
                KeyCode::Char('r') => {
                    self.begin("Refreshing…");
                    return Some(Cmd::Refresh);
                }
                _ => {}
            },
            View::ScanResults { items, selected } => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.view = View::DeviceList,
                KeyCode::Char('j') | KeyCode::Down => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                }
                KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Char('g') => *selected = 0,
                KeyCode::Char('G') => *selected = items.len().saturating_sub(1),
                KeyCode::Enter => {
                    let d = items.get(*selected)?;
                    let cmd = Cmd::PairConnect {
                        mac: d.mac.clone(),
                        name: d.name.clone(),
                        // Hand the list to the worker so a pair failure can
                        // reopen the picker without another 10s scan.
                        others: items.clone(),
                    };
                    self.view = View::DeviceList;
                    self.begin("Pairing…");
                    return Some(cmd);
                }
                _ => {}
            },
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
        }
        None
    }

    fn move_sel(&mut self, delta: i64) {
        let len = match &self.view {
            View::ScanResults { items, .. } => items.len(),
            _ => self.devices.len(),
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
