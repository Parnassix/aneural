//! Plan documents, and what they said they would touch.
//!
//! Claude Code keeps every approved plan in `~/.claude/plans/<slug>.md` and
//! never updates it afterwards. There is no `targets:` frontmatter the way a
//! workspace plan under `.aneural/plans` has — a Claude plan is prose, and the
//! files it names are named in the middle of sentences:
//!
//! ```text
//! Promote it to `crates/aneural-gui/src/markdown.rs`, keeping the marketplace
//! using it, and shown before `available_rect_before_wrap()` at `ui.rs:607`.
//! ```
//!
//! So the paths are pulled out by shape and then resolved against the graph,
//! and anything that does not resolve is dropped. This is a heuristic where the
//! frontmatter list was a fact, and the edges say so: they carry
//! `via: "mentioned"` against the spore's `via: "frontmatter"`.

use crate::spores::markdown;
use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Every plan hangs off this one origin: they are read together, and which
/// plans exist at all depends on which sessions named them.
pub const ORIGIN: &str = "claude://plans";

/// A plan as read off disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// File name within the plans directory, which is what a session names.
    pub name: String,
    pub title: String,
    pub body: String,
    /// Absolute path, for opening it.
    pub path: PathBuf,
    pub mentions: Vec<Mention>,
    /// The plan's own headings, in document order. These are its steps.
    pub steps: Vec<Heading>,
}

/// A heading in a plan, which is what a reader walks the plan by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub text: String,
    /// 2 for `##`, 3 for `###`.
    pub level: u8,
    /// 1-based line within the document.
    pub line: u32,
}

/// A path-shaped run of text found in a plan's prose.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mention {
    /// The path as written, with any `:line` suffix removed.
    pub path: String,
    /// 1-based line within the document.
    pub line: u32,
}

/// What is known about a plan once the graph is consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Named by a session, but nothing has come of it yet.
    Proposed,
    /// A session is still working, or the files it touched are not committed.
    InProgress,
    /// Commits carried it out.
    Built,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Proposed => "proposed",
            State::InProgress => "in progress",
            State::Built => "built",
        }
    }
}

/// The directory Claude Code keeps plans in.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("plans")
}

/// Read one plan by the file name a session recorded.
///
/// Only plans a session named are read. `plans/` is flat and global — it holds
/// plans from every project on the machine — so being referenced by a
/// transcript of *this* workspace is the only thing that scopes it.
pub fn read(state_dir: &Path, name: &str) -> Option<Plan> {
    // The name comes out of a transcript, so it is not ours to trust: keep the
    // file name and nothing else, or `../../.ssh/id_rsa` would be a plan.
    let name = Path::new(name).file_name()?.to_str()?.to_string();
    if !name.ends_with(".md") {
        return None;
    }
    let path = dir(state_dir).join(&name);
    let text = std::fs::read_to_string(&path).ok()?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&name);
    let doc = markdown::parse(&text, stem);
    Some(Plan {
        mentions: mentions(&doc.body, doc.body_offset),
        steps: headings(&doc.body, doc.body_offset),
        title: doc.title,
        body: doc.body,
        path,
        name,
    })
}

/// Every `##` and `###` heading, in document order.
///
/// A second heading reader next to [`markdown::parse`] rather than an extension
/// of it: that one is level-2 by contract and the plans spore depends on the
/// sections it returns being chapters. A plan's steps are finer than its
/// chapters — this plan's work is `A.` through `E.` under one `## Build`.
pub fn headings(body: &str, offset: u32) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (i, line) in body.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Deepest first, or `### x` would be read as a level-2 `# x`.
        let (level, text) = match line.strip_prefix("### ") {
            Some(t) => (3, t),
            None => match line.strip_prefix("## ") {
                Some(t) => (2, t),
                None => continue,
            },
        };
        out.push(Heading {
            text: text.trim().to_string(),
            level,
            line: offset + i as u32 + 1,
        });
    }
    out
}

/// The step a line of the document belongs to: the last heading at or above it.
/// `None` for the preamble, which belongs to the plan rather than to any step.
fn step_of(steps: &[Heading], line: u32) -> Option<usize> {
    steps.partition_point(|h| h.line <= line).checked_sub(1)
}

/// Extensions that make a bare word a path even without a slash in it, so that
/// `ui.rs:607` is found but `e.g.` and `0.87.1` are not.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "php", "rb", "md", "json",
    "toml", "yaml", "yml", "sql", "sh", "html", "css", "scm", "lock",
];

