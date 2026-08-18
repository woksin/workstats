//! Drawing. Every level of the drill-down, the diff pane, and the overlays.
//!
//! `draw` is the only entry point and is a pure function of `App` plus the
//! frame size. Nothing here decides anything — the two `&mut` calls it makes
//! (`set_viewport`, `set_offset`) hand the renderer's own geometry back so a
//! page key moves by what the reader can actually see.
//!
//! Every panel degrades instead of failing on a small terminal: columns are
//! dropped from the right, the chrome gives up its rows before the body does,
//! and an overlay is clipped to whatever room is left. A one-cell window must
//! still not panic.
//!
//! Text drawn here is shortened and stripped of control characters first. Paths
//! come from Git and diff lines come from a file, so neither is text this tool
//! chose; an escape sequence in one must not be able to move the cursor around
//! the screen. `src/output.rs` neutralises its diagnostic messages for the same
//! reason.

use std::mem;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Padding,
    Paragraph, Row, Table, TableState, Wrap,
};

use super::app::App;
use super::diff::{DiffKind, DiffView};
use super::state::{Column, Entry, KEYBINDINGS, LevelKind, Mode, SavedView, Sort};

/// The selection marker and the width `Table` reserves for it. Reserving it
/// always keeps the columns from jumping sideways when a selection appears.
const MARKER: &str = "› ";
const MARKER_WIDTH: u16 = 2;
/// `Table`'s own default, repeated here so `column_widths` resolves the same
/// layout the widget will.
const COLUMN_SPACING: u16 = 1;
/// A fill column has no natural width. Below this it stops carrying a readable
/// path, so the column is dropped rather than shown as a stub.
const FILL_FLOOR: u16 = 12;
/// A header and two rows. The body is served before any chrome is.
const BODY_FLOOR: u16 = 3;

const ELLIPSIS: &str = "…";
const TRAIL: &str = " › ";
const DOT: &str = " · ";
/// Drawn into the text rather than moved with the terminal's own cursor: the
/// cursor would have to be placed by whichever panel owns the input, and an
/// overlay can be drawn over that panel later in the same frame.
const CARET: &str = "▏";

/// Foreground only, on purpose. A background painted across the page fights
/// whatever theme the reader chose, and light terminals are as common as dark
/// ones.
const PLAIN: Style = Style::new();
const DIM: Style = Style::new().fg(Color::DarkGray);
const HEAD: Style = Style::new().add_modifier(Modifier::BOLD);
const ACCENT: Style = Style::new().fg(Color::Cyan);
const WARN: Style = Style::new().fg(Color::Yellow);
const MATCH: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const SORTED: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let message = message_line(app);
    let panels = Panels::split(area, message.is_some());

    draw_breadcrumb(frame, panels.breadcrumb, app);
    draw_summary(frame, panels.summary, app);
    if app.level() == LevelKind::Diff {
        draw_diff(frame, panels.body, app);
    } else {
        draw_table(frame, panels.body, app);
    }
    if let Some(message) = message {
        frame.render_widget(Paragraph::new(message), panels.message);
    }
    draw_status(frame, panels.status, app);
    draw_overlays(frame, area, app);
}

// ---- layout ---------------------------------------------------------------

/// The horizontal bands of the screen. A band with zero height is simply never
/// drawn into, which is how the layout degrades on a short terminal.
struct Panels {
    breadcrumb: Rect,
    summary: Rect,
    body: Rect,
    message: Rect,
    status: Rect,
}

impl Panels {
    fn split(area: Rect, message: bool) -> Self {
        let heights = band_heights(area.height, message);
        let [breadcrumb, summary, body, message, status] =
            Layout::vertical(heights.map(Constraint::Length)).areas(area);
        Self {
            breadcrumb,
            summary,
            body,
            message,
            status,
        }
    }
}

/// How many rows each band gets. The body is served first — a reader can lose
/// the summary strip and keep working, but a screen with no rows on it is
/// useless — and the bands are given up in the order they are least missed.
/// The summary is context, the breadcrumb is repeated by the status bar, and
/// the message line carries errors, so it outlives both.
fn band_heights(height: u16, message: bool) -> [u16; 5] {
    let mut bands = [1, 1, 0, u16::from(message), 1];
    for band in [1, 0, 3, 4] {
        let chrome: u16 = bands.iter().sum();
        if height >= chrome + BODY_FLOOR {
            break;
        }
        bands[band] = 0;
    }
    let chrome: u16 = bands.iter().sum();
    bands[2] = height.saturating_sub(chrome);
    bands
}

