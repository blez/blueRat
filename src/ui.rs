//! Rendering: bordered binsider-style chrome — outer frame with the title in
//! the top border and `[key→action]` hints in the bottom border, a bordered
//! device panel with scrollbar, status line, and the confirm modal.
//!
//! Palette sampled from assets/banner.png: steel blue chrome, ice-blue
//! highlights, burnt-orange prompt accents, green for connected.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use crate::app::{App, View};
use crate::worker::Tone;

// Brand palette (assets/banner.png), brightened for terminal contrast.
const STEEL: Color = Color::Rgb(0x4a, 0x7f, 0xb8); // outer frame
const STEEL_DIM: Color = Color::Rgb(0x31, 0x4d, 0x70); // panel borders, tracks
const BLUE: Color = Color::Rgb(0x6a, 0xa3, 0xe0); // wordmark "blue", panel titles
const ICE: Color = Color::Rgb(0x7f, 0xd6, 0xff); // bright highlights, counters
const SNOW: Color = Color::Rgb(0xee, 0xf6, 0xff); // bright text, wordmark "Rat"
const TEXT: Color = Color::Rgb(0xc2, 0xd4, 0xe8); // regular text
const SLATE: Color = Color::Rgb(0x77, 0x8d, 0xa8); // muted text, MACs, brackets
const ORANGE: Color = Color::Rgb(0xff, 0x8c, 0x4a); // keys, prompt ❯, busy
const GREEN: Color = Color::Rgb(0x5f, 0xd7, 0x84); // connected / success
const RED: Color = Color::Rgb(0xf2, 0x6d, 0x6d); // destructive / failure
const SEL_BG: Color = Color::Rgb(0x2e, 0x54, 0x80); // selection bar

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// Responsive log layout. Invariant: at the breakpoint, the side panel takes
// max(SIDE_LOG_MIN, 35%) columns and the device panel keeps at least
// CONTENT_MIN — SIDE_LOG_BREAKPOINT - SIDE_LOG_MIN must stay >= CONTENT_MIN.
const SIDE_LOG_BREAKPOINT: u16 = 90;
const SIDE_LOG_RATIO: u32 = 35; // percent of total width
const SIDE_LOG_MIN: u16 = 34;
const CONTENT_MIN: u16 = 40;

/// Widest the details popup may grow, as a percentage of the terminal. It
/// shrinks to fit its content, so this only bites when a line is genuinely
/// longer than the screen can comfortably show.
const DETAILS_MAX_RATIO: u32 = 90;
/// Columns of breathing room between the details text and its border.
const DETAILS_PAD: u16 = 1;
/// Narrowest a popup may be before it just takes whatever the screen has.
const POPUP_MIN_W: u16 = 20;

/// Width for a popup that would like `want` columns: at least readable, never
/// more than `DETAILS_MAX_RATIO` of the screen, never wider than the screen.
///
/// The order matters. `clamp` panics when min > max, so a terminal narrower
/// than `POPUP_MIN_W` must not be allowed to produce `clamp(20, 19)`.
fn popup_width(want: u16, screen_w: u16) -> u16 {
    let cap = ((screen_w as u32 * DETAILS_MAX_RATIO / 100) as u16)
        .max(POPUP_MIN_W)
        .min(screen_w.max(1));
    want.max(POPUP_MIN_W.min(cap)).min(cap)
}

/// Where the event log renders this frame.
enum LogSlot {
    Bottom(Rect),
    Side(Rect),
}

/// One `[key→action]` chip in the bottom border.
struct Hint {
    key: &'static str,
    action: &'static str,
    /// Drop order when the row is wider than the terminal: highest goes
    /// first. Values are distinct within a row so the order is explicit
    /// rather than an accident of how ties are broken.
    prio: u8,
}

const fn h(key: &'static str, action: &'static str, prio: u8) -> Hint {
    Hint { key, action, prio }
}

// The last two standing are `q` and `?`: how to leave, and how to find
// everything this row had to drop.
const HINTS_DEVICE_LIST: [Hint; 11] = [
    h("Enter", "Toggle", 2),
    h("s", "Scan", 3),
    h("a", "Audio", 6),
    h("i", "Info", 5),
    h("/", "Filter", 7),
    h("t", "Trust", 8),
    h("x", "Remove", 9),
    h("d", "Doctor", 4),
    h("r", "Refresh", 10),
    h("?", "Keys", 1),
    h("q", "Quit", 0),
];

