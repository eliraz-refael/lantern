//! Drawing: header, parent / current / preview columns, footer. Colors follow the design mockup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table, TableState};
use ratatui_image::{FilterType, Resize, StatefulImage};

use crate::App;
use crate::files::{self, Entry, Kind};
use crate::images::Preview;

const AMBER: Color = Color::Rgb(0xff, 0xb5, 0x47);
const AMBER_DIM: Color = Color::Rgb(0x8c, 0x67, 0x2e);
const FG: Color = Color::Rgb(0xd9, 0xd7, 0xd0);
const DIM: Color = Color::Rgb(0x6d, 0x6b, 0x66);
const LINE: Color = Color::Rgb(0x38, 0x38, 0x3e);
const DIR: Color = Color::Rgb(0x86, 0xb6, 0xff);
const DIR_UNLIT: Color = Color::Rgb(0x4f, 0x6c, 0x96);
const IMAGE: Color = Color::Rgb(0xd7, 0xa6, 0xff);
const SELECTED_BG: Color = Color::Rgb(0x33, 0x27, 0x0f);
const PARENT_BG: Color = Color::Rgb(0x26, 0x26, 0x2b);
const FOOTER_BG: Color = Color::Rgb(0x14, 0x14, 0x16);
const ON_AMBER: Color = Color::Rgb(0x1a, 0x12, 0x05);

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

fn sep() -> Span<'static> {
    Span::styled(" │ ", fg(LINE))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let widths = if body.width >= 110 {
        [
            Constraint::Length(26),
            Constraint::Length(46),
            Constraint::Fill(1),
        ]
    } else {
        [
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Fill(1),
        ]
    };
    let [parent, current, preview_area] = Layout::horizontal(widths).areas(body);

    header(frame, app, head);

    let mut parent_state =
        TableState::default().with_selected(app.parent.iter().position(|e| e.path == app.cwd));
    let parent_table = entry_table(app, &app.parent)
        .block(column_block())
        .row_highlight_style(Style::new().bg(PARENT_BG));
    frame.render_stateful_widget(parent_table, parent, &mut parent_state);

    let current_table = entry_table(app, &app.entries)
        .block(column_block())
        .row_highlight_style(Style::new().bg(SELECTED_BG));
    frame.render_stateful_widget(current_table, current, &mut app.table);

    preview(frame, app, preview_area);
    footer(frame, app, foot);
}

fn column_block() -> Block<'static> {
    Block::new()
        .borders(Borders::RIGHT)
        .border_style(fg(LINE))
        .padding(Padding::horizontal(1))
}

/// Renders `left` and `right` on one row, giving `right` the room it needs.
fn split_line(frame: &mut Frame, area: Rect, left: Line, right: Line) {
    let [l, r] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(right.width() as u16),
    ])
    .areas(area);
    frame.render_widget(left, l);
    frame.render_widget(right, r);
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new().borders(Borders::BOTTOM).border_style(fg(LINE));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut left = vec![
        Span::styled(" ✦ lantern  ", fg(AMBER).add_modifier(Modifier::BOLD)),
        Span::styled(app.display_path(&app.cwd), fg(FG)),
    ];
    match app.lit_root(&app.cwd) {
        Some(root) if root == app.cwd => left.push(Span::styled(" ✦", fg(AMBER))),
        Some(_) => left.push(Span::styled(" ✦", fg(AMBER_DIM))),
        None => {}
    }
    let right = Line::from(vec![
        Span::styled("images: ", fg(DIM)),
        Span::styled(app.images.protocol_name(), fg(FG)),
        sep(),
        Span::styled("no agent", fg(DIM)),
        sep(),
        Span::styled(app.lit.len().to_string(), fg(FG)),
        Span::styled(" lit", fg(DIM)),
        sep(),
        Span::styled(app.indexed.to_string(), fg(FG)),
        Span::styled(" indexed ", fg(DIM)),
    ]);
    split_line(frame, inner, Line::from(left), right);
}

fn entry_table(app: &App, entries: &[Entry]) -> Table<'static> {
    let rows = entries.iter().map(|e| entry_row(app, e));
    Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(6),
        ],
    )
    .column_spacing(1)
}

fn entry_row(app: &App, entry: &Entry) -> Row<'static> {
    let (mark, name, meta) = match entry.kind {
        Kind::Dir => {
            let lit = app.lit_root(&entry.path);
            let mark = match lit {
                Some(root) if root == entry.path => Span::styled("✦", fg(AMBER)),
                Some(_) => Span::styled("✦", fg(AMBER_DIM)),
                None => Span::styled("○", fg(DIM)),
            };
            // Unlit folders are dimmed so it's clear what search can see.
            let style = match lit {
                Some(_) => fg(DIR).add_modifier(Modifier::BOLD),
                None => fg(DIR_UNLIT),
            };
            let meta = entry.count.map(|c| c.to_string()).unwrap_or_default();
            (mark, Span::styled(format!("{}/", entry.name), style), meta)
        }
        Kind::Image => (
            Span::raw(" "),
            Span::styled(entry.name.clone(), fg(IMAGE)),
            files::human_size(entry.size),
        ),
        Kind::File => (
            Span::raw(" "),
            Span::styled(entry.name.clone(), fg(FG)),
            files::human_size(entry.size),
        ),
    };
    Row::new([
        Cell::from(mark),
        Cell::from(name),
        Cell::from(Line::styled(meta, fg(DIM)).right_aligned()),
    ])
}

