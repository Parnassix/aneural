//! What a workspace can run, and what it is declared to run on.
//!
//! `cargo run -p aneural-engine --example scripts -- <dir>`
//!
//! Indexes the workspace the way the app does and prints the Script and
//! Schedule nodes the `scripts` spore found, the runs it can see evidence of,
//! and then checks the claim the graph makes about itself: every schedule that
//! names a script should reach one.
//!
//! **Read-only.** It never runs a script and never writes to the workspace, so
//! it is safe to point at any directory.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{Node, NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_store::{EdgeQuery, NodeQuery};
use std::collections::BTreeMap;

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".into())
        .parse::<std::path::PathBuf>()
        .unwrap()
        .canonicalize()
        .expect("no such directory");
    let ws = Workspace::at(&root);
    let mut engine = Engine::open_in_memory(ws).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();

    let of = |kind: &str| -> Vec<Node> {
        let mut v = engine
            .store()
            .query_nodes(&NodeQuery {
                kinds: vec![kind.into()],
                ..Default::default()
            })
            .unwrap();
        v.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        v
    };
    let scripts = of(NodeKind::SCRIPT);
    let schedules = of(NodeKind::SCHEDULE);

    println!("\n{}", root.display());
    if scripts.is_empty() && schedules.is_empty() {
        println!(
            "\n  nothing found. The `scripts` spore is off by default — enable it with\n  \
             `aneural spores enable aneural.scripts`, or from the marketplace."
        );
        return;
    }

    // ---- what can be run, by the directory it lives in --------------------
    println!("\n\x1b[1mScripts\x1b[0m  ({})", scripts.len());
    let mut by_dir: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    for s in &scripts {
        let path = s.prop_str("file").unwrap_or_else(|| s.id.path_part());
        by_dir.entry(dir_of(path)).or_default().push(s);
    }
    for (dir, mut here) in by_dir {
        here.sort_by_key(|n| n.label.as_str());
        println!("\n  \x1b[2m{dir}/\x1b[0m  ({})", here.len());
        for s in here.iter().take(6) {
            println!(
                "    {:<44} {:<9} {}",
                s.label.chars().take(43).collect::<String>(),
                s.prop_str("found").unwrap_or(""),
                s.prop_str("interpreter").unwrap_or(""),
            );
        }
        if here.len() > 6 {
            println!("    \x1b[2m… and {} more\x1b[0m", here.len() - 6);
        }
    }

    // ---- what they are declared to run on ---------------------------------
    // Which script each schedule reaches, read off the edges rather than
    // guessed from the name: the declaration and the file rarely agree.
    let exists: std::collections::HashSet<&NodeId> = scripts.iter().map(|n| &n.id).collect();
    let mut points_at: BTreeMap<&NodeId, Vec<NodeId>> = BTreeMap::new();
    for edge in engine
        .store()
        .get_edges(&EdgeQuery {
            kinds: vec![EdgeKind::ANNOTATES.into()],
            ..Default::default()
        })
        .unwrap()
    {
        if edge.dst.prefix() == "script"
            && let Some(s) = schedules.iter().find(|s| s.id == edge.src)
        {
            points_at.entry(&s.id).or_default().push(edge.dst.clone());
        }
    }

    // The verdicts are this producer's own nodes, keyed to the declaration they
    // judge; the declaration itself belongs to the spore that read it.
    let declared: Vec<&Node> = schedules
        .iter()
        .filter(|n| !matches!(n.prop_str("declaredBy"), Some("verdict" | "installed")))
        .collect();
    let mut verdict_of: BTreeMap<&str, &Node> = BTreeMap::new();
    for n in schedules
        .iter()
        .filter(|n| n.prop_str("declaredBy") == Some("verdict"))
    {
        verdict_of.insert(n.label.as_str(), n);
    }
    let installed: Vec<&Node> = schedules
        .iter()
        .filter(|n| n.prop_str("declaredBy") == Some("installed"))
        .collect();

    if !installed.is_empty() {
        println!(
            "\n\x1b[1mInstalled on this machine\x1b[0m  ({})",
            installed.len()
        );
        for i in &installed {
            println!(
                "  {:<46} {:<22} {}",
                i.label.chars().take(45).collect::<String>(),
                i.prop_str("when").unwrap_or(""),
                match i.props.get("loaded").and_then(|v| v.as_bool()) {
                    Some(true) => "\x1b[32mloaded\x1b[0m".to_string(),
                    _ => "\x1b[33mnot loaded\x1b[0m".to_string(),
                }
            );
        }
    }

    println!("\n\x1b[1mSchedules\x1b[0m  ({})", declared.len());
    let mut reaching = 0;
    let mut dangling = Vec::new();
    let mut wants_attention = 0;
    let mut drifting = 0;
    for s in &declared {
        let targets = points_at.get(&s.id).cloned().unwrap_or_default();
        let hit = targets.iter().any(|t| exists.contains(t));
        if !targets.is_empty() {
            match hit {
                true => reaching += 1,
                false => dangling.push((s.label.clone(), targets[0].clone())),
            }
        }
        let verdict = verdict_of.get(s.label.as_str());
        let due = verdict.and_then(|v| v.prop_str("due")).unwrap_or("");
        if matches!(due, "due" | "overdue") {
            wants_attention += 1;
        }
        if verdict.and_then(|v| v.prop_str("drift")).is_some() {
            drifting += 1;
        }
        println!(
            "  {:<30} {:<14} {:<10} {:<12} {}",
            s.label.chars().take(29).collect::<String>(),
            s.prop_str("cadence")
                .or_else(|| s.prop_str("cron"))
                .unwrap_or("—"),
            match due {
                "overdue" => "\x1b[31moverdue\x1b[0m".to_string(),
                "due" => "\x1b[33mdue\x1b[0m".to_string(),
                "" => "\x1b[2m—\x1b[0m".to_string(),
                other => format!("\x1b[2m{other}\x1b[0m"),
            },
            // The date, not the commentary that follows it in the cell.
            s.prop_str("lastRefreshed")
                .unwrap_or("—")
                .split_whitespace()
                .next()
                .unwrap_or("—"),
            match (targets.first(), hit) {
                (Some(t), true) => format!("→ {}", t.path_part()),
                (Some(t), false) => format!("\x1b[31m✗ no such script\x1b[0m {}", t.path_part()),
                (None, _) => match s.prop_str("declaredBy") {
                    Some(d) => format!("\x1b[2m{d}\x1b[0m"),
                    None => String::new(),
                },
            }
        );
    }

    if !verdict_of.is_empty() {
        println!(
            "\n  {wants_attention} of {} declarations want attention; {drifting} disagree with what is installed",
            declared.len()
        );
    }

    // ---- and what has actually run ----------------------------------------
    // Observation only: launchd keeps no history, so this is what the log files
    // an agent names say about the last time it wrote anything.
    let runs = of(NodeKind::RUN);
    if !runs.is_empty() {
        println!("\n\x1b[1mLatest run\x1b[0m  ({} scripts)", runs.len());
        for r in &runs {
            let bytes = r.prop_i64("outBytes").unwrap_or(0) + r.prop_i64("errBytes").unwrap_or(0);
            println!(
                "  {:<46} {:<10} {:<9} {:>10}  {}",
                r.label.chars().take(45).collect::<String>(),
                r.prop_str("fidelity").unwrap_or(""),
                r.prop_str("machine").unwrap_or(""),
                match bytes {
                    0 => "—".to_string(),
                    n => format!("{n} B"),
                },
                r.prop_str("startedAt").unwrap_or(""),
            );
            // The last line of the tail: for anything logging through Python
            // that is the only place progress appears at all, and it is the
            // most recent thing the job had to say.
            if let Some(first) = r
                .prop_str("tail")
                .and_then(|t| t.lines().next_back())
                .filter(|l| !l.trim().is_empty())
            {
                println!(
                    "      \x1b[2m{}\x1b[0m",
                    first.chars().take(100).collect::<String>()
                );
            }
        }
    }

    // ---- does it add up? --------------------------------------------------
    let named: usize = reaching + dangling.len();
    if named > 0 {
        let ok = dangling.is_empty();
        println!(
            "\n  {} {reaching} of {named} schedules that name a script reach one",
            match ok {
                true => "\x1b[32m✓\x1b[0m",
                false => "\x1b[31m✗\x1b[0m",
            }
        );
        // A declaration pointing at nothing is a finding, not a bug: it is a
        // cadence kept for a source whose script was renamed or never written.
        for (label, target) in dangling.iter().take(8) {
            println!("      \x1b[2m{label} → {}\x1b[0m", target.path_part());
        }
        if dangling.len() > 8 {
            println!("      \x1b[2m… and {} more\x1b[0m", dangling.len() - 8);
        }
    }
    println!();
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".")
}