fn path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // A path-ish run ending in an extension, optionally followed by the
        // `:12` or `:12-34` line reference this house style uses. The leading
        // dot is allowed so `.github/workflows/ci.yml` keeps it; the damage
        // that would do to `e.g.` is undone by `looks_like_a_path`.
        Regex::new(r"[A-Za-z0-9_.][A-Za-z0-9_.\-/]*\.[A-Za-z0-9]+(?::\d+(?:-\d+)?)?").unwrap()
    })
}

/// Every path-shaped reference in a plan's prose, in order, deduplicated.
///
/// `offset` is the body's 0-based line offset within the file, so the lines
/// reported are lines of the document rather than of the body.
pub fn mentions(body: &str, offset: u32) -> Vec<Mention> {
    let mut out: Vec<Mention> = Vec::new();
    for (i, line) in body.lines().enumerate() {
        for m in path_re().find_iter(line) {
            let raw = m.as_str().trim_end_matches(['.', ',', ')', ':']);
            let path = raw.split(':').next().unwrap_or(raw);
            if !looks_like_a_path(path) {
                continue;
            }
            if out.iter().any(|existing| existing.path == path) {
                continue;
            }
            out.push(Mention {
                path: path.to_string(),
                line: offset + i as u32 + 1,
            });
        }
    }
    out
}

fn looks_like_a_path(candidate: &str) -> bool {
    let Some((_, ext)) = candidate.rsplit_once('.') else {
        return false;
    };
    // A slash is proof enough. Without one, the extension has to carry it.
    if candidate.contains('/') {
        return !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric());
    }
    CODE_EXTENSIONS.contains(&ext)
}

