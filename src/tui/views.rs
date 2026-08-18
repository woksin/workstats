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
use crate::output::{is_direction_override, number};

/// The selection marker and the width `Table` reserves for it. Reserving it
/// always keeps the columns from jumping sideways when a selection appears.
const MARKER: &str = "› ";
const MARKER_WIDTH: u16 = 2;
/// `Table`'s own default, repeated here because `column_widths` has to leave
/// room for the gap the widget draws between two columns.
const COLUMN_SPACING: u16 = 1;
/// The narrowest a text column is allowed to become. Below this it stops
/// carrying a readable path, so the column is dropped rather than shown as a
/// stub.
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
    // Every width is resolved here, so `Table` is handed lengths that already
    // fit and a cell can be shortened to the room it will actually get instead
    // of being cut mid-word by the widget.
    let widths = column_widths(columns, rows, width);
    let columns = &columns[..widths.len()];
    let titles: Vec<Cell<'static>> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let sorted = (index == sort.column).then_some(sort.descending);
            let room = widths.get(index).copied().unwrap_or(0) as usize;
            Cell::from(aligned(header_text(column, sorted, room), column.numeric))
                .style(if sorted.is_some() { SORTED } else { HEAD })
        })
        .collect();
    let header = Row::new(titles);
    let body: Vec<Row<'static>> = rows
        .iter()
        .map(|entry| body_row(entry, columns, &widths))
        .collect();
    Table::new(body, widths.iter().map(|width| Constraint::Length(*width)))
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

/// The column header: its title, and an arrow on the one the rows are sorted
/// by. It carries no sort key: `1`–`9` still select a column, but a bare digit
/// in front of every title reads as part of the title — and the sorted column,
/// which shows an arrow instead, made the row look like `1 2 _ 4 5`. The key
/// map is spelled out in the `?` overlay, beside the column each key sorts.
fn header_text(column: &Column, sorted: Option<bool>, width: usize) -> String {
    let arrow = match sorted {
        Some(true) => " ▼",
        Some(false) => " ▲",
        None => "",
    };
    let title = column.title;
    if title.chars().count() + arrow.chars().count() <= width {
        format!("{title}{arrow}")
    } else {
        shorten(title, width)
    }
}

/// Room for the ` ▼` a sorted column carries. Reserved on every column, not
/// only the sorted one, so that changing the sort does not shuffle the whole
/// table sideways.
const ARROW_WIDTH: u16 = 2;

/// The widths every column would like: enough for its title plus the sort
/// arrow, and enough for the widest value actually under it.
///
/// Measured from the rows rather than declared in the schema, because a
/// declared width is wrong in both directions — it clips the longest path on a
/// wide terminal, and it left `Repository` stretched across half the screen
/// beside three short names. Measuring every row rather than the visible ones
/// keeps the width steady while the reader scrolls; it does move when a filter
/// changes which rows exist, which is the one moment a reader expects the table
/// to redraw anyway.
fn wanted_widths(columns: &[Column], rows: &[Entry]) -> Vec<u16> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let title = cells(column.title.chars().count()).saturating_add(ARROW_WIDTH);
            rows.iter()
                .filter_map(|row| row.fields.get(index))
                .map(|field| cells(field.text.chars().count()))
                .fold(title, u16::max)
        })
        .collect()
}

/// What a column shrinks to before it is dropped instead. A number keeps its
/// full width — half a number is worse than no number — so only text gives
/// ground, and only down to the point where a path still identifies something.
fn floor_widths(columns: &[Column], wanted: &[u16]) -> Vec<u16> {
    columns
        .iter()
        .zip(wanted)
        .map(|(column, wanted)| {
            if column.numeric {
                *wanted
            } else {
                (*wanted).min(FILL_FLOOR)
            }
        })
        .collect()
}