/// Centres a panel of at most `width` × `height` inside `area`. A panel that
/// cannot fit is shrunk rather than pushed off the edge.
fn overlay(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

// ---- breadcrumb and summary strip ------------------------------------------

fn draw_breadcrumb(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let spans = breadcrumb_spans(&app.breadcrumb(), area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Keeps the end of the trail rather than its start: the level a reader is on
/// matters more than the way they got there.
fn breadcrumb_spans(trail: &[String], width: usize) -> Vec<Span<'static>> {
    let Some((last, head)) = trail.split_last() else {
        return Vec::new();
    };
    let tail = clip(last, width);
    let mut used = tail.chars().count();
    let mut kept = 0;
    for label in head.iter().rev() {
        let needed = label.chars().count() + TRAIL.chars().count();
        if used + needed > width {
            break;
        }
        used += needed;
        kept += 1;
    }
    // The "…" that stands in for the dropped head needs room of its own, so
    // give back segments until it has some.
    if kept < head.len() {
        let prefix = ELLIPSIS.chars().count() + TRAIL.chars().count();
        while kept > 0 && used + prefix > width {
            let label = &head[head.len() - kept];
            used -= label.chars().count() + TRAIL.chars().count();
            kept -= 1;
        }
    }
    let mut spans = Vec::with_capacity(kept * 2 + 3);
    if kept < head.len() {
        spans.push(Span::styled(ELLIPSIS, DIM));
        spans.push(Span::styled(TRAIL, DIM));
    }
    for label in &head[head.len() - kept..] {
        spans.push(Span::styled(label.clone(), DIM));
        spans.push(Span::styled(TRAIL, DIM));
    }
    spans.push(Span::styled(tail, HEAD));
    spans
}

fn draw_summary(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let segments: Vec<Vec<Span<'static>>> = app
        .summary()
        .iter()
        .map(|(label, value)| {
            vec![
                Span::styled(format!("{label} "), DIM),
                Span::styled(value.clone(), PLAIN),
            ]
        })
        .collect();
    frame.render_widget(
        Paragraph::new(pack(segments, DOT, area.width as usize)),
        area,
    );
}

/// Joins as many segments as the width allows, dropping from the right. The
/// leftmost segments answer "where am I", so they are the last to go; the first
/// one is kept even when it overflows, because a blank strip reads as a bug.
fn pack(segments: Vec<Vec<Span<'static>>>, separator: &'static str, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for segment in segments {
        let length: usize = segment
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        let gap = if spans.is_empty() {
            0
        } else {
            separator.chars().count()
        };
        if !spans.is_empty() && used + gap + length > width {
            break;
        }
        if gap != 0 {
            spans.push(Span::styled(separator, DIM));
        }
        used += gap + length;
        spans.extend(segment);
    }
    Line::from(spans)
}

// ---- the table levels -------------------------------------------------------

fn draw_table(frame: &mut Frame, area: Rect, app: &mut App) {
    // The header takes a row, so the page keys must not think they have it.
    app.set_viewport(area.height.saturating_sub(1).max(1) as usize);
    if area.is_empty() {
        return;
    }
    if app.rows().is_empty() {
        frame.render_widget(empty_state(app), area);
        app.set_offset(0);
        return;
    }
    let widget = table(app.columns(), app.rows(), app.sort(), area.width);
    let selected = app.selected().min(app.rows().len() - 1);
    let mut state = TableState::new()
        .with_offset(app.offset())
        .with_selected(Some(selected));
    frame.render_stateful_widget(widget, area, &mut state);
    // `Table` scrolls the offset to keep the selection visible; handing it back
    // is what makes the position survive a trip into a child level and out.
    app.set_offset(state.offset());
}

/// Built apart from `draw_table` so the widget can be exercised at every
/// terminal width without standing up an `App`.
fn table(columns: &[Column], rows: &[Entry], sort: Sort, width: u16) -> Table<'static> {
    let columns = &columns[..visible_columns(columns, width)];
    let widths = column_widths(width, columns);
    let titles: Vec<Cell<'static>> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let sorted = (index == sort.column).then_some(sort.descending);
            let room = widths.get(index).copied().unwrap_or(0) as usize;
            Cell::from(aligned(
                header_text(column, index, sorted, room),
                column.numeric,
            ))
            .style(if sorted.is_some() { SORTED } else { HEAD })
        })
        .collect();
    let header = Row::new(titles);
    let body: Vec<Row<'static>> = rows
        .iter()
        .map(|entry| body_row(entry, columns, &widths))
        .collect();
    Table::new(body, columns.iter().map(constraint).collect::<Vec<_>>())
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .row_highlight_style(SELECTED)
        .highlight_symbol(MARKER)
        .highlight_spacing(HighlightSpacing::Always)
}

fn body_row(entry: &Entry, columns: &[Column], widths: &[u16]) -> Row<'static> {
    let cells: Vec<Cell<'static>> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let width = widths.get(index).copied().unwrap_or(0) as usize;
            let text = entry
                .fields
                .get(index)
                .map_or_else(String::new, |field| clip(&field.text, width));
            Cell::from(aligned(text, column.numeric))
        })
        .collect();
    Row::new(cells)
}

