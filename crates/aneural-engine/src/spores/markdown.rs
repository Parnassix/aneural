//! Minimal markdown structure: frontmatter, title, level-2 sections, wiki-links.

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frontmatter {
    /// Scalar values.
    pub scalars: BTreeMap<String, String>,
    /// List values (`key:` followed by `- item` lines, or `key: [a, b]`).
    pub lists: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub heading: String,
    /// 1-based line of the heading.
    pub line: u32,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WikiLink {
    pub target: String,
    pub line: u32,
}

/// One row of a pipe table, keyed by column.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRow {
    /// Column key to cell text. Keys are slugged with `_`; see [`column_key`].
    pub cells: BTreeMap<String, String>,
    /// The same cells in document order, for `{col.1}`.
    pub positional: Vec<String>,
    /// 1-based row within its own table.
    pub row: u32,
    /// 1-based line in the file.
    pub line: u32,
}

/// A pipe table: its header keys, the heading it sits under, and its rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub headers: Vec<String>,
    /// The nearest heading above the table, so two tables in one document can
    /// be told apart by where they are rather than only by their columns.
    pub heading: String,
    pub rows: Vec<TableRow>,
}

impl Table {
    /// Does this table carry every one of these column keys?
    pub fn has_columns(&self, required: &[String]) -> bool {
        required.iter().all(|r| self.headers.contains(r))
    }
}

/// A cell longer than this is truncated. Props are served to agents over MCP,
/// and one real `notes` cell in the wild is 6 KB of prose, so this is a payload
/// bound rather than a cosmetic one.
pub const MAX_CELL_CHARS: usize = 2_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    pub frontmatter: Frontmatter,
    pub title: String,
    /// Body without frontmatter.
    pub body: String,
    /// Line offset of `body` within the file (0-based).
    pub body_offset: u32,
    pub sections: Vec<Section>,
    pub links: Vec<WikiLink>,
}

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]\|#]+)(?:#[^\]\|]*)?(?:\|[^\]]*)?\]\]").unwrap())
}

pub fn parse(text: &str, fallback_title: &str) -> Document {
    let (frontmatter, body, body_offset) = split_frontmatter(text);
    let mut doc = Document {
        frontmatter,
        body: body.to_string(),
        body_offset,
        ..Default::default()
    };

    // title: frontmatter > first H1 > fallback
    doc.title = doc
        .frontmatter
        .scalars
        .get("title")
        .cloned()
        .or_else(|| {
            body.lines()
                .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        })
        .unwrap_or_else(|| fallback_title.to_string());

    // level-2 sections
    let mut current: Option<Section> = None;
    let mut in_fence = false;
    for (i, line) in body.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && let Some(h) = line.strip_prefix("## ") {
            if let Some(s) = current.take() {
                doc.sections.push(finish(s));
            }
            current = Some(Section {
                heading: h.trim().to_string(),
                line: body_offset + i as u32 + 1,
                body: String::new(),
            });
            continue;
        }
        if let Some(s) = &mut current {
            s.body.push_str(line);
            s.body.push('\n');
        }
    }
    if let Some(s) = current.take() {
        doc.sections.push(finish(s));
    }

    // wiki links
    for (i, line) in body.lines().enumerate() {
        for cap in wikilink_re().captures_iter(line) {
            doc.links.push(WikiLink {
                // A trailing backslash is markdown escaping, not part of the
                // name: inside a table the alias separator has to be written
                // `\|` or the row breaks, and Obsidian writes it that way.
                // No filename ends in a backslash, so stripping is safe.
                target: cap[1].trim().trim_end_matches('\\').trim().to_string(),
                line: body_offset + i as u32 + 1,
            });
        }
    }
    doc
}

/// A header cell as a template key: slugged, but with `_` rather than `-`.
///
/// Forced, not chosen. A template var is matched by `[A-Za-z_][A-Za-z0-9_.]*`,
/// which admits `.` but not `-`, so `{col.last-refreshed}` would render as the
/// empty string and give no hint why.
pub fn column_key(header: &str) -> String {
    aneural_core::slug(header).replace('-', "_")
}