/// The resolved width of every column that fits, in order; its length is how
/// many columns are shown. Trailing columns are dropped rather than squeezed,
/// and the status bar still names the column the rows are sorted by when it has
/// gone off screen.
///
/// Whatever room is left over after every column has what its content needs is
/// simply not spent. Handing it to one column is what pushed `Source root` half
/// a screen away from the repository it belongs to; a table that ends before
/// the right edge keeps figures that are read together next to each other.
fn column_widths(columns: &[Column], rows: &[Entry], width: u16) -> Vec<u16> {
    if columns.is_empty() {
        return Vec::new();
    }
    let wanted = wanted_widths(columns, rows);
    let floors = floor_widths(columns, &wanted);
    let room = width.saturating_sub(MARKER_WIDTH);

    // How many columns fit at the width below which they stop being worth
    // showing.
    let mut widths: Vec<u16> = Vec::with_capacity(columns.len());
    let mut used: u16 = 0;
    for floor in &floors {
        let gap = if widths.is_empty() { 0 } else { COLUMN_SPACING };
        let next = used.saturating_add(gap).saturating_add(*floor);
        if !widths.is_empty() && next > room {
            break;
        }
        used = next;
        widths.push(*floor);
    }
    // The first column carries the row's identity, so it is shown even on a
    // terminal too narrow for it — clipped to whatever room there is.
    if widths[0] > room {
        widths[0] = room;
        used = room;
    }

    // Then spend what is left widening the text columns towards their content,
    // left to right: the leftmost is the one that names the row.
    let mut spare = room.saturating_sub(used);
    for (index, width) in widths.iter_mut().enumerate() {
        let grow = wanted[index].saturating_sub(*width).min(spare);
        *width += grow;
        spare -= grow;
    }
    widths
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
            Span::styled(format!(" {}", noun(lines, "diff line", "diff lines")), DIM),
        ];
    }
    let rows = app.rows().len();
    let mut spans = vec![
        Span::styled(number(rows as u64), PLAIN),
        Span::styled(
            format!(" {}", noun(rows, one_row(app.level()), app.level().label())),
            DIM,
        ),
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
        draw_help(frame, area, app.columns());
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

/// The gap between the key list and the sort keys beside it.
const HELP_GAP: u16 = 3;

fn draw_help(frame: &mut Frame, area: Rect, columns: &[Column]) {
    let keys = KEYBINDINGS
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = KEYBINDINGS
        .iter()
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(format!("{key:<keys$}"), ACCENT),
                Span::styled("  ", DIM),
                Span::styled(*description, PLAIN),
            ])
        })
        .collect();
    // Measured from the lines themselves rather than estimated: the widest key
    // and the longest description are on different rows, and a panel sized for
    // their sum clipped the end of the longest sentence in it.
    let sorts = sort_key_lines(columns);
    let left_width = widest_line(&lines);
    let right_width = widest_line(&sorts);
    let beside = if right_width == 0 {
        0
    } else {
        HELP_GAP + right_width
    };
    // 2 borders and the block's own horizontal padding.
    let outer = overlay(
        area,
        left_width.saturating_add(beside).saturating_add(4),
        cells(KEYBINDINGS.len() + 2),
    );
    let inner = panel(frame, outer, "Keys", "? or Esc closes");
    if inner.is_empty() {
        return;
    }
    // On a terminal too narrow for both, the sort keys go rather than the
    // sentences: they are a reminder of what the key list already says, and half
    // a sentence beside a full list of columns helps nobody.
    let beside = (inner.width >= left_width.saturating_add(beside)).then_some(right_width);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(beside.unwrap_or(0))])
            .spacing(if beside.is_some() { HELP_GAP } else { 0 })
            .areas(inner);
    // Spend the last visible row on saying that there is more rather than
    // letting the list end without a word.
    let rows = left.height as usize;
    lines.truncate(rows);
    if KEYBINDINGS.len() > rows
        && let Some(last) = lines.last_mut()
    {
        *last = Line::styled(format!("… {} more", KEYBINDINGS.len() - rows + 1), DIM);
    }
    frame.render_widget(Paragraph::new(lines), left);
    if beside.is_some() {
        frame.render_widget(Paragraph::new(sorts), right);
    }
}

/// The `1`–`9` keys written against the columns they sort at this level.
///
/// This is where the digits live now. In the header they read as part of the
/// column title and told a reader who had never pressed `?` nothing at all;
/// here they sit under the sentence that explains them, and they can name the
/// columns of the level actually on screen — which a fixed `1 – 9` line cannot,
/// because every level has a different table.
fn sort_key_lines(columns: &[Column]) -> Vec<Line<'static>> {
    if columns.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::styled("Sort keys", HEAD)];
    // Past nine there is no key to press, so there is nothing to list.
    lines.extend(columns.iter().take(9).enumerate().map(|(index, column)| {
        Line::from(vec![
            Span::styled(format!("{}  ", index + 1), ACCENT),
            Span::styled(column.title, PLAIN),
        ])
    }));
    lines
}

