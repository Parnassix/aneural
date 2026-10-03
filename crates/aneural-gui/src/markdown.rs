//! Just enough markdown to read a document in a panel.
//!
//! Deliberately not a markdown library. The GUI renders two kinds of document —
//! a spore's README and a plan — and both are plain prose with headings, lists
//! and code. A parser would be a new dependency, a new build, and a new set of
//! ways for a malformed line to panic mid-frame; this is a line loop.
//!
//! The one thing it does that a general renderer would not: a run of text
//! shaped like a file path becomes a link, because a plan is mostly a list of
//! files and the point of reading one here is to go and look at them.

use crate::theme;
use bevy_egui::egui;

/// What the reader did while the document was on screen.
#[derive(Default)]
pub struct Clicked {
    /// A path-shaped run the reader followed, exactly as written.
    pub path: Option<String>,
}

/// Render `text`. `linkable` decides which path-shaped runs become links —
/// the panel passes one that answers "is this a file I actually have", so a
/// plan's references to another repository read as plain text rather than as
/// links that go nowhere.
pub fn body(
    ui: &mut egui::Ui,
    text: &str,
    palette: &theme::Palette,
    linkable: &dyn Fn(&str) -> bool,
) -> Clicked {
    let mut out = Clicked::default();
    let mut in_code = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            ui.label(egui::RichText::new(line).monospace().small());
            continue;
        }
        if line.trim().is_empty() {
            ui.add_space(4.0);
            continue;
        }
        if let Some(h) = line.strip_prefix("### ") {
            ui.label(egui::RichText::new(h).strong());
        } else if let Some(h) = line.strip_prefix("## ") {
            ui.label(egui::RichText::new(h).strong().size(14.0));
        } else if let Some(h) = line.strip_prefix("# ") {
            ui.label(egui::RichText::new(h).strong().size(15.0));
        } else if let Some(item) = bullet(line) {
            inline(ui, &format!("• {item}"), palette, linkable, &mut out);
        } else {
            inline(ui, line, palette, linkable, &mut out);
        }
    }
    out
}

/// The lines of `text` from `from` up to but not including `to`, both 1-based
/// document lines. Used to show one section of a plan on its own.
///
/// An out-of-range start is an empty slice rather than a panic: the line came
/// from an index built against a file that may since have been edited, and a
/// stale line number should cost a blank panel, not the frame.
pub fn slice(text: &str, from: u32, to: Option<u32>) -> String {
    let from = from.saturating_sub(1) as usize;
    let to = to.map_or(usize::MAX, |t| t.saturating_sub(1) as usize);
    if to <= from {
        return String::new();
    }
    text.lines()
        .skip(from)
        .take(to - from)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of a `- `, `* ` or `1. ` list item.
fn bullet(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest);
        }
    }
    // `12. item`, but not `1.5 of them`
    let digits = trimmed.find(|c: char| !c.is_ascii_digit())?;
    (digits > 0 && trimmed[digits..].starts_with(". ")).then(|| &trimmed[digits + 2..])
}

/// One line, with backticked spans set in monospace and paths made clickable.
///
/// Laid out piece by piece rather than as one styled job, because egui can only
/// make a whole widget clickable — a link inside a paragraph has to be its own
/// widget, and `horizontal_wrapped` is what keeps the pieces reading as a
/// sentence rather than a column.
fn inline(
    ui: &mut egui::Ui,
    line: &str,
    palette: &theme::Palette,
    linkable: &dyn Fn(&str) -> bool,
    out: &mut Clicked,
) {
    if !line.contains('`') && !looks_pathy(line) {
        ui.add(egui::Label::new(line).wrap());
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for piece in split(line) {
            match piece {
                Piece::Text(t) => {
                    ui.add(egui::Label::new(t).wrap());
                }
                Piece::Code(t) => {
                    let path = t.split(':').next().unwrap_or(t);
                    if linkable(path) {
                        let link = ui.link(
                            egui::RichText::new(t)
                                .monospace()
                                .color(theme::egui_color(palette.accent)),
                        );
                        if link.clicked() {
                            out.path = Some(path.to_string());
                        }
                    } else {
                        ui.label(egui::RichText::new(t).monospace());
                    }
                }
            }
        }
    });
}

enum Piece<'a> {
    Text(&'a str),
    Code(&'a str),
}

/// Split a line on backticks. An unclosed backtick is text, not a code span:
/// prose about a shell command should not swallow the rest of the paragraph.
fn split(line: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        out.push(Piece::Code(&after[..close]));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    out
}

/// A cheap pre-test so an ordinary sentence never goes through the splitter.
fn looks_pathy(line: &str) -> bool {
    line.contains('/') && line.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(line: &str) -> Vec<String> {
        split(line)
            .into_iter()
            .map(|p| match p {
                Piece::Text(t) => format!("T:{t}"),
                Piece::Code(t) => format!("C:{t}"),
            })
            .collect()
    }

    #[test]
    fn backticked_spans_come_out_as_code() {
        assert_eq!(
            pieces("Rewrite `src/a.ts` now"),
            vec!["T:Rewrite ", "C:src/a.ts", "T: now"]
        );
        assert_eq!(pieces("`only.rs`"), vec!["C:only.rs"]);
        assert_eq!(pieces("nothing here"), vec!["T:nothing here"]);
    }

    /// A single backtick in prose must not swallow the rest of the line.
    #[test]
    fn an_unclosed_backtick_stays_text() {
        assert_eq!(
            pieces("use ` to quote, like this"),
            vec!["T:use ` to quote, like this"]
        );
    }

    #[test]
    fn list_markers_are_recognised_without_eating_decimals() {
        assert_eq!(bullet("- one"), Some("one"));
        assert_eq!(bullet("  * two"), Some("two"));
        assert_eq!(bullet("3. three"), Some("three"));
        assert_eq!(bullet("12. twelve"), Some("twelve"));
        assert_eq!(bullet("1.5 of them"), None);
        assert_eq!(bullet("plain text"), None);
        assert_eq!(bullet("-no space"), None);
    }

    #[test]
    fn a_section_can_be_cut_out_of_a_document_by_line() {
        let doc = "# Title\n\n## One\n\nfirst\n\n## Two\n\nsecond\n";
        assert_eq!(slice(doc, 3, Some(7)), "## One\n\nfirst\n");
        assert_eq!(slice(doc, 7, None), "## Two\n\nsecond");
        assert_eq!(
            slice(doc, 99, None),
            "",
            "a stale line number costs a blank"
        );
        assert_eq!(slice(doc, 7, Some(7)), "");
    }

    #[test]
    fn the_pathy_pre_test_skips_ordinary_prose() {
        assert!(looks_pathy("see crates/aneural-gui/src/ui.rs"));
        assert!(!looks_pathy("a sentence with a full stop."));
        assert!(!looks_pathy("and/or"));
    }
}