/// Every pipe table in a document.
///
/// A table is a header row, a `|---|` delimiter, then rows until something that
/// is not a row. Tables inside a fence are skipped, the same way headings are.
pub fn tables(doc: &Document) -> Vec<Table> {
    let lines: Vec<&str> = doc.body.lines().collect();
    let mut out: Vec<Table> = Vec::new();
    let mut heading = String::new();
    let mut in_fence = false;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            i += 1;
            continue;
        }
        if let Some(h) = line.trim_start().strip_prefix('#') {
            heading = h.trim_start_matches('#').trim().to_string();
            i += 1;
            continue;
        }
        // A table needs a header row and a delimiter under it.
        if !is_row(line) || i + 1 >= lines.len() || !is_delimiter(lines[i + 1]) {
            i += 1;
            continue;
        }
        let headers: Vec<String> = split_row(line).iter().map(|c| column_key(c)).collect();
        let mut table = Table {
            headers: headers.clone(),
            heading: heading.clone(),
            rows: Vec::new(),
        };
        i += 2;
        let mut n = 0;
        while i < lines.len() && is_row(lines[i]) {
            let mut positional: Vec<String> = split_row(lines[i]).iter().map(|c| cell(c)).collect();
            // A row written `… | |` has one more cell than the header does and
            // the extra is empty: that is padding, not a column. Only cells
            // *beyond* the header are dropped, so a table whose real last
            // column happens to be blank is left alone.
            while positional.len() > headers.len()
                && positional.last().is_some_and(|c| c.is_empty())
            {
                positional.pop();
            }
            let mut cells = BTreeMap::new();
            for (key, value) in headers.iter().zip(&positional) {
                // An unnamed column has no key to reach it by; it stays
                // positional rather than colliding on the empty string.
                if !key.is_empty() {
                    cells.insert(key.clone(), value.clone());
                }
            }
            n += 1;
            table.rows.push(TableRow {
                cells,
                positional,
                row: n,
                line: doc.body_offset + i as u32 + 1,
            });
            i += 1;
        }
        if !table.rows.is_empty() {
            out.push(table);
        }
    }
    out
}

fn is_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// `|---|:--:|---:|` — the line that makes the row above it a header.
fn is_delimiter(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim().trim_start_matches(':').trim_end_matches(':');
            !c.is_empty() && c.chars().all(|ch| ch == '-')
        })
}

/// Split a table line on the pipes that are actually separators.
///
/// Three things in real documents are not separators, and every one of them
/// appears in the vault this was written against:
///
/// - `\|`, the escaped separator, written inside a cell deliberately;
/// - a pipe inside a backtick span, as in `` `launchctl list | grep x` ``;
/// - a **bare** pipe inside `[[a|b]]`, which is the wiki-link alias separator.
///   Strictly that row is malformed markdown — the author should have escaped
///   it — but `[[…]]` makes the meaning unambiguous, and one such row in a
///   209-row table should not shear its columns and silently lose a cadence.
///
/// Splitting on every pipe puts the rest of the row into the wrong columns,
/// which is worse than dropping it: the cells still look plausible.
fn split_row(line: &str) -> Vec<String> {
    let mut body = line.trim();
    body = body.strip_prefix('|').unwrap_or(body);
    if body.ends_with('|') && !body.ends_with("\\|") {
        body = &body[..body.len() - 1];
    }

    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut ticks = false;
    let mut link = 0usize;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cur.push('|');
            }
            '`' => {
                ticks = !ticks;
                cur.push('`');
            }
            '[' if !ticks && chars.peek() == Some(&'[') => {
                chars.next();
                link += 1;
                cur.push_str("[[");
            }
            ']' if !ticks && chars.peek() == Some(&']') => {
                chars.next();
                link = link.saturating_sub(1);
                cur.push_str("]]");
            }
            '|' if !ticks && link == 0 => {
                cells.push(std::mem::take(&mut cur));
            }
            other => cur.push(other),
        }
    }
    cells.push(cur);
    cells
}

/// A cell's text: trimmed, unquoted if it is one backtick span, and capped.
fn cell(raw: &str) -> String {
    let t = raw.trim();
    // `` `aact` `` means the identifier aact. A prose cell that merely contains
    // backticks keeps them, so only a span covering the whole cell is stripped.
    let unquoted = match t.len() >= 2 && t.starts_with('`') && t.ends_with('`') {
        true => {
            let inner = &t[1..t.len() - 1];
            match inner.contains('`') {
                true => t,
                false => inner,
            }
        }
        false => t,
    };
    match unquoted.chars().count() > MAX_CELL_CHARS {
        true => unquoted.chars().take(MAX_CELL_CHARS).collect::<String>() + "…",
        false => unquoted.to_string(),
    }
}

/// The frontmatter alone, without walking the body.
///
/// For callers that only need the header of a file they are about to skip —
/// cheap enough to run over every markdown file in a workspace.
pub fn frontmatter(text: &str) -> Frontmatter {
    split_frontmatter(text).0
}