fn aligned(text: String, numeric: bool) -> Line<'static> {
    let line = Line::from(text);
    if numeric { line.right_aligned() } else { line }
}

/// The column header: its number, because `1`–`9` are what select it, and an
/// arrow on the one the rows are sorted by. The number is the first thing
/// dropped when the column is too narrow for all three.
fn header_text(column: &Column, index: usize, sorted: Option<bool>, width: usize) -> String {
    let arrow = match sorted {
        Some(true) => " ▼",
        Some(false) => " ▲",
        None => "",
    };
    let number = if index < 9 {
        format!("{} ", index + 1)
    } else {
        String::new()
    };
    let title = column.title;
    if number.chars().count() + title.chars().count() + arrow.chars().count() <= width {
        format!("{number}{title}{arrow}")
    } else if title.chars().count() + arrow.chars().count() <= width {
        format!("{title}{arrow}")
    } else {
        shorten(title, width)
    }
}

/// How many columns fit. Trailing columns are dropped rather than squeezed —
/// half a number is worse than no number — and the status bar still names the
/// column the rows are sorted by when it has gone off screen.
fn visible_columns(columns: &[Column], width: u16) -> usize {
    if columns.is_empty() {
        return 0;
    }
    let mut used = MARKER_WIDTH;
    let mut visible = 0;
    for column in columns {
        let needed = if column.width == 0 {
            FILL_FLOOR
        } else {
            column.width
        };
        let next = used.saturating_add(needed).saturating_add(if visible == 0 {
            0
        } else {
            COLUMN_SPACING
        });
        if visible > 0 && next > width {
            break;
        }
        used = next;
        visible += 1;
    }
    // The first column carries the row's identity, so it is shown even on a
    // terminal too narrow for it and left to the widget to clip.
    visible.max(1)
}

/// Resolves the widths `Table` will use internally, so a cell can be shortened
/// to the room it will actually get instead of being cut mid-word by the
/// widget. It mirrors `Table::get_column_widths`: the selection column first,
/// then the constraints with the same spacing.
fn column_widths(width: u16, columns: &[Column]) -> Vec<u16> {
    if columns.is_empty() {
        return Vec::new();
    }
    let [_marker, rest] =
        Layout::horizontal([Constraint::Length(MARKER_WIDTH), Constraint::Fill(0)])
            .areas(Rect::new(0, 0, width, 1));
    Layout::horizontal(columns.iter().map(constraint).collect::<Vec<_>>())
        .spacing(COLUMN_SPACING)
        .split(rest)
        .iter()
        .map(|rect| rect.width)
        .collect()
}

const fn constraint(column: &Column) -> Constraint {
    if column.width == 0 {
        Constraint::Fill(1)
    } else {
        Constraint::Length(column.width)
    }
}

/// A level with nothing in it says why. A blank panel reads as a bug, and the
/// reason is most often a filter the reader has forgotten they typed.
fn empty_state(app: &App) -> Paragraph<'static> {
    let (headline, hint) = if app.filter().is_empty() {
        match app.level() {
            LevelKind::Overview => (
                "This report has no commits, so there is nothing to explore.".to_string(),
                "Widen the date range, check --author, or scan another source root.".to_string(),
            ),
            LevelKind::Repo => (
                "No commits are recorded for this repository.".to_string(),
                "Switch the period between month and day with p.".to_string(),
            ),
            LevelKind::Period => (
                "Nothing was categorised in this period.".to_string(),
                "Every changed path in it matched an ignore rule.".to_string(),
            ),
            LevelKind::Category => (
                "No commit in this period touched this category.".to_string(),
                "Esc goes back to the categories.".to_string(),
            ),
            LevelKind::Commit => (
                "This commit changed no counted files.".to_string(),
                "Its paths were all covered by an ignore rule.".to_string(),
            ),
            LevelKind::File => (
                "No commit in this repository touched this path.".to_string(),
                "Git reports a rename under its new path, so older work can be filed \
                 under the old name."
                    .to_string(),
            ),
            LevelKind::Diff => (
                "There is no diff to show.".to_string(),
                "Esc goes back to the file list.".to_string(),
            ),
        }
    } else {
        (
            format!("No {} match “{}”.", app.level().label(), app.filter()),
            "Esc clears the filter.".to_string(),
        )
    };
    Paragraph::new(vec![
        Line::from(""),
        Line::styled(headline, WARN),
        Line::styled(hint, DIM),
    ])
    .centered()
    .wrap(Wrap { trim: true })
}

// ---- the diff pane ----------------------------------------------------------

