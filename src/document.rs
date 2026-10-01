//! A small, format-neutral document model and its two prose renderers:
//! GitHub-flavoured Markdown and a single self-contained HTML page.
//!
//! The report and `allocate` each describe what they print once, as a
//! `Document` built from the figures the table view already formats. The
//! renderers know nothing about workstats, so a figure cannot read differently
//! in Markdown than it does in the terminal, and the escaping rules live in one
//! place instead of being remembered at every call site.
//!
//! Every string that reaches a renderer is first passed through
//! [`safe_text`], the same filter the table, CSV and
//! explorer use, and then escaped for the target syntax. A repository name is
//! text this tool did not write, and a page or a PR comment is read by people
//! who never saw the terminal it came from: control characters and Trojan
//! Source direction overrides are as unwelcome there as in a terminal.

use crate::output::safe_text;

/// One document: a title and the blocks below it, in reading order.
pub(crate) struct Document {
    pub(crate) title: String,
    pub(crate) blocks: Vec<Block>,
}

pub(crate) enum Block {
    /// A heading for the blocks that follow.
    Section(String),
    /// Label/value pairs: the summary the table view prints as indented lines.
    Facts(Vec<(String, String)>),
    Table(Table),
    Paragraph(String),
    List(Vec<String>),
    Heatmap(Heatmap),
}

/// A calendar of one figure per day, already reduced to shade levels. The
/// renderers draw it and know nothing about what the figure is: the title of a
/// cell carries the date and the figure as text, and the legend says what the
/// shades mean.
pub(crate) struct Heatmap {
    pub(crate) grids: Vec<HeatGrid>,
    pub(crate) legend: String,
}

/// One grid: ISO weeks as columns, Monday first, seven rows.
pub(crate) struct HeatGrid {
    pub(crate) label: String,
    pub(crate) columns: usize,
    pub(crate) cells: Vec<HeatCell>,
    /// A month name and the column it starts at, for the ruler above the grid.
    pub(crate) months: Vec<(usize, String)>,
}

pub(crate) struct HeatCell {
    pub(crate) column: usize,
    /// 0 is Monday.
    pub(crate) row: usize,
    /// 0 is no activity; 1 to 4 are the quartiles of the days that had some.
    pub(crate) level: u8,
    /// What a hover shows: the date and the figure.
    pub(crate) title: String,
}

/// Shade glyphs by level. A space for level 0 keeps a Markdown code block quiet;
/// the terminal view swaps in a dot so the shape of the grid shows.
const SHADES: [char; 5] = [' ', '░', '▒', '▓', '█'];
const WEEKDAYS: [&str; 7] = ["Mon", "", "Wed", "", "Fri", "", "Sun"];
const LABEL_WIDTH: usize = 4;

/// The month names of a grid laid out over its columns. A name that would run
/// into the previous one is left out rather than printed over it, and only
/// ASCII letters are kept, so the ruler is safe inside a Markdown code fence.
pub(crate) fn heatmap_ruler(grid: &HeatGrid) -> String {
    let mut ruler = vec![' '; grid.columns];
    let mut next_free = 0;
    for (column, name) in &grid.months {
        let name: Vec<char> = name
            .chars()
            .filter(char::is_ascii_alphabetic)
            .take(3)
            .collect();
        if *column < next_free {
            continue;
        }
        if ruler.len() < column + name.len() {
            ruler.resize(column + name.len(), ' ');
        }
        for (offset, character) in name.iter().enumerate() {
            ruler[column + offset] = *character;
        }
        next_free = column + name.len() + 1;
    }
    ruler.into_iter().collect::<String>().trim_end().to_string()
}

/// A grid as lines of text: a month ruler, then one line per weekday. A day
/// with no activity is drawn as `empty`; a day outside the window as a space.
/// Only the shade glyphs, ASCII letters and spaces can appear, whatever the
/// grid holds, so the lines are safe inside a Markdown code fence.
pub(crate) fn heatmap_lines(grid: &HeatGrid, empty: char) -> Vec<String> {
    let mut rows = vec![vec![' '; grid.columns]; 7];
    for cell in &grid.cells {
        if let Some(slot) = rows
            .get_mut(cell.row)
            .and_then(|row| row.get_mut(cell.column))
        {
            *slot = if cell.level == 0 {
                empty
            } else {
                SHADES[usize::from(cell.level).min(4)]
            };
        }
    }
    let mut lines = vec![format!("{:LABEL_WIDTH$}{}", "", heatmap_ruler(grid))];
    for (label, row) in WEEKDAYS.iter().zip(rows) {
        let line: String = row.into_iter().collect();
        lines.push(format!("{label:LABEL_WIDTH$}{}", line.trim_end()));
    }
    lines
}