/// A frontmatter key read as a list however it was written.
///
/// `key: [a, b]` and a `- item` block both land in `lists`, but `key: a, b`
/// is a scalar as far as the parser is concerned, and real vaults write all
/// three. Values are trimmed and blanks dropped.
pub fn declared_list(fm: &Frontmatter, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = fm.lists.get(key) {
        out.extend(list.iter().cloned());
    }
    if let Some(one) = fm.scalars.get(key) {
        out.extend(one.split(',').map(|s| unquote(s.trim())));
    }
    out.retain(|v| !v.trim().is_empty());
    for v in &mut out {
        *v = v.trim().to_string();
    }
    out
}

/// Names this note answers to besides its filename.
///
/// Obsidian writes `aliases:` and accepts `alias:`, in any of the shapes
/// [`declared_list`] handles.
pub fn declared_aliases(text: &str) -> Vec<String> {
    let fm = frontmatter(text);
    let mut out = declared_list(&fm, "aliases");
    out.extend(declared_list(&fm, "alias"));
    out
}

fn finish(mut s: Section) -> Section {
    s.body = s.body.trim().to_string();
    s
}

/// Wiki links inside a section's line range.
pub fn links_in(doc: &Document, section: &Section, next_line: Option<u32>) -> Vec<WikiLink> {
    doc.links
        .iter()
        .filter(|l| l.line > section.line && next_line.is_none_or(|n| l.line < n))
        .cloned()
        .collect()
}