fn draw_diff(frame: &mut Frame, area: Rect, app: &mut App) {
    let body = area.height.saturating_sub(1) as usize;
    app.set_viewport(body.max(1));
    if area.is_empty() {
        return;
    }
    let width = area.width as usize;
    let Some(view) = app.diff() else {
        frame.render_widget(empty_state(app), area);
        return;
    };
    let offset = app.diff_offset().min(view.lines.len().saturating_sub(1));
    let mut lines = Vec::with_capacity(body + 1);
    lines.push(diff_title(view, offset, width));
    lines.extend(
        view.lines
            .iter()
            .skip(offset)
            .take(body)
            .map(|line| Line::styled(shorten(&line.text, width), diff_style(line.kind))),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// What is being shown, how far down it the reader is, and whether the tool
/// stopped reading early. The last one matters: a truncated diff that does not
/// say so looks like a complete one.
fn diff_title(view: &DiffView, offset: usize, width: usize) -> Line<'static> {
    let position = if view.lines.is_empty() {
        "empty".to_string()
    } else {
        format!(
            "{} / {}",
            number(offset as u64 + 1),
            number(view.lines.len() as u64)
        )
    };
    let truncated = if view.truncated { "  truncated" } else { "" };
    let reserved = 2 + position.chars().count() + truncated.chars().count();
    let mut spans = vec![
        Span::styled(clip(&view.title, width.saturating_sub(reserved)), HEAD),
        Span::styled(format!("  {position}"), DIM),
    ];
    if view.truncated {
        spans.push(Span::styled(truncated, WARN));
    }
    Line::from(spans)
}

const fn diff_style(kind: DiffKind) -> Style {
    match kind {
        DiffKind::Meta => Style::new().fg(Color::Magenta),
        DiffKind::Hunk => Style::new().fg(Color::Cyan),
        DiffKind::Added => Style::new().fg(Color::Green),
        DiffKind::Removed => Style::new().fg(Color::Red),
        DiffKind::Context => PLAIN,
    }
}

// ---- the message line and the status bar ------------------------------------

/// The one line between the body and the status bar. What lands on it is
/// whatever the reader most needs right now: what they are typing, then what
/// the app has to tell them, then the filter that is hiding rows from them.
fn message_line(app: &App) -> Option<Line<'static>> {
    if app.mode() == Mode::Filter {
        return Some(Line::from(vec![
            Span::styled("filter ", ACCENT),
            Span::styled(app.filter().to_string(), PLAIN),
            Span::styled(CARET, ACCENT),
            Span::styled("   Enter keeps it · Esc clears it", DIM),
        ]));
    }
    if let Some(status) = app.status() {
        return Some(Line::styled(shorten(status, MAX_STATUS), WARN));
    }
    if !app.filter().is_empty() {
        return Some(Line::from(vec![
            Span::styled("filter ", DIM),
            Span::styled(app.filter().to_string(), PLAIN),
            Span::styled("   Esc clears it", DIM),
        ]));
    }
    None
}

/// A status message quotes a path or a Git error this tool did not choose, so
/// it is bounded the same way `src/output.rs` bounds a diagnostic message.
const MAX_STATUS: usize = 200;

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(pack(status_segments(app), DOT, area.width as usize)),
        area,
    );
}

fn status_segments(app: &App) -> Vec<Vec<Span<'static>>> {
    let mut segments = Vec::with_capacity(5);
    if app.mode() != Mode::Normal {
        segments.push(vec![Span::styled(mode_name(app.mode()), ACCENT)]);
    }
    segments.push(count_segment(app));
    segments.push(sort_segment(app));
    segments.push(vec![
        Span::styled("period ", DIM),
        Span::styled(app.grain().label(), PLAIN),
    ]);
    segments.push(vec![Span::styled("? keys · q quit", DIM)]);
    segments
}

fn count_segment(app: &App) -> Vec<Span<'static>> {
    if app.level() == LevelKind::Diff {
        let lines = app.diff().map_or(0, |view| view.lines.len());
        return vec![
            Span::styled(number(lines as u64), PLAIN),
            Span::styled(" diff lines", DIM),
        ];
    }
    let mut spans = vec![
        Span::styled(number(app.rows().len() as u64), PLAIN),
        Span::styled(format!(" {}", app.level().label()), DIM),
    ];
    // Without this a filtered level looks like the whole truth.
    if !app.filter().is_empty() {
        spans.push(Span::styled(" (filtered)", WARN));
    }
    spans
}

fn sort_segment(app: &App) -> Vec<Span<'static>> {
    let sort = app.sort();
    let Some(column) = app.columns().get(sort.column) else {
        return vec![Span::styled("no sort", DIM)];
    };
    vec![
        Span::styled("sort ", DIM),
        Span::styled(column.title, PLAIN),
        Span::styled(if sort.descending { " ▼" } else { " ▲" }, ACCENT),
    ]
}

const fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "browse",
        Mode::Filter => "filter",
        Mode::Search => "search",
        Mode::SaveView => "save view",
        Mode::Views => "saved views",
    }
}

// ---- overlays ---------------------------------------------------------------