pub(crate) struct Column {
    pub(crate) label: String,
    /// Right-aligned, because the cells are figures.
    pub(crate) numeric: bool,
}

impl Column {
    pub(crate) fn text(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            numeric: false,
        }
    }

    pub(crate) fn number(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            numeric: true,
        }
    }
}

pub(crate) struct Table {
    pub(crate) columns: Vec<Column>,
    pub(crate) rows: Vec<Vec<String>>,
    /// A closing row that sums or concludes the rows above it.
    pub(crate) total: Option<Vec<String>>,
}

impl Table {
    pub(crate) fn new(columns: Vec<Column>, rows: Vec<Vec<String>>) -> Self {
        Self {
            columns,
            rows,
            total: None,
        }
    }
}

/// Backslash-escapes every character that Markdown, GitHub's table syntax, or
/// GitHub's inline HTML handling would otherwise act on, and defuses the
/// references GitHub links from plain text.
///
/// `|` would end a table cell and start a new one, `<` could open an HTML tag,
/// `` ` `` `*` `_` `~` `[` `]` are emphasis, code and link syntax, `&` starts an
/// entity, and `\` would swallow the escape that follows it. All of them are
/// ASCII punctuation, which CommonMark lets a backslash escape, so the text
/// still renders as written. `>` is included so a value that begins a line
/// cannot become a quote. Newlines and other control characters have already
/// been replaced by [`safe_text`], so a value cannot leave its table row.
///
/// A repository called `@scope/pkg` would notify a user or team when the report
/// is pasted into a PR, and `#123` or `GH-123` would link someone else's issue.
/// GitHub finds those in the rendered text, where a backslash is already gone,
/// so the escape cannot be one: a zero-width space after the `@`, and between
/// the marker and its number, breaks the match without changing how the text
/// reads. The table and the HTML page are untouched by this: only Markdown is
/// pasted somewhere that links.
pub(crate) fn escape_markdown(value: &str) -> String {
    const ZERO_WIDTH_SPACE: char = '\u{200B}';
    let characters: Vec<char> = value.chars().collect();
    let mut escaped = String::with_capacity(value.len());
    for (index, &character) in characters.iter().enumerate() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' | '&'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
        let next_is_digit = characters.get(index + 1).is_some_and(char::is_ascii_digit);
        let after_gh = character == '-'
            && index >= 2
            && characters[index - 2].eq_ignore_ascii_case(&'G')
            && characters[index - 1].eq_ignore_ascii_case(&'H');
        if character == '@' || (next_is_digit && (character == '#' || after_gh)) {
            escaped.push(ZERO_WIDTH_SPACE);
        }
    }
    escaped
}

/// Replaces the five characters that mean something in HTML text or in a quoted
/// attribute. Applied to every value, including the ones this tool wrote, so
/// there is no list of "trusted" strings to keep correct.
pub(crate) fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn markdown_text(value: &str) -> String {
    escape_markdown(&safe_text(value))
}

fn html_text(value: &str) -> String {
    escape_html(&safe_text(value))
}

/// GitHub-flavoured Markdown, ending in a newline. Tables keep their column
/// alignment, so a pasted report lines up the way the terminal one does.
pub(crate) fn render_markdown(document: &Document) -> String {
    let mut output = format!("# {}\n", markdown_text(&document.title));
    for block in &document.blocks {
        output.push('\n');
        match block {
            Block::Section(title) => output.push_str(&format!("## {}\n", markdown_text(title))),
            Block::Paragraph(text) => {
                output.push_str(&markdown_text(text));
                output.push('\n');
            }
            Block::List(items) => {
                for item in items {
                    output.push_str(&format!("- {}\n", markdown_text(item)));
                }
            }
            Block::Facts(facts) => {
                output.push_str("| Measure | Value |\n| --- | --- |\n");
                for (label, value) in facts {
                    output.push_str(&format!(
                        "| {} | {} |\n",
                        markdown_text(label),
                        markdown_text(value)
                    ));
                }
            }
            Block::Table(table) => render_markdown_table(&mut output, table),
            Block::Heatmap(heatmap) => render_markdown_heatmap(&mut output, heatmap),
        }
    }
    output
}