fn preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let area = Block::new().padding(Padding::horizontal(2)).inner(area);
    let Some(entry) = app.selected() else {
        frame.render_widget(Line::styled("empty", fg(DIM)), area);
        return;
    };
    let rule = Line::styled("╌".repeat(area.width as usize), fg(LINE));

    if entry.kind == Kind::Dir {
        let count = entry.count.map_or("can't read this folder".into(), |c| {
            files::plural(c, "item")
        });
        let status = match app.lit_root(&entry.path) {
            Some(root) if root == entry.path => Line::from(vec![
                Span::styled("✦ lit", fg(AMBER)),
                Span::styled(" · not indexed yet, no agent connected", fg(DIM)),
            ]),
            Some(root) => Line::from(vec![
                Span::styled("✦ lit", fg(AMBER)),
                Span::styled(format!(" via {}", files::display_name(root)), fg(DIM)),
            ]),
            None => Line::from(vec![
                Span::styled("○ not lit · press ", fg(DIM)),
                Span::styled("L", fg(AMBER)),
                Span::styled(" to light up", fg(DIM)),
            ]),
        };
        let title = Line::from(vec![
            Span::styled(
                format!("{}/", entry.name),
                fg(DIR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {count}"), fg(DIM)),
        ]);
        let [top, rest] =
            Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
        frame.render_widget(Paragraph::new(vec![title, status, rule]), top);
        let children = files::list(&entry.path, app.show_hidden, false);
        frame.render_widget(entry_table(app, &children), rest);
        return;
    }

    let is_image = entry.kind == Kind::Image;
    let mut details = files::human_size(entry.size);
    if let Some(modified) = entry.modified {
        details += &format!(" · modified {}", files::age(modified));
    }
    let status = match app.lit_root(&entry.path) {
        Some(_) => Line::from(vec![
            Span::styled("✦ in a lit folder", fg(AMBER)),
            Span::styled(" · not indexed yet, no agent connected", fg(DIM)),
        ]),
        None => Line::styled("○ not indexed: this folder isn't lit", fg(DIM)),
    };
    let mut lines = vec![
        Line::styled(entry.name.clone(), fg(if is_image { IMAGE } else { FG })),
        Line::styled(details, fg(DIM)),
        status,
        rule,
    ];
    if is_image {
        // The image goes on top with its details right underneath, as in the mockup.
        lines.truncate(3);
        let path = entry.path.clone();
        let max_height = area.height.saturating_sub(lines.len() as u16 + 1);
        let used = match app.images.get(&path) {
            Some(Preview::Ready(protocol)) => {
                let resize = Resize::Fit(Some(FilterType::Triangle));
                let size = protocol.size_for(resize.clone(), Size::new(area.width, max_height));
                let rect = Rect {
                    width: size.width,
                    height: size.height,
                    ..area
                };
                frame.render_stateful_widget(
                    StatefulImage::default().resize(resize),
                    rect,
                    protocol,
                );
                size.height
            }
            Some(Preview::Failed(message)) => {
                frame.render_widget(Line::styled(message.clone(), fg(DIM)), area);
                1
            }
            _ => {
                frame.render_widget(Line::styled("loading…", fg(DIM)), area);
                1
            }
        };
        let below = Rect {
            y: area.y + used + 1,
            height: area.height.saturating_sub(used + 1),
            ..area
        };
        frame.render_widget(Paragraph::new(lines), below);
        return;
    }
    match files::text_head(&entry.path, area.height as usize) {
        Some(text) => lines.extend(text.into_iter().map(|l| Line::styled(l, fg(FG)))),
        None => lines.push(Line::styled("binary file", fg(DIM))),
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::new().style(Style::new().bg(FOOTER_BG)), area);

    let position = match app.table.selected() {
        Some(i) if !app.entries.is_empty() => format!(" {}/{}", i + 1, app.entries.len()),
        _ => " 0/0".into(),
    };
    let lit = match app.lit_root(&app.cwd) {
        Some(_) => Span::styled("✦ lit", fg(AMBER)),
        None => Span::styled("○ not lit", fg(DIM)),
    };
    let mut left = vec![
        Span::styled(
            " NORMAL ",
            fg(ON_AMBER).bg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(position, fg(FG)),
        sep(),
        lit,
    ];
    if let Some(status) = &app.status {
        left.push(sep());
        left.push(Span::styled(status.clone(), fg(AMBER)));
    }

    let keys = [
        ("h/l", "out/in"),
        ("j/k", "move"),
        ("L", "light up"),
        (".", "hidden"),
        ("q", "quit"),
    ];
    let right = Line::from(
        keys.iter()
            .flat_map(|(key, action)| {
                [
                    Span::styled(*key, fg(AMBER)),
                    Span::styled(format!(" {action}  "), fg(DIM)),
                ]
            })
            .collect::<Vec<_>>(),
    );
    // A status message gets the whole row if it would otherwise run into the key hints.
    let left = Line::from(left);
    if app.status.is_some() && left.width() + right.width() > area.width as usize {
        frame.render_widget(left, area);
    } else {
        split_line(frame, area, left, right);
    }
}
