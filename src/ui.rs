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
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use crate::app::{App, View};
use crate::worker::Tone;

// Brand palette (assets/banner.png).
const STEEL: Color = Color::Rgb(0x35, 0x61, 0x8c); // outer frame
const STEEL_DIM: Color = Color::Rgb(0x25, 0x38, 0x50); // panel borders, tracks
const BLUE: Color = Color::Rgb(0x53, 0x88, 0xbd); // wordmark "blue", panel titles
const ICE: Color = Color::Rgb(0x69, 0xc4, 0xe6); // bright highlights, counters
const SNOW: Color = Color::Rgb(0xdc, 0xe8, 0xf5); // bright text, wordmark "Rat"
const TEXT: Color = Color::Rgb(0xa8, 0xb8, 0xcc); // regular text
const SLATE: Color = Color::Rgb(0x5d, 0x6f, 0x85); // muted text, MACs, brackets
const ORANGE: Color = Color::Rgb(0xd9, 0x77, 0x42); // keys, prompt ❯, busy
const GREEN: Color = Color::Rgb(0x59, 0xa9, 0x6d); // connected / success
const RED: Color = Color::Rgb(0xd1, 0x69, 0x69); // destructive / failure
const SEL_BG: Color = Color::Rgb(0x24, 0x40, 0x5e); // selection bar

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn bold(c: Color) -> Style {
    Style::new().fg(c).bold()
}

pub fn draw(frame: &mut Frame, app: &App) {
    let hints: &[(&str, &str)] = match &app.view {
        View::ScanResults { .. } => &[("Enter", "Pair"), ("j/k", "Move"), ("Esc", "Back")],
        View::ConfirmRemove { .. } => &[("y", "Yes"), ("n", "No")],
        View::DeviceList => &[
            ("Enter", "Toggle"),
            ("s", "Pair new"),
            ("t", "Trust"),
            ("x", "Remove"),
            ("r", "Refresh"),
            ("q", "Quit"),
        ],
    };

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
        .title_bottom(hint_line(hints).centered());
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let [panel_area, status_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);

    match &app.view {
        View::ScanResults { items, selected } => {
            let rows: Vec<ListItem> = items
                .iter()
                .map(|d| {
                    ListItem::new(Line::from(vec![
                        Span::styled("󰐗 ", fg(ORANGE)),
                        Span::styled(d.name.clone(), fg(SNOW)),
                        Span::styled(format!("  {}", d.mac), fg(SLATE)),
                    ]))
                })
                .collect();
            render_panel(
                frame,
                panel_area,
                Span::styled(" 󰐷 New Devices ", bold(ORANGE)),
                None,
                rows,
                *selected,
                items.len(),
            );
        }
        _ => {
            let rows: Vec<ListItem> = app
                .devices
                .iter()
                .map(|d| {
                    let (mark, name_style) = if d.connected {
                        (Span::styled("󰂱 ", fg(GREEN)), bold(SNOW))
                    } else {
                        (Span::styled("󰂯 ", fg(SLATE)), fg(TEXT))
                    };
                    let mut spans = vec![mark, Span::styled(d.name.clone(), name_style)];
                    if d.connected {
                        spans.push(Span::styled(" [CONNECTED]", fg(GREEN)));
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
            if rows.is_empty() {
                let block = panel_block(Span::styled(" 󰂯 Paired Devices ", bold(BLUE)));
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
                    Span::styled(" 󰂯 Paired Devices ", bold(BLUE)),
                    badge,
                    rows,
                    app.selected,
                    app.devices.len(),
                );
            }
        }
    }

    frame.render_widget(status_line(app), status_area);

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
}

/// Banner-style prompt line: orange `❯`, message colored by its severity.
fn status_line(app: &App) -> Line<'static> {
    if let Some(busy) = &app.busy {
        let spin = SPINNER[app.tick as usize % SPINNER.len()];
        let text = app
            .status
            .as_ref()
            .map(|(msg, _, _)| msg.clone())
            .unwrap_or_else(|| busy.clone());
        return Line::from(vec![
            Span::styled(format!(" {spin} "), bold(ORANGE)),
            Span::styled(text, fg(SNOW)),
        ]);
    }
    if let Some((msg, tone, _)) = &app.status {
        let style = match tone {
            Tone::Err => bold(RED),
            Tone::Warn => fg(ORANGE),
            Tone::Ok => fg(GREEN),
            Tone::Info => fg(ICE),
        };
        return Line::from(vec![
            Span::styled(" ❯ ", bold(ORANGE)),
            Span::styled(msg.clone(), style),
        ]);
    }
    Line::default()
}

/// `─[Enter→Toggle]─[s→Pair new]─…` chips for the bottom border.
fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, action)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("─", fg(STEEL)));
        }
        spans.push(Span::styled("[", fg(SLATE)));
        spans.push(Span::styled((*key).to_string(), bold(ORANGE)));
        spans.push(Span::styled("→", fg(SLATE)));
        spans.push(Span::styled((*action).to_string(), fg(TEXT)));
        spans.push(Span::styled("]", fg(SLATE)));
    }
    Line::from(spans)
}

fn panel_block(title: Span<'static>) -> Block<'static> {
    Block::bordered().border_style(fg(STEEL_DIM)).title(title)
}

fn render_panel(
    frame: &mut Frame,
    area: Rect,
    title: Span<'static>,
    badge: Option<Line<'static>>,
    rows: Vec<ListItem>,
    selected: usize,
    len: usize,
) {
    let mut block = panel_block(title).title_bottom(
        Line::from(vec![
            Span::styled("┤", fg(STEEL_DIM)),
            Span::styled(format!("{}/{}", selected + 1, len), bold(ICE)),
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