fn widest_line(lines: &[Line<'static>]) -> u16 {
    lines
        .iter()
        .map(|line| {
            cells(
                line.iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>(),
            )
        })
        .max()
        .unwrap_or(0)
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
            format!(
                "   {} {}",
                number(hits.len() as u64),
                noun(hits.len(), "match", "matches")
            ),
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

/// Control characters and direction overrides are replaced before anything is
/// drawn: a Git path or a diff line is text this tool did not choose, an escape
/// sequence in one would otherwise be handed straight to the terminal, and an
/// override would reorder the cells around it so a row reads as a repository or
/// a file it is not. The diff pane's `clip` refuses the same two classes.
///
/// Each replacement is one character wide, so the widths the caller counts
/// still match the cells the terminal draws.
fn safe_chars(text: &str) -> Vec<char> {
    text.chars()
        .map(|character| {
            if character.is_control() || is_direction_override(character) {
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

/// The noun that agrees with `value`, the way `output::counted` picks one. Both
/// spellings are given because these nouns are not all a plain `s` apart.
fn noun<'a>(value: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if value == 1 { singular } else { plural }
}

/// What one row of a level is called. `LevelKind::label` is the plural the
/// status bar and the empty state are written in, and a level holding a single
/// row needs the singular of it. Spelled out rather than derived: `repositories`
/// and `categories` do not lose a plain `s`, and `history` and `diff` are not
/// plurals at all, so they stay as they are. The match is exhaustive, so a new
/// level cannot be added without answering this.
const fn one_row(level: LevelKind) -> &'static str {
    match level {
        LevelKind::Overview => "repository",
        LevelKind::Repo => "period",
        LevelKind::Period => "category",
        LevelKind::Category => "commit",
        LevelKind::Commit => "file",
        LevelKind::File | LevelKind::Diff => level.label(),
    }
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
    use crate::tui::app::app_for_test;
    use crate::tui::event::Action;
    use crate::tui::state::{Dataset, Field, columns, sample_commit};

    /// Three short repository names and figures worth grouping: the shape of
    /// the report the explorer looked broken on. Rendering it is the only way
    /// to see what a reader sees, because `workstats ui` needs a real terminal.
    fn wide_app() -> (App, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut data = Dataset::from_commits(vec![
            sample_commit(
                "aaaaaaaaaaaa",
                "/repos/workstats",
                &[("src/main.rs", 12_205, 3_477), ("README.md", 1_048, 96)],
            ),
            sample_commit("bbbbbbbbbbbb", "/repos/widget", &[("src/lib.rs", 640, 210)]),
            sample_commit("cccccccccccc", "/repos/gadget", &[("tests/api.rs", 88, 4)]),
        ]);
        data.summary = vec![
            (
                "Observed".to_string(),
                "2026-01-04 → 2026-08-18".to_string(),
            ),
            ("Commits".to_string(), number(1_048_u64)),
            ("Tokens".to_string(), number(60_471_298_552_u64)),
        ];
        let app = app_for_test(data, directory.path().join("views.json"));
        (app, directory)
    }

    /// The whole screen as text, row by row, with the trailing blanks trimmed.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect();
                row.trim_end().to_string()
            })
            .collect()
    }

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
            for schema in [columns(LevelKind::Overview), columns(LevelKind::Diff)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        draw_help(frame, area, schema);
                    })
                    .unwrap();
            }
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
        let rows = entries(schema.len(), 4);
        assert_eq!(schema.len(), column_widths(schema, &rows, 400).len());
        assert_eq!(1, column_widths(schema, &rows, 1).len());
        assert!(column_widths(&[], &rows, 80).is_empty());
        // Narrowing never adds a column back.
        let mut previous = schema.len();
        for width in (1..400_u16).rev() {
            let visible = column_widths(schema, &rows, width).len();
            assert!(visible <= previous, "width {width}");
            previous = visible;
        }
    }

    #[test]
    fn resolved_column_widths_never_exceed_the_area() {
        for kind in [LevelKind::Overview, LevelKind::Category, LevelKind::Commit] {
            let schema = columns(kind);
            let rows = entries(schema.len(), 4);
            for width in 1..400_u16 {
                let widths = column_widths(schema, &rows, width);
                // Everything the columns claim has to fit in what is left after
                // the selection marker, or a cell would be shortened to a width
                // the widget never gives it.
                let claimed: u32 = widths.iter().map(|value| u32::from(*value)).sum::<u32>()
                    + u32::from(COLUMN_SPACING) * widths.len().saturating_sub(1) as u32;
                let room = u32::from(width.saturating_sub(MARKER_WIDTH));
                assert!(claimed <= room, "{kind:?} at width {width}: {widths:?}");
            }
        }
    }

    /// A column is as wide as the widest thing it has to show and no wider.
    /// `Repository` stretched to half a wide terminal beside three short names
    /// is the defect this measures against.
    #[test]
    fn a_column_is_as_wide_as_its_content_and_leftover_room_is_left_alone() {
        let schema = columns(LevelKind::Overview);
        let rows = vec![Entry {
            id: "workstats".to_string(),
            fields: vec![
                Field::text("workstats"),
                Field::text("studio"),
                Field::count(3),
                Field::count(12),
                Field::count(12_205),
                Field::count(3_477),
                Field::lines(8_728),
                Field::hours(0.0),
                Field::hours(0.0),
            ],
        }];
        let widths = column_widths(schema, &rows, 200);
        assert_eq!(schema.len(), widths.len());
        // Title plus room for the sort arrow, because every value under these
        // is shorter than the title above it.
        assert_eq!(vec![12, 13, 9, 7, 7, 9, 6, 6, 9], widths);
        // The table therefore ends well before the right edge instead of
        // pushing `Source root` half a screen away from the repository.
        let used: u16 =
            widths.iter().sum::<u16>() + COLUMN_SPACING * (widths.len() as u16 - 1) + MARKER_WIDTH;
        assert!(used < 100, "{used} of 200 cells");

        // A longer value widens its own column, and only its own.
        let mut wider = rows.clone();
        wider[0].fields[0] = Field::text("a-considerably-longer-repository");
        let widths = column_widths(schema, &wider, 200);
        assert_eq!(32, widths[0]);
        assert_eq!(13, widths[1]);
    }

    #[test]
    fn a_header_shows_its_title_and_the_sort_arrow() {
        let column = Column {
            title: "Commits",
            numeric: true,
        };
        // No sort key in front of it: a bare digit reads as part of the title,
        // and the sorted column could not show one at all.
        assert_eq!("Commits", header_text(&column, None, 9));
        assert_eq!("Commits ▼", header_text(&column, Some(true), 9));
        assert_eq!("Commits ▲", header_text(&column, Some(false), 11));
        // Too narrow for the arrow, so the title keeps the room.
        assert_eq!("Commits", header_text(&column, Some(true), 8));
        assert_eq!("Commi…", header_text(&column, Some(true), 6));
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
    fn a_row_cannot_reorder_itself() {
        // U+202E would draw the rest of the row right-to-left, so a file named
        // `gnp.exe` could sit in the explorer looking like `exe.png`.
        assert_eq!(vec!['a', '·', 'b'], safe_chars("a\u{202e}b"));
        assert_eq!(
            "path·to·file·",
            shorten("path\u{202e}to\u{2066}file\u{202c}", 20)
        );
        // One character in, one cell out: the override does not buy extra room.
        assert_eq!("…c/d·e", shorten_path("a/b/c/d\u{202e}e", 6));
        // U+200F opens no directional scope, so it cannot reorder the row, and
        // it is ordinary content in a Hebrew directory name. Escaped rather
        // than written literally so that this file carries no invisible
        // characters of its own.
        let hebrew = "~/\u{5de}\u{5e1}\u{5de}\u{5db}\u{5d9}\u{5dd}\u{200f}/log";
        assert_eq!(hebrew, shorten(hebrew, 20));
    }

    /// The status bar counts the rows of whichever level is open, so every
    /// level needs a name that works with a `1` in front of it.
    #[test]
    fn the_status_bar_counts_agree_with_the_nouns_beside_them() {
        assert_eq!("repository", noun(1, one_row(LevelKind::Overview), "x"));
        assert_eq!(
            "repositories",
            noun(4, one_row(LevelKind::Overview), LevelKind::Overview.label())
        );
        assert_eq!("commit", one_row(LevelKind::Category));
        assert_eq!("file", one_row(LevelKind::Commit));
        // Neither of these is a plural to begin with, so neither has a
        // singular of its own to fall back to.
        assert_eq!(LevelKind::File.label(), one_row(LevelKind::File));
        assert_eq!(LevelKind::Diff.label(), one_row(LevelKind::Diff));
        // Zero takes the plural, the way "no repositories match" reads.
        assert_eq!(
            "periods",
            noun(0, one_row(LevelKind::Repo), LevelKind::Repo.label())
        );
        assert_eq!("diff line", noun(1, "diff line", "diff lines"));
        assert_eq!("matches", noun(2, "match", "matches"));
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

    /// The status bar, the diff position and the search hit count all go
    /// through `output::number`, which is the printed report's own formatter —
    /// so there is no second spelling of a count left to drift.
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

    /// What a reader on a wide terminal actually sees. Written against the
    /// rendered cells rather than the pieces that build them, because every one
    /// of the defects this guards against — sort digits reading as part of a
    /// title, a space inside a number reading as a column gap, one column
    /// stretched across half the screen — was invisible until the whole frame
    /// was on screen at once.
    #[test]
    fn the_overview_reads_as_a_table_on_a_wide_terminal() {
        let (mut app, _directory) = wide_app();
        let screen = screen(&mut app, 200, 50);

        // No sort key in front of a title, and so no apparent gap where the
        // sorted column's key used to be replaced by an arrow.
        let header = &screen[2];
        assert_eq!(
            "  Repository   Source root   Commits ▼   Files   Added   Removed    Net   AI h   Human h",
            header
        );
        assert!(
            !header.chars().any(|character| character.is_ascii_digit()),
            "{header}"
        );

        // Thousands separated with a comma, the way the printed report
        // separates them. A space here would be indistinguishable from the
        // space between two columns.
        assert!(screen[3].contains("13,253"), "{}", screen[3]);
        assert!(screen[1].contains("Tokens 60,471,298,552"), "{}", screen[1]);

        // Every column is as wide as its content needs, so the table ends long
        // before the right edge instead of pushing `Source root` away from the
        // repository it belongs to.
        assert!(header.chars().count() < 100, "{header}");
        let gap = header.find("Source root").expect("a second column")
            - header.find("Repository").expect("a first column")
            - "Repository".len();
        assert!(gap <= 4, "{gap} cells between the first two columns");

        // The row count in the footer is a count, and now that no header
        // carries a digit it can no longer be read as another key hint.
        assert!(
            screen[49].starts_with("3 repositories · sort Commits ▼"),
            "{}",
            screen[49]
        );
    }

    /// The sort keys moved out of the header and into the one place a reader
    /// goes when they want to know what a key does.
    #[test]
    fn the_help_overlay_names_the_column_each_sort_key_selects() {
        let (mut app, _directory) = wide_app();
        app.apply(Action::ToggleHelp);
        let panel = screen(&mut app, 200, 50).join("\n");
        assert!(panel.contains("Sort keys"), "{panel}");
        assert!(panel.contains("1  Repository"), "{panel}");
        assert!(panel.contains("3  Commits"), "{panel}");
        assert!(panel.contains("9  Human h"), "{panel}");
        // The panel is sized from the lines it draws, so the longest sentence
        // in it survives to its full stop.
        assert!(
            panel.contains("sort by the numbered column; press again to reverse"),
            "{panel}"
        );

        // A level with a different table gets a different list.
        app.apply(Action::Descend);
        app.apply(Action::Descend);
        let panel = screen(&mut app, 200, 50).join("\n");
        assert!(panel.contains("1  Category"), "{panel}");
        assert!(panel.contains("6  Share"), "{panel}");
        assert!(!panel.contains("Repository"), "{panel}");
    }

    /// The sort keys are the first thing the help panel gives up, because a
    /// clipped sentence is worse than a missing reminder.
    #[test]
    fn a_narrow_help_overlay_keeps_the_sentences_and_drops_the_sort_keys() {
        let (mut app, _directory) = wide_app();
        app.apply(Action::ToggleHelp);
        let panel = screen(&mut app, 60, 30).join("\n");
        assert!(!panel.contains("Sort keys"), "{panel}");
        assert!(panel.contains("descend into the selected row"), "{panel}");
    }

    /// `clip` shortens a cell to the width this module resolved, so a width the
    /// widget then disagrees with would cut a value in half without an ellipsis
    /// to say so.
    #[test]
    fn the_widths_this_module_resolves_are_the_widths_the_widget_draws() {
        let schema = columns(LevelKind::Overview);
        let rows = entries(schema.len(), 3);
        for width in [40_u16, 80, 120, 200] {
            let widths = column_widths(schema, &rows, width);
            let mut terminal = Terminal::new(TestBackend::new(width, 6)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let widget = table(schema, &rows, Sort::default(), area.width);
                    let mut state = TableState::new().with_selected(Some(0));
                    frame.render_stateful_widget(widget, area, &mut state);
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            // Cell by cell, not byte by byte: an arrow is three bytes wide and
            // one column wide, and it is the columns that have to line up.
            let header: Vec<String> = (0..width)
                .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol().to_string()))
                .collect();
            // Each title lands inside the cells this module set aside for it.
            let mut x = MARKER_WIDTH as usize;
            for (index, resolved) in widths.iter().enumerate() {
                let sorted = (index == Sort::default().column).then_some(false);
                let expected = header_text(&schema[index], sorted, *resolved as usize);
                let drawn: String = header[x..x + *resolved as usize].concat();
                assert_eq!(expected, drawn.trim(), "width {width}, column {index}");
                x += *resolved as usize + COLUMN_SPACING as usize;
            }
        }
    }
}