fn draw_overlays(frame: &mut Frame, area: Rect, app: &App) {
    match app.mode() {
        Mode::Views => draw_views(frame, area, app),
        Mode::Search => draw_search(frame, area, app),
        Mode::SaveView => draw_save_view(frame, area, app),
        Mode::Normal | Mode::Filter => {}
    }
    // Help sits on top of everything else: it is what a lost reader reaches for.
    if app.help_visible() {
        draw_help(frame, area);
    }
}

/// A framed panel with the screen behind it cleared, which is what makes an
/// overlay legible over a full table.
fn panel(frame: &mut Frame, area: Rect, title: &str, footer: &str) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(DIM)
        .title(Span::styled(format!(" {title} "), HEAD))
        .title_bottom(Span::styled(format!(" {footer} "), DIM))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let keys = KEYBINDINGS
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let widest = KEYBINDINGS
        .iter()
        .map(|(key, description)| key.chars().count() + description.chars().count())
        .max()
        .unwrap_or(0);
    let outer = overlay(area, cells(widest + 6), cells(KEYBINDINGS.len() + 2));
    let inner = panel(frame, outer, "Keys", "? or Esc closes");
    if inner.is_empty() {
        return;
    }
    let rows = inner.height as usize;
    let mut lines: Vec<Line<'static>> = KEYBINDINGS
        .iter()
        .take(rows)
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(format!("{key:<keys$}"), ACCENT),
                Span::styled("  ", DIM),
                Span::styled(*description, PLAIN),
            ])
        })
        .collect();
    // Spend the last visible row on saying that there is more rather than
    // letting the list end without a word.
    if KEYBINDINGS.len() > rows
        && let Some(last) = lines.last_mut()
    {
        *last = Line::styled(format!("… {} more", KEYBINDINGS.len() - rows + 1), DIM);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_views(frame: &mut Frame, area: Rect, app: &App) {
    let views = app.saved_views();
    let widest = views
        .iter()
        .map(|view| view.name.chars().count() + describe_view(view).chars().count() + 2)
        .max()
        .unwrap_or(0);
    let outer = overlay(
        area,
        cells(widest + 6).max(44),
        cells(views.len().max(1) + 2),
    );
    let inner = panel(
        frame,
        outer,
        "Saved views",
        "Enter opens · d deletes · Esc closes",
    );
    if inner.is_empty() {
        return;
    }
    if views.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No saved views yet — press w to save one.",
                DIM,
            ))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    let width = inner.width as usize;
    let items: Vec<ListItem<'static>> = views
        .iter()
        .map(|view| {
            let name = clip(&view.name, width);
            let room = width.saturating_sub(name.chars().count() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(name, HEAD),
                Span::styled("  ", DIM),
                Span::styled(clip(&describe_view(view), room), DIM),
            ]))
        })
        .collect();
    let mut state =
        ListState::default().with_selected(Some(app.views_selected().min(views.len() - 1)));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(SELECTED)
            .highlight_symbol(MARKER),
        inner,
        &mut state,
    );
}

/// What a bookmark points at, written the way the breadcrumb writes it.
fn describe_view(view: &SavedView) -> String {
    let path = if view.path.is_empty() {
        "overview".to_string()
    } else {
        view.path.join(TRAIL)
    };
    let filter = if view.filter.is_empty() {
        String::new()
    } else {
        format!(" · filter {}", view.filter)
    };
    format!("{path} · {}{filter}", view.grain.label())
}

/// The search overlay is the widest one: its rows are file paths, and a path
/// cut in half identifies nothing.
const MAX_SEARCH_ROWS: usize = 12;

