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
        }
    }
    output
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
:root{color-scheme:light dark;--bg:#fff;--fg:#1f2328;--muted:#59636e;--line:#d1d9e0;--head:#f6f8fa;--accent:#0969da}\
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--line:#3d444d;--head:#151b23;--accent:#4493f8}}\
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
ul{padding-left:1.4rem}";

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
        }
    }
    output.push_str("</main>\n</body>\n</html>\n");
    output
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