fn split_frontmatter(text: &str) -> (Frontmatter, &str, u32) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (Frontmatter::default(), text, 0);
    }
    let mut fm = Frontmatter::default();
    let mut current_list: Option<String> = None;
    let mut consumed = 1u32;
    for line in lines {
        consumed += 1;
        if line.trim() == "---" {
            let offset: usize = text
                .lines()
                .take(consumed as usize)
                .map(|l| l.len() + 1)
                .sum();
            let body = text.get(offset.min(text.len())..).unwrap_or("");
            return (fm, body, consumed);
        }
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if let Some(key) = &current_list {
                fm.lists
                    .entry(key.clone())
                    .or_default()
                    .push(unquote(item.trim()));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim();
            if value.is_empty() {
                current_list = Some(key.clone());
                fm.lists.entry(key).or_default();
            } else if value.starts_with('[') && value.ends_with(']') {
                let items = value[1..value.len() - 1]
                    .split(',')
                    .map(|s| unquote(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect();
                fm.lists.insert(key, items);
                current_list = None;
            } else {
                fm.scalars.insert(key, unquote(value));
                current_list = None;
            }
        }
    }
    // unterminated frontmatter: treat as body
    (Frontmatter::default(), text, 0)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') || t.starts_with('\'') && t.ends_with('\''))
        && t.len() >= 2
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header of the real `Refresh Cadences.md`, which is what this parser
    /// exists for.
    const CADENCES: &str = concat!(
        "# Refresh Cadences\n\n",
        "| source_id | cadence | cost | last_refreshed | drift_signal | notes |\n",
        "|---|---|---|---|---|---|\n",
        "| `aact` | daily | light | 2026-06-07 | If upstream `dataTimestamp` advances, we missed one. | CTTI exports each morning. |\n",
        "| `pubmed_updates` | daily | trivial | 2026-06-08 | slipped | cron `0 4 * * *` picks them up. |\n\n",
        "## Refresh shapes\n\n",
        "| source_id | refresh_shape | refresh_cost |\n",
        "|---|---|---|\n",
        "| `aact` | full-re-pull | heavy |\n",
    );

    #[test]
    fn a_table_row_becomes_one_row_keyed_by_column() {
        let tables = tables(&parse(CADENCES, "t"));
        assert_eq!(tables.len(), 2, "two tables, found {tables:?}");
        let cadence = &tables[0];
        assert_eq!(
            cadence.headers,
            vec![
                "source_id",
                "cadence",
                "cost",
                "last_refreshed",
                "drift_signal",
                "notes"
            ]
        );
        // The heading is how two tables in one document are told apart.
        assert_eq!(cadence.heading, "Refresh Cadences");
        assert_eq!(tables[1].heading, "Refresh shapes");
        assert_eq!(cadence.rows.len(), 2);

        let first = &cadence.rows[0];
        assert_eq!(first.row, 1);
        // a cell that is one backtick span is the identifier it names
        assert_eq!(first.cells["source_id"], "aact");
        assert_eq!(first.cells["cadence"], "daily");
        // a prose cell that merely contains backticks keeps them
        assert!(first.cells["drift_signal"].contains("`dataTimestamp`"));
        assert_eq!(first.positional.len(), 6);
    }

    #[test]
    fn column_keys_are_underscored_because_a_hyphen_is_not_a_template_var() {
        // `{col.last-refreshed}` would render as the empty string and say
        // nothing about why, so the key can never carry a hyphen.
        assert_eq!(column_key("last_refreshed"), "last_refreshed");
        assert_eq!(column_key("Refresh Shape"), "refresh_shape");
        assert_eq!(column_key("last-refreshed"), "last_refreshed");
        let re = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*$").unwrap();
        for header in ["source_id", "Refresh Shape", "drift signal"] {
            assert!(re.is_match(&column_key(header)), "{header}");
        }
    }

    #[test]
    fn a_pipe_that_is_not_a_separator_does_not_shear_the_row() {
        // Both shapes occur in the real file: an escaped pipe, and a pipe
        // inside a backtick span. Splitting on every pipe puts the rest of the
        // row into the wrong columns.
        let doc = parse(
            concat!(
                "| source_id | drift_signal | notes |\n",
                "|---|---|---|\n",
                "| `x` | check `launchctl list \\| grep com.acme` daily | {a\\|b} |\n",
            ),
            "t",
        );
        let t = &tables(&doc)[0];
        assert_eq!(t.rows.len(), 1);
        let row = &t.rows[0];
        assert_eq!(row.positional.len(), 3, "{:?}", row.positional);
        assert_eq!(row.cells["source_id"], "x");
        // the escape is removed: the cell means a literal pipe
        assert_eq!(
            row.cells["drift_signal"],
            "check `launchctl list | grep com.acme` daily"
        );
        assert_eq!(row.cells["notes"], "{a|b}");
    }

    #[test]
    fn an_enormous_cell_is_capped_rather_than_served_whole() {
        // One real `notes` cell is 6 KB of prose, and props reach agents
        // over MCP.
        let long = "z".repeat(MAX_CELL_CHARS * 2);
        let doc = parse(&format!("| a | b |\n|---|---|\n| x | {long} |\n"), "t");
        let cell = &tables(&doc)[0].rows[0].cells["b"];
        assert_eq!(
            cell.chars().count(),
            MAX_CELL_CHARS + 1,
            "capped plus ellipsis"
        );
        assert!(cell.ends_with('…'));
    }

    #[test]
    fn a_table_inside_a_fence_is_not_a_table() {
        let doc = parse(
            concat!(
                "# x\n\n```\n| not | a | table |\n|---|---|---|\n| a | b | c |\n```\n\n",
                "| real | table |\n|---|---|\n| a | b |\n",
            ),
            "t",
        );
        let found = tables(&doc);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].headers, vec!["real", "table"]);
    }

    #[test]
    fn a_header_without_a_delimiter_under_it_is_just_prose() {
        let doc = parse("| this looks like a row |\nbut nothing follows\n", "t");
        assert!(tables(&doc).is_empty());
    }

    #[test]
    fn a_bare_pipe_inside_a_wikilink_is_the_alias_separator_not_a_column() {
        // Verbatim shape from the vault: the author escaped the pipe in four
        // rows and forgot in two others. Splitting there shears the row and the
        // cells still look plausible, which is worse than dropping it.
        let doc = parse(
            concat!(
                "| source_id | drift_signal | notes |\n",
                "|---|---|---|\n",
                "| `swissmedic` | stalled | Aligns with ",
                "[[Refresh Cadences|MHRA + Health Canada MDALL]] in the Monday window. |\n",
            ),
            "t",
        );
        let row = &tables(&doc)[0].rows[0];
        assert_eq!(row.positional.len(), 3, "{:?}", row.positional);
        assert_eq!(row.cells["source_id"], "swissmedic");
        assert!(row.cells["notes"].ends_with("in the Monday window."));
        // and the link is still a link, so the row can be wired to that note
        assert!(doc.links.iter().any(|l| l.target == "Refresh Cadences"));
    }

    #[test]
    fn a_trailing_empty_cell_is_padding_not_a_column() {
        // Also from the vault: one row of 205 ends `… applies. | |`.
        let doc = parse("| a | b |\n|---|---|\n| x | y | |\n", "t");
        let row = &tables(&doc)[0].rows[0];
        assert_eq!(row.positional, vec!["x", "y"]);
        // but a table whose real last column is blank keeps it
        let doc = parse("| a | b |\n|---|---|\n| x |  |\n", "t");
        assert_eq!(tables(&doc)[0].rows[0].positional, vec!["x", ""]);
    }

    #[test]
    fn a_row_missing_a_separator_still_keeps_the_columns_it_does_have() {
        // One row of 205 genuinely merges two cells. Ragged rows are normal in
        // a hand-maintained table, so the row is read for what it does say
        // rather than dropped or shifted.
        let doc = parse(
            "| source_id | cadence | notes |\n|---|---|---|\n| `tga` | on-demand |\n",
            "t",
        );
        let row = &tables(&doc)[0].rows[0];
        assert_eq!(row.cells["source_id"], "tga");
        assert_eq!(row.cells["cadence"], "on-demand");
        assert!(
            !row.cells.contains_key("notes"),
            "absent, not empty-and-wrong"
        );
    }

    #[test]
    fn a_harvester_can_pick_the_table_it_means_by_the_columns_it_needs() {
        let found = tables(&parse(CADENCES, "t"));
        let cadence = ["source_id".to_string(), "cadence".to_string()];
        let shape = ["source_id".to_string(), "refresh_shape".to_string()];
        assert!(found[0].has_columns(&cadence));
        assert!(!found[0].has_columns(&shape));
        assert!(found[1].has_columns(&shape));
        assert!(!found[1].has_columns(&cadence));
        // no requirement at all matches every table
        assert!(found.iter().all(|t| t.has_columns(&[])));
    }

    #[test]
    fn a_link_inside_a_table_has_its_escaped_pipe_stripped() {
        // In a table row the separator must be escaped or the row breaks, so
        // this is how the majority of cross-folder links in a real vault are
        // written. Keeping the backslash makes the target unresolvable.
        let doc = parse(
            "| doc | note |\n|---|---|\n| a | [[../FDA/Warning Letters\\|Warning Letters]] |\n",
            "t",
        );
        assert_eq!(doc.links.len(), 1);
        assert_eq!(doc.links[0].target, "../FDA/Warning Letters");
    }

    #[test]
    fn aliases_are_read_in_all_four_shapes_obsidian_writes() {
        let list = "---\naliases:\n  - ADR\n  - \"Decision Log\"\n---\n# x\n";
        assert_eq!(declared_aliases(list), vec!["ADR", "Decision Log"]);

        let inline = "---\naliases: [ADR, Decision Log]\n---\n";
        assert_eq!(declared_aliases(inline), vec!["ADR", "Decision Log"]);

        let scalar = "---\nalias: ADR\n---\n";
        assert_eq!(declared_aliases(scalar), vec!["ADR"]);

        // An empty `aliases:` key is a list with nothing in it, not an alias.
        assert!(declared_aliases("---\naliases:\n---\n").is_empty());
        assert!(declared_aliases("# no frontmatter at all\n").is_empty());
    }

    const DOC: &str = "---\ntitle: Focus loader\nstatus: in-progress\ntargets:\n  - apps/web/src/index.ts\n  - services/api/api/main.py\ntags: [a, \"b\"]\n---\n\n# Ignored H1\n\nIntro [[Architecture]].\n\n## First idea\nBody with [[README|alias]] and [[Notes#sec]].\n\n## Second\n```\n## not a heading\n```\ntext\n";

    #[test]
    fn parses_frontmatter_sections_links() {
        let d = parse(DOC, "fallback");
        assert_eq!(d.title, "Focus loader");
        assert_eq!(d.frontmatter.scalars["status"], "in-progress");
        assert_eq!(
            d.frontmatter.lists["targets"],
            vec!["apps/web/src/index.ts", "services/api/api/main.py"]
        );
        assert_eq!(d.frontmatter.lists["tags"], vec!["a", "b"]);
        assert_eq!(d.sections.len(), 2);
        assert_eq!(d.sections[0].heading, "First idea");
        assert_eq!(d.sections[0].line, 14);
        assert_eq!(d.sections[1].body, "```\n## not a heading\n```\ntext");
        let targets: Vec<_> = d.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["Architecture", "README", "Notes"]);
        let first = links_in(&d, &d.sections[0], Some(d.sections[1].line));
        assert_eq!(first.len(), 2);
    }

    #[test]
    fn no_frontmatter_uses_h1_then_fallback() {
        let d = parse("# Hello\n\ntext", "x.md");
        assert_eq!(d.title, "Hello");
        assert_eq!(d.body_offset, 0);
        assert_eq!(parse("just text", "x").title, "x");
    }
}