fn render_markdown_heatmap(output: &mut String, heatmap: &Heatmap) {
    for grid in &heatmap.grids {
        output.push_str(&format!("### {}\n\n```text\n", markdown_text(&grid.label)));
        for line in heatmap_lines(grid, ' ') {
            output.push_str(line.trim_end());
            output.push('\n');
        }
        output.push_str("```\n\n");
    }
    output.push_str(&markdown_text(&heatmap.legend));
    output.push('\n');
}

fn render_markdown_table(output: &mut String, table: &Table) {
    let row = |cells: Vec<String>| format!("| {} |\n", cells.join(" | "));
    output.push_str(&row(table
        .columns
        .iter()
        .map(|column| markdown_text(&column.label))
        .collect()));
    output.push_str(&row(table
        .columns
        .iter()
        .map(|column| if column.numeric { "---:" } else { "---" }.to_string())
        .collect()));
    for cells in &table.rows {
        output.push_str(&row(cells.iter().map(|cell| markdown_text(cell)).collect()));
    }
    if let Some(total) = &table.total {
        output.push_str(&row(total
            .iter()
            .map(|cell| {
                // Bold markers around nothing would print as four asterisks.
                if cell.is_empty() {
                    String::new()
                } else {
                    format!("**{}**", markdown_text(cell))
                }
            })
            .collect()));
    }
}

/// Inline CSS only, and deliberately small: the page has to be readable when
/// saved, mailed, or opened from a wiki attachment with no network at all.
/// `prefers-color-scheme` follows the reader's system without a script, and the
/// fonts are the system's own, so nothing is fetched.
const STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fff;--fg:#1f2328;--muted:#59636e;--line:#d1d9e0;--head:#f6f8fa;--accent:#0969da;--l0:#ebedf0;--l1:#9be9a8;--l2:#40c463;--l3:#30a14e;--l4:#216e39}\
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--line:#3d444d;--head:#151b23;--accent:#4493f8;--l0:#161b22;--l1:#0e4429;--l2:#006d32;--l3:#26a641;--l4:#39d353}}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif}\
main{max-width:70rem;margin:0 auto;padding:2rem 1rem}\
h1{font-size:1.6rem;margin:0 0 1rem}\
h2{font-size:1.15rem;margin:2rem 0 .5rem;padding-bottom:.25rem;border-bottom:1px solid var(--line)}\
p,li{color:var(--muted);margin:.4rem 0}\
.scroll{overflow-x:auto}\
table{border-collapse:collapse;margin:.5rem 0;width:100%}\
th,td{padding:.35rem .7rem;border:1px solid var(--line);text-align:left;vertical-align:top}\
thead th,tbody th{background:var(--head);font-weight:600}\
.num{text-align:right;font-variant-numeric:tabular-nums}\
tfoot td{font-weight:600;background:var(--head)}\
ul{padding-left:1.4rem}\
h3{font-size:1rem;margin:1rem 0 .25rem}\
svg.cal{display:block}\
svg.cal text{fill:var(--muted);font-size:9px}\
svg.cal .l0{fill:var(--l0)}svg.cal .l1{fill:var(--l1)}svg.cal .l2{fill:var(--l2)}svg.cal .l3{fill:var(--l3)}svg.cal .l4{fill:var(--l4)}";

/// A single static HTML page: inline CSS, no script, and no reference to any
/// other resource. A `Content-Security-Policy` of `default-src 'none'` says so
/// to the browser too, so a page that is later edited or embedded cannot start
/// fetching from anywhere without the policy being removed first.
pub(crate) fn render_html(document: &Document) -> String {
    let title = html_text(&document.title);
    let mut output = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<meta name=\"color-scheme\" content=\"light dark\">\n\
<title>{title}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n<h1>{title}</h1>\n"
    );
    for block in &document.blocks {
        match block {
            Block::Section(title) => output.push_str(&format!("<h2>{}</h2>\n", html_text(title))),
            Block::Paragraph(text) => output.push_str(&format!("<p>{}</p>\n", html_text(text))),
            Block::List(items) => {
                output.push_str("<ul>\n");
                for item in items {
                    output.push_str(&format!("<li>{}</li>\n", html_text(item)));
                }
                output.push_str("</ul>\n");
            }
            Block::Facts(facts) => {
                output.push_str("<div class=\"scroll\"><table>\n<tbody>\n");
                for (label, value) in facts {
                    output.push_str(&format!(
                        "<tr><th scope=\"row\">{}</th><td>{}</td></tr>\n",
                        html_text(label),
                        html_text(value)
                    ));
                }
                output.push_str("</tbody>\n</table></div>\n");
            }
            Block::Table(table) => render_html_table(&mut output, table),
            Block::Heatmap(heatmap) => render_html_heatmap(&mut output, heatmap),
        }
    }
    output.push_str("</main>\n</body>\n</html>\n");
    output
}