/// Turn a plan into its node and edges.
///
/// `resolve` maps a mentioned path to a workspace-relative one, returning
/// `None` when nothing in the graph matches — most references in a plan are to
/// files, but plenty are to files in another repository, or ones that were
/// never created. `realized_by` is the sessions that approved this plan.
pub fn to_graph(
    plan: &Plan,
    state: State,
    approved: &str,
    realized_by: &[NodeId],
    used: &crate::history::claude::ByModel,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> (Node, Vec<Edge>) {
    let id = NodeId::home_plan(&plan.name);
    let mut edges = Vec::new();
    // Distinct files, not resolved mentions: two ways of writing the same path
    // resolve to one file and therefore to one edge, and a count that did not
    // agree with the edges would be a count nobody could check.
    let mut named: std::collections::BTreeSet<String> = Default::default();

    for mention in &plan.mentions {
        let Some(rel) = resolve(&mention.path) else {
            continue;
        };
        if !named.insert(rel.clone()) {
            continue;
        }
        let mut edge = Edge::new(
            EdgeKind::ANNOTATES,
            id.clone(),
            NodeId::file(&rel),
            Source::CLAUDE,
        )
        .with_origin(ORIGIN)
        // Not `frontmatter`: this was read out of a sentence, and the
        // difference is worth keeping where it can be seen.
        .with_prop("via", "mentioned")
        .with_prop("line", mention.line as i64);
        // The step that introduced the file. A file named twice is one edge —
        // identity is (kind, src, dst, source), so a second is not
        // representable — and the first mention is the one that argued for it.
        if let Some(step) = step_of(&plan.steps, mention.line) {
            edge = edge
                .with_prop("step", step as i64)
                .with_prop("section", plan.steps[step].text.as_str());
        }
        edges.push(edge);
    }

    for session in realized_by {
        edges.push(
            Edge::new(
                EdgeKind::REALIZES,
                session.clone(),
                id.clone(),
                Source::CLAUDE,
            )
            .with_origin(ORIGIN)
            .with_prop("via", "plan-mode"),
        );
    }

    let mut node = Node::new(id, NodeKind::PLAN, &plan.title, Source::CLAUDE)
        .with_origin(ORIGIN)
        .with_prop("file", plan.path.display().to_string())
        .with_prop("state", state.as_str())
        .with_prop("names", plan.mentions.len() as i64)
        .with_prop("namesInGraph", named.len() as i64)
        // Every step, including the ones that name no file at all: "prompts
        // into session props" is a step of this very plan and names nothing,
        // and a walk that skipped it would be lying about the shape of the work.
        .with_prop(
            "steps",
            serde_json::Value::Array(
                plan.steps
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "heading": h.text,
                            "level": h.level,
                            "line": h.line,
                        })
                    })
                    .collect(),
            ),
        );
    if !approved.is_empty() {
        node = node.with_prop("approvedAt", approved);
    }
    // What carrying it out cost, by model: the stretch of transcript between
    // this plan being approved and the next one, across every session that
    // picked it up. No props at all for a plan that was approved and then
    // abandoned before a single message ran under it.
    node = crate::history::claude::write_usage(node, used);
    (node, edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(body: &str) -> Vec<String> {
        mentions(body, 0).into_iter().map(|m| m.path).collect()
    }

    #[test]
    fn paths_are_pulled_out_of_prose_however_they_are_written() {
        let body = "Promote `readme_body` to `crates/aneural-gui/src/markdown.rs`, shown\n\
                    before `available_rect_before_wrap()` at `ui.rs:607`, and see\n\
                    docs/graph-schema.md plus App.tsx:514-572 for the rest.";
        assert_eq!(
            found(body),
            vec![
                "crates/aneural-gui/src/markdown.rs",
                "ui.rs",
                "docs/graph-schema.md",
                "App.tsx",
            ]
        );
    }

    #[test]
    fn line_numbers_are_of_the_document_not_the_body() {
        let m = mentions("first\nsecond `src/a.ts` here", 4);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 6, "4 lines of frontmatter, then body line 2");
    }

    /// The failure mode that makes naive extraction useless: English is full of
    /// things shaped like filenames.
    #[test]
    fn prose_that_merely_looks_like_a_path_is_left_alone() {
        for body in [
            "for example, e.g. this and i.e. that",
            "pinned at 0.87.1 and bumped from 0.84",
            "see Section 3.2 for why",
            "the U.S. version",
        ] {
            assert!(found(body).is_empty(), "matched something in: {body}");
        }
    }

    #[test]
    fn a_reference_named_twice_is_listed_once() {
        let body = "`src/a.ts` is imported by `src/b.ts`, and `src/a.ts` re-exports it.";
        assert_eq!(found(body), vec!["src/a.ts", "src/b.ts"]);
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_path() {
        assert_eq!(found("Change `src/a.ts`."), vec!["src/a.ts"]);
        assert_eq!(found("Change src/a.ts, then stop."), vec!["src/a.ts"]);
        assert_eq!(found("(see src/a.ts)"), vec!["src/a.ts"]);
    }

    #[test]
    fn a_slashed_path_is_taken_whatever_its_extension() {
        assert_eq!(
            found("see .github/workflows/ci.yml"),
            vec![".github/workflows/ci.yml"]
        );
        assert_eq!(
            found("schema/spore-v2.json is generated"),
            vec!["schema/spore-v2.json"]
        );
    }

    fn walk(body: &str) -> Vec<(u8, String, u32)> {
        headings(body, 0)
            .into_iter()
            .map(|h| (h.level, h.text, h.line))
            .collect()
    }

    #[test]
    fn the_steps_of_a_plan_are_its_own_headings() {
        let body = "# Title\n\nPreamble.\n\n## Build\n\n### A. git\n\ntext\n\n## Risks\n";
        assert_eq!(
            walk(body),
            vec![
                (2, "Build".to_string(), 5),
                (3, "A. git".to_string(), 7),
                (2, "Risks".to_string(), 11),
            ],
            "the H1 is the plan's title, not a step within it"
        );
    }

    /// A shell transcript in a fenced block is full of `#` comments, and none
    /// of them are steps.
    #[test]
    fn headings_inside_a_code_fence_are_not_steps() {
        let body = "## Real\n\n```sh\n## not a heading\n### nor this\n```\n\n## Also real\n";
        assert_eq!(
            walk(body),
            vec![(2, "Real".to_string(), 1), (2, "Also real".to_string(), 8)]
        );
    }

    #[test]
    fn a_line_belongs_to_the_last_heading_above_it() {
        let steps = headings("## One\n\ntext\n\n## Two\n\ntext\n", 0);
        assert_eq!(step_of(&steps, 1), Some(0), "the heading line itself");
        assert_eq!(step_of(&steps, 3), Some(0));
        assert_eq!(step_of(&steps, 7), Some(1));
        assert_eq!(
            step_of(&steps, 0),
            None,
            "a preamble belongs to the plan, not to a step"
        );
        assert_eq!(step_of(&[], 9), None);
    }

    #[test]
    fn a_mention_above_every_heading_is_filed_under_no_step() {
        let plan = Plan {
            name: "p.md".into(),
            title: "P".into(),
            body: String::new(),
            path: "/p.md".into(),
            steps: vec![Heading {
                text: "Build".into(),
                level: 2,
                line: 10,
            }],
            mentions: vec![Mention {
                path: "src/a.ts".into(),
                line: 2,
            }],
        };
        let (_, edges) = to_graph(&plan, State::Proposed, "", &[], &Default::default(), &|p| {
            Some(p.to_string())
        });
        assert!(edges[0].props.get("step").is_none());
        assert!(edges[0].props.get("section").is_none());
    }

    #[test]
    fn a_plan_reads_its_steps_off_disk_with_document_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(tmp.path())).unwrap();
        std::fs::write(
            dir(tmp.path()).join("p.md"),
            "# Title\n\n## Build\n\nRewrite `src/a.ts`.\n\n## Risks\n\nNone.\n",
        )
        .unwrap();

        let plan = read(tmp.path(), "p.md").unwrap();
        assert_eq!(
            plan.steps,
            vec![
                Heading {
                    text: "Build".into(),
                    level: 2,
                    line: 3
                },
                Heading {
                    text: "Risks".into(),
                    level: 2,
                    line: 7
                },
            ]
        );
        assert_eq!(step_of(&plan.steps, plan.mentions[0].line), Some(0));
    }

    #[test]
    fn a_plan_becomes_a_node_with_edges_only_to_files_we_have() {
        let plan = Plan {
            name: "wise-phoenix.md".into(),
            title: "Platform skeleton".into(),
            body: String::new(),
            path: "/home/.claude/plans/wise-phoenix.md".into(),
            steps: vec![Heading {
                text: "Build".into(),
                level: 2,
                line: 2,
            }],
            mentions: vec![
                Mention {
                    path: "src/a.ts".into(),
                    line: 3,
                },
                Mention {
                    path: "somewhere/else.ts".into(),
                    line: 9,
                },
            ],
        };
        let session = NodeId::session("uuid-1");
        let (node, edges) = to_graph(
            &plan,
            State::Built,
            "2026-09-15T00:00:00Z",
            std::slice::from_ref(&session),
            &Default::default(),
            &|p| (p == "src/a.ts").then(|| p.to_string()),
        );

        assert_eq!(node.id, NodeId::home_plan("wise-phoenix.md"));
        assert_eq!(node.kind, NodeKind::PLAN);
        assert_eq!(node.prop_str("state"), Some("built"));
        assert_eq!(node.props.get("names").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(
            node.props.get("namesInGraph").and_then(|v| v.as_i64()),
            Some(1),
            "what it named, against what we can point at"
        );

        let annotates: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ANNOTATES)
            .collect();
        assert_eq!(annotates.len(), 1);
        assert_eq!(annotates[0].dst, NodeId::file("src/a.ts"));
        assert_eq!(annotates[0].props.get("via").unwrap(), "mentioned");
        assert_eq!(annotates[0].props.get("step").unwrap(), 0);
        assert_eq!(annotates[0].props.get("section").unwrap(), "Build");
        assert_eq!(
            node.props.get("steps").unwrap(),
            &serde_json::json!([{"heading": "Build", "level": 2, "line": 2}]),
            "the walk is the plan's own headings, carried on the node"
        );

        let realizes: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::REALIZES)
            .collect();
        assert_eq!(realizes.len(), 1);
        assert_eq!(realizes[0].src, session);
        assert_eq!(realizes[0].dst, node.id);
    }

    #[test]
    fn a_plan_name_out_of_a_transcript_cannot_walk_out_of_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(tmp.path())).unwrap();
        std::fs::write(tmp.path().join("secret.md"), "# not a plan").unwrap();
        std::fs::write(dir(tmp.path()).join("real.md"), "# A plan").unwrap();

        assert!(read(tmp.path(), "../secret.md").is_none());
        assert!(read(tmp.path(), "/etc/passwd").is_none());
        assert!(read(tmp.path(), "real.txt").is_none());
        assert_eq!(read(tmp.path(), "real.md").unwrap().title, "A plan");
    }

    #[test]
    fn a_plan_reads_its_title_and_mentions_off_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(tmp.path())).unwrap();
        std::fs::write(
            dir(tmp.path()).join("p.md"),
            "# Grow the graph\n\nRewrite `crates/aneural-gui/src/layout.rs` entirely.\n",
        )
        .unwrap();

        let plan = read(tmp.path(), "p.md").unwrap();
        assert_eq!(plan.title, "Grow the graph");
        assert_eq!(plan.name, "p.md");
        assert_eq!(
            plan.mentions,
            vec![Mention {
                path: "crates/aneural-gui/src/layout.rs".into(),
                line: 3
            }]
        );
    }
}