enum HelpRow {
    Section(&'static str),
    Key(&'static str, &'static str),
}

use HelpRow::{Key, Section};

/// The full binding list behind `?`. The border row above is a deliberately
/// shortened subset, so this is the authoritative one and the place to add a
/// new key.
const HELP: [HelpRow; 21] = [
    Section("Navigation"),
    Key("j / ↓", "move down"),
    Key("k / ↑", "move up"),
    Key("g / G", "jump to first / last"),
    Section("Devices"),
    Key("Enter", "connect / disconnect"),
    Key("s", "scan & pair a new device"),
    Key("t", "trust / untrust"),
    Key("x / Del", "remove (asks first)"),
    Key("r", "refresh now"),
    Section("Audio"),
    Key("a", "switch profile (A2DP ↔ mic)"),
    Key("d", "audio diagnostics"),
    Section("Finding things"),
    Key("/", "filter by name"),
    Key("i", "device details"),
    Key("?", "this list"),
    Section("Leaving"),
    Key("Esc", "back — clears the filter first"),
    Key("q", "quit (back, in a popup)"),
    Key("Ctrl-C", "quit from anywhere"),
];
const HINTS_SCAN: [Hint; 3] = [
    h("Enter", "Pair", 1),
    h("j/k", "Move", 2),
    h("Esc", "Back", 0),
];
const HINTS_DETAILS: [Hint; 2] = [h("j/k", "Scroll", 1), h("Esc", "Close", 0)];
const HINTS_PIN: [Hint; 2] = [h("Enter", "Submit", 1), h("Esc", "Cancel", 0)];
const HINTS_YES_NO: [Hint; 2] = [h("y", "Yes", 1), h("n", "No", 0)];

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn bold(c: Color) -> Style {
    Style::new().fg(c).bold()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    // The overlay owns the keyboard while it is up, so it owns the hint row.
    let hints: &[Hint] = if app.help {
        &HINTS_DETAILS
    } else {
        match &app.view {
            View::ScanResults { .. } => &HINTS_SCAN,
            View::ConfirmRemove { .. } => &HINTS_YES_NO,
            View::Details { .. } => &HINTS_DETAILS,
            View::PairPrompt { pin: true, .. } => &HINTS_PIN,
            View::PairPrompt { .. } => &HINTS_YES_NO,
            View::DeviceList => &HINTS_DEVICE_LIST,
        }
    };
    // The bottom border is the only room the hints get: the two corner glyphs
    // are not ours to write over.
    let hint_budget = frame.area().width.saturating_sub(2);

    let outer = Block::bordered()
        .border_style(fg(STEEL))
        .title_top(
            Line::from(vec![
                Span::styled("┤ ", fg(STEEL)),
                Span::styled("󰂯 ", fg(ICE)),
                Span::styled("blue", bold(BLUE)),
                Span::styled("Rat", bold(SNOW)),
                Span::styled(concat!(" v", env!("CARGO_PKG_VERSION")), fg(SLATE)),
                Span::styled(" ├", fg(STEEL)),
            ])
            .centered(),
        )
        .title_bottom(hint_line(&fit_hints(hints, hint_budget)).centered());
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let log = log_lines(app);
    // Wide terminals get a banner-style console panel on the right (stable
    // layout, mirrors the banner composition); narrow ones keep the log at
    // the bottom.
    let (panel_area, log_slot) = if inner.width >= SIDE_LOG_BREAKPOINT {
        // At least 35% of the width, never below the readable minimum.
        // u32 math: u16 would overflow at ~1873 columns.
        let side_w = ((inner.width as u32 * SIDE_LOG_RATIO / 100) as u16).max(SIDE_LOG_MIN);
        let [content, side] =
            Layout::horizontal([Constraint::Min(CONTENT_MIN), Constraint::Length(side_w)])
                .areas(inner);
        (content, LogSlot::Side(side))
    } else {
        // Constant height while anything is logged or running, so the device
        // list doesn't jump on every entry push/expiry.
        let h = if log.is_empty() {
            1
        } else {
            crate::app::LOG_MAX as u16 + 1
        };
        let [content, bottom] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(h)]).areas(inner);
        (content, LogSlot::Bottom(bottom))
    };

    // One row per device name; a device advertising several addresses is a
    // single pickable entry (the worker chooses which address to pair).
    let scan_groups = app.scan_groups();

    match &app.view {
        View::ScanResults { selected, .. } => {
            if scan_groups.is_empty() {
                let block = panel_block(Line::from(Span::styled(" 󰐷 New Devices ", bold(ORANGE))));
                frame.render_widget(&block, panel_area);
                let avail = block.inner(panel_area);
                let msg_area = center(avail, 40, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "listening for devices…",
                        fg(SLATE),
                    )))
                    .centered(),
                    msg_area,
                );
            } else {
                let rows: Vec<ListItem> = scan_groups
                    .iter()
                    .map(|g| {
                        let mut spans = vec![
                            Span::styled("󰐗 ", fg(ORANGE)),
                            Span::styled(g.name.clone(), fg(SNOW)),
                            Span::styled(format!("  {}", g.macs[0]), fg(SLATE)),
                        ];
                        // Earbuds announce several addresses at once; say so
                        // rather than showing the same name three times.
                        if g.macs.len() > 1 {
                            spans.push(Span::styled(
                                format!("  +{} more", g.macs.len() - 1),
                                fg(ICE),
                            ));
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect();
                render_panel(
                    frame,
                    panel_area,
                    Line::from(Span::styled(" 󰐷 New Devices ", bold(ORANGE))),
                    None,
                    rows,
                    *selected,
                    scan_groups.len(),
                );
            }
        }
        _ => {
            let vis = app.visible();
            let rows: Vec<ListItem> = vis
                .iter()
                .map(|&idx| {
                    let d = &app.devices[idx];
                    let (mark, name_style) = if d.connected {
                        (Span::styled("󰂱 ", fg(GREEN)), bold(SNOW))
                    } else {
                        (Span::styled("󰂯 ", fg(SLATE)), fg(TEXT))
                    };
                    let mut spans = vec![mark, Span::styled(d.name.clone(), name_style)];
                    if d.connected {
                        spans.push(Span::styled(" [CONNECTED]", fg(GREEN)));
                    }
                    if let Some(b) = d.battery {
                        let c = if b >= 50 {
                            GREEN
                        } else if b >= 20 {
                            ORANGE
                        } else {
                            RED
                        };
                        spans.push(Span::styled(format!(" 󰁹 {b}%"), fg(c)));
                    }
                    if d.trusted {
                        spans.push(Span::styled(" 󰒘 trusted", fg(SLATE)));
                    }
                    spans.push(Span::styled(format!("  {}", d.mac), fg(SLATE)));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let connected = app.devices.iter().filter(|d| d.connected).count();
            let badge = (connected > 0).then(|| {
                Line::from(vec![
                    Span::styled("┤", fg(STEEL_DIM)),
                    Span::styled(format!("● {connected} connected"), fg(GREEN)),
                    Span::styled("├", fg(STEEL_DIM)),
                ])
            });
            let mut title_spans = vec![Span::styled(" 󰂯 Paired Devices ", bold(BLUE))];
            if app.filtering || !app.filter.is_empty() {
                title_spans.push(Span::styled(
                    format!("/{}{} ", app.filter, if app.filtering { "▏" } else { "" }),
                    bold(ORANGE),
                ));
            }
            let title = Line::from(title_spans);
            if app.devices.is_empty() {
                let block = panel_block(title);
                frame.render_widget(&block, panel_area);
                let avail = block.inner(panel_area);
                let lines = vec![
                    Line::from(Span::styled("󰂲", bold(BLUE))),
                    Line::default(),
                    Line::from(Span::styled("No paired devices", fg(SNOW))),
                    Line::from(vec![
                        Span::styled("press ", fg(SLATE)),
                        Span::styled("s", bold(ORANGE)),
                        Span::styled(" to scan & pair", fg(SLATE)),
                    ]),
                ];
                let msg_area = center(avail, 40, (lines.len() as u16).min(avail.height));
                frame.render_widget(Paragraph::new(lines).centered(), msg_area);
            } else {
                render_panel(
                    frame,
                    panel_area,
                    title,
                    badge,
                    rows,
                    app.selected,
                    vis.len(),
                );
            }
        }
    }

    match log_slot {
        LogSlot::Bottom(area) => {
            let lines = fit_tail(log, area.width, area.height);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
        }
        LogSlot::Side(area) => {
            let block = Block::bordered()
                .border_style(fg(STEEL_DIM))
                .title(Line::from(vec![
                    Span::styled(" [", fg(SLATE)),
                    Span::styled("blueRat", bold(BLUE)),
                    Span::styled("] ", fg(SLATE)),
                ]));
            let content = block.inner(area);
            frame.render_widget(block, area);
            let lines = fit_tail(log, content.width, content.height);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
        }
    }

    if let View::Details { name, text, scroll } = &mut app.view {
        let src: Vec<String> = text.lines().map(|l| l.replace('\t', "  ")).collect();
        // Size to the content rather than a fixed column count: `bluetoothctl
        // info` runs to ~72 columns, and wrapping every UUID line doubled the
        // popup's height for nothing. The 90% cap keeps it a popup on narrow
        // terminals; fitting the content keeps it from becoming a mostly-empty
        // banner on wide ones.
        let longest = src
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .min(u16::MAX as usize - 4) as u16;
        // "┤ 󰋽 " + name + " ├"
        let title_w = name.chars().count().min(u16::MAX as usize - 8) as u16 + 6;
        // Borders plus the padding the block will eat on both sides.
        let chrome = 2 + 2 * DETAILS_PAD;
        let width = popup_width(longest.max(title_w) + chrome, frame.area().width);
        let inner_w = width.saturating_sub(chrome).max(1) as usize;
        // Wrap by hand so the row count is exact — Paragraph's Wrap would
        // silently clip whatever the (line-count-based) height missed.
        let mut rows: Vec<String> = Vec::new();
        for l in src {
            let chars: Vec<char> = l.chars().collect();
            if chars.is_empty() {
                rows.push(String::new());
            } else {
                for chunk in chars.chunks(inner_w) {
                    rows.push(chunk.iter().collect());
                }
            }
        }
        let height = (rows.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
        let visible = height.saturating_sub(2);
        let max_scroll = (rows.len() as u16).saturating_sub(visible);
        *scroll = (*scroll).min(max_scroll);
        let start = *scroll as usize;
        let end = (start + visible as usize).min(rows.len());
        let lines: Vec<Line> = rows[start..end]
            .iter()
            .map(|r| Line::from(Span::styled(r.clone(), fg(TEXT))))
            .collect();
        let mut block = Block::bordered()
            .border_style(fg(STEEL))
            .padding(Padding::horizontal(DETAILS_PAD))
            .title(Line::from(vec![
                Span::styled("┤ 󰋽 ", fg(STEEL)),
                Span::styled(name.clone(), bold(BLUE)),
                Span::styled(" ├", fg(STEEL)),
            ]));
        if max_scroll > 0 {
            block = block.title_bottom(
                Line::from(vec![
                    Span::styled("┤", fg(STEEL)),
                    Span::styled(format!("{}–{}/{}", start + 1, end, rows.len()), bold(ICE)),
                    Span::styled("├", fg(STEEL)),
                ])
                .right_aligned(),
            );
        }
        let area = center(frame.area(), width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    if let View::PairPrompt {
        pin,
        passkey,
        input,
    } = &app.view
    {
        let text = if *pin {
            vec![
                Line::from(Span::styled("Enter PIN code for pairing:", fg(TEXT))),
                Line::default(),
                Line::from(Span::styled(format!("{input}▏"), bold(SNOW))),
            ]
        } else {
            let shown = if passkey.is_empty() {
                "Accept pairing request?".to_string()
            } else {
                format!("Passkey: {passkey}")
            };
            vec![
                Line::from(Span::styled("Confirm this matches the device:", fg(TEXT))),
                Line::from(Span::styled(shown, bold(ICE))),
                Line::from(vec![
                    Span::styled("y", bold(GREEN)),
                    Span::styled(" yes    ", fg(TEXT)),
                    Span::styled("n", bold(RED)),
                    Span::styled(" no", fg(TEXT)),
                ]),
            ]
        };
        let area = center(frame.area(), 42, 5);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text).centered().block(
                Block::bordered()
                    .border_style(fg(ORANGE))
                    .title(Span::styled("┤ 󰌆 Pairing ├", bold(ORANGE))),
            ),
            area,
        );
    }

    if let View::ConfirmRemove { name, mac } = &app.view {
        // Width fits the longest line, "Remove (unpair) <name>?" (17 cells of
        // fixed text), measured in chars, not bytes. On narrow terminals the
        // paragraph wraps, so grow the height by the extra rows needed.
        let name_w = name.chars().count() as u16;
        let width = (name_w + 21)
            .max(34)
            .min(frame.area().width.saturating_sub(2));
        let inner = width.saturating_sub(2).max(1);
        let extra = (17 + name_w).saturating_sub(1) / inner;
        let area = center(frame.area(), width, 5 + extra);
        frame.render_widget(Clear, area);
        let text = vec![
            Line::from(vec![
                Span::styled("Remove (unpair) ", fg(TEXT)),
                Span::styled(name.clone(), bold(SNOW)),
                Span::styled("?", fg(TEXT)),
            ]),
            Line::from(Span::styled(mac.clone(), fg(SLATE))),
            Line::from(vec![
                Span::styled("y", bold(RED)),
                Span::styled(" yes    ", fg(TEXT)),
                Span::styled("n", bold(ICE)),
                Span::styled(" no", fg(TEXT)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(text)
                .centered()
                .wrap(Wrap { trim: true })
                .block(
                    Block::bordered()
                        .border_style(fg(RED))
                        .title(Span::styled("┤ 󰩹 Remove ├", bold(RED))),
                ),
            area,
        );
    }

    // Last, so the overlay sits above every popup it can be opened over.
    if app.help {
        // Key column wide enough for the longest binding, so the descriptions
        // line up into a readable second column.
        let key_w = HELP
            .iter()
            .filter_map(|r| match r {
                Key(k, _) => Some(k.chars().count()),
                Section(_) => None,
            })
            .max()
            .unwrap_or(0);
        let rows: Vec<Line> = HELP
            .iter()
            .map(|row| match row {
                Section(title) => Line::from(vec![
                    Span::styled("─ ", fg(STEEL_DIM)),
                    Span::styled(*title, bold(BLUE)),
                ]),
                Key(k, what) => Line::from(vec![
                    Span::styled(format!("  {k:>key_w$}"), bold(ORANGE)),
                    Span::styled("  ", fg(TEXT)),
                    Span::styled(*what, fg(TEXT)),
                ]),
            })
            .collect();
        let longest = rows.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let chrome = 2 + 2 * DETAILS_PAD;
        let width = popup_width(longest + chrome, frame.area().width);

        let height = (rows.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
        let visible = height.saturating_sub(2) as usize;
        let max_scroll = rows.len().saturating_sub(visible) as u16;
        app.help_scroll = app.help_scroll.min(max_scroll);
        let start = app.help_scroll as usize;
        let end = (start + visible).min(rows.len());

        let mut block = Block::bordered()
            .border_style(fg(STEEL))
            .padding(Padding::horizontal(DETAILS_PAD))
            .title(Line::from(vec![
                Span::styled("┤ 󰌌 ", fg(STEEL)),
                Span::styled("Keys", bold(BLUE)),
                Span::styled(" ├", fg(STEEL)),
            ]));
        if max_scroll > 0 {
            block = block.title_bottom(
                Line::from(vec![
                    Span::styled("┤", fg(STEEL)),
                    Span::styled(format!("{}–{}/{}", start + 1, end, rows.len()), bold(ICE)),
                    Span::styled("├", fg(STEEL)),
                ])
                .right_aligned(),
            );
        }
        let area = center(frame.area(), width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(rows[start..end].to_vec()).block(block), area);
    }
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Err => bold(RED),
        Tone::Warn => fg(ORANGE),
        Tone::Ok => fg(GREEN),
        Tone::Info => fg(ICE),
    }
}

/// Banner-console-style event log: `❯`-prefixed lines colored by severity,
/// all but the newest dimmed, plus a spinner line carrying the live progress
/// text while an operation runs.
fn log_lines(app: &App) -> Vec<Line<'static>> {
    let n = app.log.len();
    let mut lines: Vec<Line> = app
        .log
        .iter()
        .enumerate()
        .map(|(i, (msg, tone, _))| {
            let (prompt, style) = if i + 1 == n {
                (bold(ORANGE), tone_style(*tone))
            } else {
                (bold(ORANGE).dim(), tone_style(*tone).dim())
            };
            Line::from(vec![
                Span::styled(" ❯ ", prompt),
                Span::styled(msg.clone(), style),
            ])
        })
        .collect();
    if let Some(busy) = &app.busy {
        let spin = SPINNER[app.tick as usize % SPINNER.len()];
        let text = app.progress.clone().unwrap_or_else(|| busy.clone());
        lines.push(Line::from(vec![
            Span::styled(format!(" {spin} "), bold(ORANGE)),
            Span::styled(text, fg(SNOW)),
        ]));
    }
    lines
}

/// Keep the newest lines that fit `height` rows at `width` (accounting for
/// wrapping), dropping the oldest — the spinner and latest events must never
/// be the ones clipped.
fn fit_tail(lines: Vec<Line<'static>>, width: u16, height: u16) -> Vec<Line<'static>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut rows = 0u16;
    let mut kept: Vec<Line> = Vec::new();
    for line in lines.into_iter().rev() {
        let need = (line.width().max(1) as u16).div_ceil(width);
        if rows + need > height {
            break;
        }
        rows += need;
        kept.push(line);
    }
    kept.reverse();
    kept
}

/// Rendered width of a chip row: `[key→action]` per hint, `─` between.
fn hints_width(hints: &[&Hint]) -> usize {
    hints
        .iter()
        .map(|h| h.key.chars().count() + h.action.chars().count() + 3)
        .sum::<usize>()
        + hints.len().saturating_sub(1)
}

/// Drop the least important chips until the row fits the border it lives in.
/// Overflowing instead would clip the row mid-chip and, because the title is
/// centered, eat the keys at *both* ends — including how to quit.
fn fit_hints(hints: &[Hint], budget: u16) -> Vec<&Hint> {
    let mut sel: Vec<&Hint> = hints.iter().collect();
    while hints_width(&sel) > budget as usize && sel.len() > 1 {
        // max_by_key returns the last maximum, so ties drop right-to-left.
        let idx = sel
            .iter()
            .enumerate()
            .max_by_key(|(_, h)| h.prio)
            .map(|(i, _)| i)
            .unwrap();
        sel.remove(idx);
    }
    if hints_width(&sel) > budget as usize {
        sel.clear();
    }
    sel
}

/// `─[Enter→Toggle]─[s→Pair new]─…` chips for the bottom border.
fn hint_line(hints: &[&Hint]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, hint) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("─", fg(STEEL)));
        }
        spans.push(Span::styled("[", fg(SLATE)));
        spans.push(Span::styled(hint.key, bold(ORANGE)));
        spans.push(Span::styled("→", fg(SLATE)));
        spans.push(Span::styled(hint.action, fg(TEXT)));
        spans.push(Span::styled("]", fg(SLATE)));
    }
    Line::from(spans)
}

fn panel_block(title: Line<'static>) -> Block<'static> {
    Block::bordered().border_style(fg(STEEL_DIM)).title(title)
}

fn render_panel(
    frame: &mut Frame,
    area: Rect,
    title: Line<'static>,
    badge: Option<Line<'static>>,
    rows: Vec<ListItem>,
    selected: usize,
    len: usize,
) {
    let pos = if len == 0 { 0 } else { selected + 1 };
    let mut block = panel_block(title).title_bottom(
        Line::from(vec![
            Span::styled("┤", fg(STEEL_DIM)),
            Span::styled(format!("{pos}/{len}"), bold(ICE)),
            Span::styled("├", fg(STEEL_DIM)),
        ])
        .right_aligned(),
    );
    if let Some(badge) = badge {
        block = block.title_bottom(badge.left_aligned());
    }
    let list_area = block.inner(area);
    frame.render_widget(block, area);

    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(rows)
        .highlight_style(Style::new().bg(SEL_BG).fg(SNOW))
        .scroll_padding(1);
    frame.render_stateful_widget(list, list_area, &mut state);

    if len > list_area.height as usize {
        let mut sb_state = ScrollbarState::new(len).position(selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(fg(BLUE))
                .track_style(fg(STEEL_DIM)),
            area.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut sb_state,
        );
    }
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(sel: &[&Hint]) -> Vec<&'static str> {
        sel.iter().map(|h| h.key).collect()
    }

    #[test]
    fn wide_terminal_keeps_every_hint() {
        let sel = fit_hints(&HINTS_DEVICE_LIST, 200);
        assert_eq!(sel.len(), HINTS_DEVICE_LIST.len());
    }

    #[test]
    fn narrow_terminal_drops_hints_instead_of_clipping() {
        // The laptop case: a ~75-column window cannot hold the full 106-column
        // row, and a clipped row loses the keys at both ends.
        let budget = 73;
        let sel = fit_hints(&HINTS_DEVICE_LIST, budget);
        assert!(hints_width(&sel) <= budget as usize);
        assert!(sel.len() < HINTS_DEVICE_LIST.len());
        // Whatever survives, these do.
        let k = keys(&sel);
        assert!(k.contains(&"Enter"), "got {k:?}");
        assert!(k.contains(&"?"), "got {k:?}");
        assert!(k.contains(&"q"), "got {k:?}");
        // Display order is preserved, not shuffled by priority.
        let order: Vec<usize> = k
            .iter()
            .map(|key| {
                HINTS_DEVICE_LIST
                    .iter()
                    .position(|h| h.key == *key)
                    .unwrap()
            })
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "got {order:?}");
    }

    #[test]
    fn very_narrow_terminal_keeps_the_way_out_and_the_way_in() {
        // Too narrow even for Enter: what has to survive is how to leave and
        // how to look up everything that was dropped.
        let sel = fit_hints(&HINTS_DEVICE_LIST, 30);
        assert!(hints_width(&sel) <= 30);
        assert_eq!(keys(&sel), vec!["?", "q"]);
    }

    #[test]
    fn popup_width_survives_a_terminal_narrower_than_the_minimum() {
        // `clamp(20, 19)` panics, and it panicked inside draw() — every frame,
        // unrecoverably — the moment `?` was pressed on a 19-column terminal.
        for screen in 0..=25u16 {
            let w = popup_width(80, screen);
            assert!(
                w <= screen.max(1),
                "popup wider than the screen at {screen}"
            );
            assert!(w >= 1, "zero-width popup at {screen}");
        }
    }

    #[test]
    fn popup_width_fits_content_up_to_the_cap() {
        // Short content gets a short popup; long content stops at 90%.
        assert_eq!(popup_width(40, 200), 40);
        assert_eq!(popup_width(400, 200), 180);
        // Never below the readable minimum while the screen allows it.
        assert_eq!(popup_width(5, 200), POPUP_MIN_W);
    }

    #[test]
    fn every_hint_row_has_distinct_priorities() {
        // Ties would make the drop order depend on max_by_key's tie-break
        // rather than on intent — that is how `q` once got dropped first.
        for set in [
            &HINTS_DEVICE_LIST[..],
            &HINTS_SCAN[..],
            &HINTS_DETAILS[..],
            &HINTS_PIN[..],
            &HINTS_YES_NO[..],
        ] {
            let mut prios: Vec<u8> = set.iter().map(|h| h.prio).collect();
            prios.sort_unstable();
            let before = prios.len();
            prios.dedup();
            assert_eq!(prios.len(), before, "duplicate priority in a hint row");
        }
    }

    #[test]
    fn hopeless_width_yields_no_chips_rather_than_a_broken_one() {
        assert!(fit_hints(&HINTS_DEVICE_LIST, 3).is_empty());
        assert!(fit_hints(&HINTS_DEVICE_LIST, 0).is_empty());
    }

    #[test]
    fn short_hint_rows_are_untouched() {
        for set in [&HINTS_SCAN[..], &HINTS_DETAILS[..], &HINTS_YES_NO[..]] {
            assert_eq!(fit_hints(set, 80).len(), set.len());
        }
    }
}