/// Each cell is the side of a square, and the pitch leaves a gap between them.
const CELL: usize = 11;
const PITCH: usize = 13;
const LEFT_MARGIN: usize = 28;
const TOP_MARGIN: usize = 14;

/// An inline `<svg>` per grid: no script, no link, no external reference. Every
/// attribute that is not a number this function computed is a class name from
/// a fixed list, and the only text is inside `<title>` and `<text>`, both
/// escaped like any other value. The colours are classes in `STYLE`, so dark
/// mode and the CSP (`style-src 'unsafe-inline'`) are the page's, not the
/// graphic's.
fn render_html_heatmap(output: &mut String, heatmap: &Heatmap) {
    for grid in &heatmap.grids {
        let label = html_text(&grid.label);
        let width = LEFT_MARGIN + grid.columns * PITCH;
        let height = TOP_MARGIN + 7 * PITCH;
        output.push_str(&format!(
            "<h3>{label}</h3>\n<div class=\"scroll\"><svg class=\"cal\" viewBox=\"0 0 {width} {height}\" width=\"{width}\" height=\"{height}\" role=\"img\" aria-label=\"{label}\">\n"
        ));
        for (column, name) in &grid.months {
            let name: String = name
                .chars()
                .filter(char::is_ascii_alphabetic)
                .take(3)
                .collect();
            output.push_str(&format!(
                "<text x=\"{}\" y=\"{}\">{}</text>\n",
                LEFT_MARGIN + column * PITCH,
                TOP_MARGIN - 4,
                html_text(&name)
            ));
        }
        for (row, weekday) in WEEKDAYS
            .iter()
            .enumerate()
            .filter(|(_, name)| !name.is_empty())
        {
            output.push_str(&format!(
                "<text x=\"0\" y=\"{}\">{weekday}</text>\n",
                TOP_MARGIN + row * PITCH + CELL - 2
            ));
        }
        for cell in &grid.cells {
            output.push_str(&format!(
                "<rect class=\"l{}\" x=\"{}\" y=\"{}\" width=\"{CELL}\" height=\"{CELL}\" rx=\"2\"><title>{}</title></rect>\n",
                cell.level.min(4),
                LEFT_MARGIN + cell.column * PITCH,
                TOP_MARGIN + cell.row.min(6) * PITCH,
                html_text(&cell.title)
            ));
        }
        output.push_str("</svg></div>\n");
    }
    output.push_str(&format!("<p>{}</p>\n", html_text(&heatmap.legend)));
}