fn draw_search(frame: &mut Frame, area: Rect, app: &App) {
    let hits = app.search_hits();
    let outer = overlay(
        area,
        area.width.saturating_sub(4),
        cells(hits.len().clamp(1, MAX_SEARCH_ROWS) + 4),
    );
    let inner = panel(
        frame,
        outer,
        "Search",
        "Enter jumps · ↑ ↓ moves · Esc closes",
    );
    if inner.is_empty() {
        return;
    }
    let [prompt, list] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let mut typed = vec![
        Span::styled("search ", ACCENT),
        Span::styled(app.input().to_string(), PLAIN),
        Span::styled(CARET, ACCENT),
    ];
    if !app.input().is_empty() {
        typed.push(Span::styled(
            format!("   {} matches", number(hits.len() as u64)),
            DIM,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(typed)), prompt);
    if list.is_empty() {
        return;
    }
    if hits.is_empty() {
        let message = if app.input().is_empty() {
            "Type to search repositories, files and commits.".to_string()
        } else {
            format!("Nothing matches “{}”.", shorten(app.input(), MAX_STATUS))
        };
        frame.render_widget(
            Paragraph::new(Line::styled(message, DIM)).wrap(Wrap { trim: true }),
            list,
        );
        return;
    }
    // The kind tag, two spaces, and the selection marker come off the label's
    // budget before it is shortened.
    let room = (list.width as usize).saturating_sub(KIND_WIDTH + 2 + MARKER_WIDTH as usize);
    let items: Vec<ListItem<'static>> = hits
        .iter()
        .map(|hit| {
            let mut spans = vec![Span::styled(
                format!("{:<width$}  ", hit.kind, width = KIND_WIDTH),
                DIM,
            )];
            spans.extend(highlight(&hit.label, &hit.indices, room));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state =
        ListState::default().with_selected(Some(app.search_selected().min(hits.len() - 1)));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(SELECTED)
            .highlight_symbol(MARKER),
        list,
        &mut state,
    );
}

/// `"commit"` is the longest of the three kinds a hit can have.
const KIND_WIDTH: usize = 6;

/// Marks the characters the needle actually matched, so a reader can see why a
/// row is in the results at all.
fn highlight(label: &str, indices: &[usize], width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let characters = safe_chars(label);
    let mut marked = vec![false; characters.len()];
    for index in indices {
        if let Some(slot) = marked.get_mut(*index) {
            *slot = true;
        }
    }
    // A hit is usually a path, so an over-long label gives up its head. The
    // "…" that replaces it costs a cell of its own.
    let dropped = characters.len().saturating_sub(width);
    let start = if dropped == 0 { 0 } else { dropped + 1 };
    let mut spans = Vec::new();
    if dropped != 0 {
        spans.push(Span::styled(ELLIPSIS, DIM));
    }
    let mut run = String::new();
    let mut lit = false;
    for (position, character) in characters.iter().enumerate().skip(start) {
        if marked[position] != lit && !run.is_empty() {
            spans.push(Span::styled(mem::take(&mut run), style_of(lit)));
        }
        lit = marked[position];
        run.push(*character);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style_of(lit)));
    }
    spans
}

const fn style_of(matched: bool) -> Style {
    if matched { MATCH } else { PLAIN }
}

fn draw_save_view(frame: &mut Frame, area: Rect, app: &App) {
    let outer = overlay(area, 56, 5);
    let inner = panel(frame, outer, "Save view", "Enter saves · Esc cancels");
    if inner.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("name ", ACCENT),
                Span::styled(app.input().to_string(), PLAIN),
                Span::styled(CARET, ACCENT),
            ]),
            Line::from(""),
            Line::styled(
                "Saved beside the config file. A view records where you are, never a diff.",
                DIM,
            ),
        ])
        .wrap(Wrap { trim: true }),
        inner,
    );
}

// ---- text ------------------------------------------------------------------

/// Control characters are replaced before anything is drawn: a Git path or a
/// diff line is text this tool did not choose, and an escape sequence in one
/// would otherwise be handed straight to the terminal.
fn safe_chars(text: &str) -> Vec<char> {
    text.chars()
        .map(|character| {
            if character.is_control() {
                '·'
            } else {
                character
            }
        })
        .collect()
}

/// Fits `text` into `width` cells, keeping its start.
fn shorten(text: &str, width: usize) -> String {
    let characters = safe_chars(text);
    if characters.len() <= width {
        return characters.into_iter().collect();
    }
    match width {
        0 => String::new(),
        1 => ELLIPSIS.to_string(),
        _ => {
            let mut head: String = characters[..width - 1].iter().collect();
            head.push_str(ELLIPSIS);
            head
        }
    }
}

/// Fits `text` into `width` cells, keeping its end: a path's file name
/// identifies it and its directory prefix usually does not.
fn shorten_path(text: &str, width: usize) -> String {
    let characters = safe_chars(text);
    if characters.len() <= width {
        return characters.into_iter().collect();
    }
    match width {
        0 => String::new(),
        1 => ELLIPSIS.to_string(),
        _ => {
            let mut tail = ELLIPSIS.to_string();
            tail.extend(&characters[characters.len() - (width - 1)..]);
            tail
        }
    }
}

/// Keeps the end of anything that looks like a path and the start of anything
/// else, the way `src/output.rs` shortens an over-long row label.
fn clip(text: &str, width: usize) -> String {
    if text.contains('/') || text.contains('\\') {
        shorten_path(text, width)
    } else {
        shorten(text, width)
    }
}