fn render_html_table(output: &mut String, table: &Table) {
    let class = |column: &Column| if column.numeric { " class=\"num\"" } else { "" };
    output.push_str("<div class=\"scroll\"><table>\n<thead><tr>");
    for column in &table.columns {
        output.push_str(&format!(
            "<th scope=\"col\"{}>{}</th>",
            class(column),
            html_text(&column.label)
        ));
    }
    output.push_str("</tr></thead>\n<tbody>\n");
    let cells = |output: &mut String, cells: &[String]| {
        output.push_str("<tr>");
        for (column, cell) in table.columns.iter().zip(cells) {
            output.push_str(&format!("<td{}>{}</td>", class(column), html_text(cell)));
        }
        output.push_str("</tr>\n");
    };
    for row in &table.rows {
        cells(output, row);
    }
    output.push_str("</tbody>\n");
    if let Some(total) = &table.total {
        output.push_str("<tfoot>\n");
        cells(output, total);
        output.push_str("</tfoot>\n");
    }
    output.push_str("</table></div>\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = "<script>&\"x\"|`a`*b*_c_[d](e)~f\\";

    fn hostile_document() -> Document {
        let mut table = Table::new(
            vec![Column::text(HOSTILE), Column::number("n")],
            vec![vec![HOSTILE.to_string(), "1".to_string()]],
        );
        table.total = Some(vec![HOSTILE.to_string(), String::new()]);
        Document {
            title: HOSTILE.to_string(),
            blocks: vec![
                Block::Section(HOSTILE.to_string()),
                Block::Facts(vec![(HOSTILE.to_string(), HOSTILE.to_string())]),
                Block::Table(table),
                Block::Paragraph(HOSTILE.to_string()),
                Block::List(vec![HOSTILE.to_string()]),
            ],
        }
    }

    #[test]
    fn markdown_escapes_every_syntax_character() {
        assert_eq!(
            "\\<script\\>\\&\"x\"\\|\\`a\\`\\*b\\*\\_c\\_\\[d\\](e)\\~f\\\\",
            escape_markdown(HOSTILE)
        );
        assert_eq!("plain text 1.5 -2", escape_markdown("plain text 1.5 -2"));
    }

    #[test]
    fn markdown_defuses_mentions_and_issue_references() {
        let zwsp = '\u{200B}';
        let escaped = escape_markdown("@scope/pkg #123 GH-42 a#b C# #tag @");
        assert_eq!(
            format!("@{zwsp}scope/pkg #{zwsp}123 GH-{zwsp}42 a#b C# #tag @{zwsp}"),
            escaped
        );
        // What GitHub would link is no longer contiguous text.
        assert!(
            !escaped.contains("@scope") && !escaped.contains("#123") && !escaped.contains("GH-42")
        );

        // Through the renderer, in every place a value can appear, but not in
        // the HTML page, which links nothing.
        let hostile = "@team #7";
        let document = Document {
            title: hostile.to_string(),
            blocks: vec![
                Block::Section(hostile.to_string()),
                Block::Facts(vec![(hostile.to_string(), hostile.to_string())]),
                Block::Paragraph(hostile.to_string()),
                Block::List(vec![hostile.to_string()]),
            ],
        };
        let markdown = render_markdown(&document);
        assert!(
            !markdown.contains("@team") && !markdown.contains("#7"),
            "{markdown}"
        );
        assert!(markdown.contains(&format!("@{zwsp}team #{zwsp}7")));
        assert!(render_html(&document).contains("@team #7"));
    }

    #[test]
    fn html_escapes_the_five_significant_characters() {
        assert_eq!(
            "&lt;script&gt;&amp;&quot;x&quot; &#39;y&#39;",
            escape_html("<script>&\"x\" 'y'")
        );
    }

    #[test]
    fn a_pipe_in_a_value_cannot_add_a_markdown_column() {
        let document = Document {
            title: "t".to_string(),
            blocks: vec![Block::Table(Table::new(
                vec![Column::text("a"), Column::text("b")],
                vec![vec!["left|right".to_string(), "x".to_string()]],
            ))],
        };
        let markdown = render_markdown(&document);
        let row = markdown.lines().find(|line| line.contains("left")).unwrap();
        assert_eq!("| left\\|right | x |", row);
    }

    #[test]
    fn a_newline_in_a_value_cannot_leave_its_table_row() {
        let document = Document {
            title: "t".to_string(),
            blocks: vec![Block::Facts(vec![(
                "a\n| injected | row |".to_string(),
                "v".to_string(),
            )])],
        };
        let markdown = render_markdown(&document);
        assert_eq!(
            1,
            markdown
                .lines()
                .filter(|line| line.contains("injected"))
                .count()
        );
        assert!(markdown.lines().all(|line| !line.starts_with("| injected")));
    }

    #[test]
    fn html_carries_no_tag_or_attribute_from_a_value() {
        let html = render_html(&hostile_document());
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("&lt;script&gt;&amp;&quot;x&quot;"));
    }

    #[test]
    fn html_is_self_contained() {
        let html = render_html(&hostile_document());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<style>"));
        for forbidden in [
            "http://", "https://", "<script", "<link", "<img", "@import", "url(",
        ] {
            assert!(!html.contains(forbidden), "found {forbidden}");
        }
        assert!(html.contains("prefers-color-scheme:dark"));
    }

    #[test]
    fn a_direction_override_is_replaced_in_both_formats() {
        let document = Document {
            title: "a\u{202e}b".to_string(),
            blocks: Vec::new(),
        };
        assert!(!render_markdown(&document).contains('\u{202e}'));
        assert!(!render_html(&document).contains('\u{202e}'));
    }

    #[test]
    fn an_empty_total_cell_is_not_bold() {
        let markdown = render_markdown(&hostile_document());
        let total = markdown
            .lines()
            .rev()
            .find(|line| line.starts_with("| **"))
            .unwrap();
        assert!(total.ends_with("|  |"), "{total}");
    }
}