/// Thousands separated the way `src/output.rs` separates them, so a count read
/// in the explorer and the same count read in the printed table look alike.
fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A `u16` is the unit of a terminal; anything that does not fit in one is
/// already wider than any screen.
fn cells(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::state::{Field, columns};

    fn entries(width: usize, count: usize) -> Vec<Entry> {
        (0..count)
            .map(|index| Entry {
                id: format!("row-{index}"),
                fields: (0..width)
                    .map(|column| Field {
                        text: format!("some/long/path/{index}/value-{column}.rs"),
                        value: Some(index as f64),
                    })
                    .collect(),
            })
            .collect()
    }

    fn text(line: &Line<'_>) -> String {
        let mut joined = String::new();
        for span in line.iter() {
            joined.push_str(&span.content);
        }
        joined
    }

    #[test]
    fn a_table_renders_at_every_width_and_height_without_panicking() {
        // The table is the one widget every level goes through, so a size that
        // panics here is a size that makes `workstats ui` unusable.
        let schema = columns(LevelKind::Overview);
        let rows = entries(schema.len(), 30);
        for width in 1..90_u16 {
            for height in [1_u16, 2, 3, 8, 40] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        let widget = table(
                            schema,
                            &rows,
                            Sort {
                                column: 4,
                                descending: true,
                            },
                            area.width,
                        );
                        let mut state = TableState::new().with_offset(0).with_selected(Some(2));
                        frame.render_stateful_widget(widget, area, &mut state);
                    })
                    .unwrap();
            }
        }
    }

    #[test]
    fn every_level_has_a_table_that_fits_its_own_schema() {
        for kind in [
            LevelKind::Overview,
            LevelKind::Repo,
            LevelKind::Period,
            LevelKind::Category,
            LevelKind::Commit,
            LevelKind::File,
        ] {
            let schema = columns(kind);
            let rows = entries(schema.len(), 3);
            let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let widget = table(schema, &rows, Sort::default(), area.width);
                    let mut state = TableState::new().with_selected(Some(0));
                    frame.render_stateful_widget(widget, area, &mut state);
                })
                .unwrap();
        }
    }

    #[test]
    fn the_help_overlay_is_clipped_rather_than_overflowing() {
        for (width, height) in [(1_u16, 1_u16), (6, 3), (20, 6), (40, 12), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw_help(frame, area);
                })
                .unwrap();
        }
    }

    #[test]
    fn the_body_keeps_its_rows_and_the_chrome_gives_up_first() {
        // Tall enough for everything.
        assert_eq!([1, 1, 20, 1, 1], band_heights(24, true));
        // The summary goes, then the breadcrumb, and the body never shrinks
        // below its floor while any chrome is left to drop.
        assert_eq!([1, 0, 3, 1, 1], band_heights(6, true));
        assert_eq!([0, 0, 3, 1, 1], band_heights(5, true));
        assert_eq!([0, 0, 3, 0, 1], band_heights(4, false));
        for height in 0..40_u16 {
            for message in [false, true] {
                let bands = band_heights(height, message);
                assert_eq!(
                    height,
                    bands.iter().sum::<u16>(),
                    "{height} rows, message {message}"
                );
            }
        }
    }

    #[test]
    fn columns_are_dropped_from_the_right_and_the_first_one_always_stays() {
        let schema = columns(LevelKind::Overview);
        assert_eq!(schema.len(), visible_columns(schema, 200));
        assert_eq!(1, visible_columns(schema, 1));
        assert_eq!(0, visible_columns(&[], 80));
        // Narrowing never adds a column back.
        let mut previous = schema.len();
        for width in (1..200_u16).rev() {
            let visible = visible_columns(schema, width);
            assert!(visible <= previous, "width {width}");
            previous = visible;
        }
    }

    #[test]
    fn resolved_column_widths_never_exceed_the_area() {
        let schema = columns(LevelKind::Overview);
        for width in 1..200_u16 {
            let visible = visible_columns(schema, width);
            let widths = column_widths(width, &schema[..visible]);
            assert_eq!(visible, widths.len(), "width {width}");
            // Everything the columns claim has to fit in what is left after the
            // selection marker, or a cell would be shortened to a width the
            // widget never gives it.
            let claimed: u32 = widths.iter().map(|value| u32::from(*value)).sum::<u32>()
                + u32::from(COLUMN_SPACING) * visible.saturating_sub(1) as u32;
            let room = u32::from(width.saturating_sub(MARKER_WIDTH));
            assert!(claimed <= room, "width {width}: {widths:?}");
        }
    }

    #[test]
    fn a_header_gives_up_its_number_before_its_arrow() {
        let column = Column {
            title: "Commits",
            width: 9,
            numeric: true,
        };
        assert_eq!("3 Commits", header_text(&column, 2, None, 9));
        assert_eq!("Commits ▼", header_text(&column, 2, Some(true), 9));
        assert_eq!("3 Commits ▲", header_text(&column, 2, Some(false), 11));
        assert_eq!("Commi…", header_text(&column, 2, Some(true), 6));
        // Past nine there is no key to press, so there is no number to show.
        assert_eq!("Commits", header_text(&column, 9, None, 20));
    }

    #[test]
    fn text_is_shortened_from_the_end_a_path_is_shortened_from_the_front() {
        assert_eq!("short", shorten("short", 10));
        assert_eq!("abcd…", shorten("abcdefgh", 5));
        assert_eq!("…d/e.rs", shorten_path("a/b/c/d/e.rs", 7));
        assert_eq!("…", shorten("abcdefgh", 1));
        assert_eq!("", shorten("abcdefgh", 0));
        // The slash decides which end survives.
        assert_eq!("…d/e.rs", clip("a/b/c/d/e.rs", 7));
        assert_eq!("a rat…", clip("a rather long phrase", 6));
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        // A Git path is not text this tool chose, and `core.quotePath=false`
        // means the raw bytes arrive.
        assert_eq!("a·b", shorten("a\nb", 10));
        // A clear-screen sequence is five glyphs, not an instruction.
        assert_eq!("·[2J·", shorten("\u{1b}[2J\r", 10));
        assert_eq!("…d/e·f", shorten_path("a/b/c/d/e\tf", 6));
        assert_eq!(
            vec!['s', '·', 'e'],
            safe_chars("s\u{7}e"),
            "a bell is not a glyph"
        );
    }

    #[test]
    fn a_breadcrumb_keeps_the_level_the_reader_is_on() {
        let trail = vec![
            "workstats".to_string(),
            "widget".to_string(),
            "2026-06".to_string(),
            "source".to_string(),
        ];
        let full = text(&Line::from(breadcrumb_spans(&trail, 80)));
        assert_eq!("workstats › widget › 2026-06 › source", full);

        let narrow = text(&Line::from(breadcrumb_spans(&trail, 22)));
        assert!(narrow.ends_with("source"), "{narrow}");
        assert!(narrow.starts_with('…'), "{narrow}");
        assert!(narrow.chars().count() <= 22, "{narrow}");

        assert!(breadcrumb_spans(&[], 40).is_empty());
        // A single segment wider than the screen is shortened, not dropped.
        let alone = text(&Line::from(breadcrumb_spans(
            &["a-very-long-repository-name".to_string()],
            10,
        )));
        assert_eq!(10, alone.chars().count(), "{alone}");
    }

    #[test]
    fn a_status_bar_drops_its_rightmost_segments_first() {
        let segments = vec![
            vec![Span::styled("first", PLAIN)],
            vec![Span::styled("second", PLAIN)],
            vec![Span::styled("third", PLAIN)],
        ];
        assert_eq!(
            "first · second · third",
            text(&pack(segments.clone(), DOT, 80))
        );
        assert_eq!("first · second", text(&pack(segments.clone(), DOT, 15)));
        // The leftmost segment is kept even when it does not fit: a blank bar
        // reads as a bug.
        assert_eq!("first", text(&pack(segments, DOT, 2)));
        assert_eq!("", text(&pack(Vec::new(), DOT, 40)));
    }

    #[test]
    fn a_search_hit_marks_the_characters_that_matched() {
        let spans = highlight("src/lib.rs", &[0, 1, 2], 40);
        assert_eq!("src/lib.rs", text(&Line::from(spans.clone())));
        assert_eq!(MATCH, spans[0].style);
        assert_eq!("src", &*spans[0].content);
        assert_eq!(PLAIN, spans[1].style);

        // An index past the end of the label is ignored rather than panicking.
        let spans = highlight("ab", &[0, 99], 40);
        assert_eq!("ab", text(&Line::from(spans)));

        // A long label loses its head and says so.
        let spans = highlight("a/very/long/path/to/a/file.rs", &[0], 10);
        let shown = text(&Line::from(spans));
        assert!(shown.starts_with('…'), "{shown}");
        assert!(shown.ends_with("file.rs"), "{shown}");
        assert_eq!(10, shown.chars().count(), "{shown}");

        assert!(highlight("anything", &[0], 0).is_empty());
    }

    #[test]
    fn counts_are_grouped_the_way_the_printed_table_groups_them() {
        assert_eq!("1,234,567", number(1_234_567));
        assert_eq!("999", number(999));
        assert_eq!("0", number(0));
    }

    #[test]
    fn an_overlay_stays_inside_the_screen() {
        let screen = Rect::new(0, 0, 40, 20);
        let panel = overlay(screen, 20, 10);
        assert_eq!(Rect::new(10, 5, 20, 10), panel);
        // Asked for more than there is, it shrinks instead of overflowing.
        let panel = overlay(screen, 200, 200);
        assert_eq!(screen, panel);
        assert_eq!(Rect::new(0, 0, 0, 0), overlay(Rect::ZERO, 10, 10));
    }

    #[test]
    fn the_saved_view_line_says_where_the_bookmark_points() {
        let view = SavedView {
            name: "widget".to_string(),
            path: vec!["/repos/widget".to_string(), "2026-06".to_string()],
            grain: crate::tui::state::Grain::Day,
            sort: Sort::default(),
            filter: "src".to_string(),
        };
        assert_eq!(
            "/repos/widget › 2026-06 · day · filter src",
            describe_view(&view)
        );
        assert_eq!("overview · month", describe_view(&SavedView::default()));
    }

    #[test]
    fn every_diff_line_kind_gets_its_own_colour() {
        assert_ne!(diff_style(DiffKind::Added), diff_style(DiffKind::Removed));
        assert_ne!(diff_style(DiffKind::Hunk), diff_style(DiffKind::Context));
        assert_eq!(PLAIN, diff_style(DiffKind::Context));
    }
}
