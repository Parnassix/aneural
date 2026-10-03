//! What the agent sessions in a workspace have cost.
//!
//! `cargo run -p aneural-engine --example tally -- <dir>`
//!
//! Indexes the workspace the way the app does and prints the three tallies:
//! the workspace and each repository, each plan that has been carried out, and
//! each session. The totals are checked against each other on the way out, so
//! the output says whether it adds up rather than asking to be trusted.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{Node, NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_store::{EdgeQuery, NodeQuery};
use std::collections::HashMap;

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
        engine
            .store()
            .query_nodes(&NodeQuery {
                kinds: vec![kind.into()],
                ..Default::default()
            })
            .unwrap()
    };
    let tallies = of(NodeKind::USAGE);
    let plans = of(NodeKind::PLAN);
    let sessions = of(NodeKind::SESSION);

    println!("\n{}", root.display());

    // ---- what each place cost -------------------------------------------
    for t in &tallies {
        let scope = t.prop_str("scope").unwrap_or(".");
        println!(
            "\n\x1b[1m{}\x1b[0m  ({scope})   {} sessions",
            t.label,
            t.prop_i64("sessions").unwrap_or(0),
        );
        for (name, key) in [
            ("input", "tokensIn"),
            ("output", "tokensOut"),
            ("cache write", "cacheWrite"),
            ("cache read", "cacheRead"),
        ] {
            println!("  {name:<12} {:>16}", commas(t.prop_i64(key).unwrap_or(0)));
        }
        println!(
            "  {:<12} {:>16}   ({} messages)",
            "TOKENS",
            commas(t.prop_i64("tokens").unwrap_or(0)),
            commas(t.prop_i64("messages").unwrap_or(0))
        );
        if let Some(thinking) = t.prop_i64("tokensThinking") {
            println!("  (of the output, {} was thinking)", commas(thinking));
        }
        by_model(t, "  ");

        // The claim the node makes about itself, checked against the nodes it
        // is a total of.
        if scope == "." || tallies.len() == 1 {
            let visible: i64 = sessions.iter().filter_map(|s| s.prop_i64("tokens")).sum();
            let swept = t.prop_i64("tokensSwept").unwrap_or(0);
            let total = t.prop_i64("tokens").unwrap_or(0);
            let ok = total - swept == visible;
            println!(
                "\n  {} {} total − {} swept ({} sessions off the canvas) = {} = the {} Session nodes",
                if ok {
                    "\x1b[32m✓\x1b[0m"
                } else {
                    "\x1b[31m✗\x1b[0m"
                },
                commas(total),
                commas(swept),
                t.prop_i64("sessionsSwept").unwrap_or(0),
                commas(visible),
                sessions.len(),
            );
        }
    }

    // ---- what each plan cost --------------------------------------------
    // Which sessions carried each plan out, so a cost can be traced to the
    // work it came from rather than appearing out of nowhere.
    let mut by_plan: HashMap<NodeId, Vec<String>> = HashMap::new();
    let titles: HashMap<&NodeId, &str> =
        sessions.iter().map(|s| (&s.id, s.label.as_str())).collect();
    for edge in engine
        .store()
        .get_edges(&EdgeQuery {
            kinds: vec![EdgeKind::REALIZES.into()],
            ..Default::default()
        })
        .unwrap()
    {
        if let Some(title) = titles.get(&edge.src) {
            by_plan
                .entry(edge.dst.clone())
                .or_default()
                .push((*title).to_string());
        }
    }

    let mut plans = plans;
    plans.sort_by_key(|p| std::cmp::Reverse(p.prop_i64("tokens").unwrap_or(0)));
    println!("\n\x1b[1mPlans\x1b[0m");
    for p in &plans {
        let mut who = by_plan.get(&p.id).cloned().unwrap_or_default();
        who.sort();
        who.dedup();
        println!(
            "  {:>15}  {:<44} {}",
            match p.prop_i64("tokens") {
                Some(t) => commas(t),
                // A plan from the `plans` spore is a document in the
                // workspace, not one Claude approved, so nothing ran under it.
                None => "—".into(),
            },
            p.label.chars().take(43).collect::<String>(),
            match who.is_empty() {
                true => p.prop_str("state").unwrap_or("").to_string(),
                false => format!("← {}", who.join(", ")),
            }
        );
        by_model(p, "                   ");
    }

    // ---- and each session -------------------------------------------------
    let mut sessions = sessions;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.prop_i64("tokens").unwrap_or(0)));
    println!("\n\x1b[1mSessions\x1b[0m");
    for s in &sessions {
        println!(
            "  {:>15}  {:<44} {}",
            commas(s.prop_i64("tokens").unwrap_or(0)),
            s.label.chars().take(43).collect::<String>(),
            s.prop_str("models").unwrap_or("")
        );
        by_model(s, "                   ");
    }
    println!();
}

/// The `byModel` rows under whatever they belong to. Skipped entirely when
/// one model did all of it — a breakdown of one is just the total again.
fn by_model(node: &Node, indent: &str) {
    let Some(rows) = node.props.get("byModel").and_then(|v| v.as_array()) else {
        return;
    };
    if rows.len() < 2 {
        return;
    }
    for row in rows {
        let n = |k: &str| commas(row.get(k).and_then(|v| v.as_i64()).unwrap_or(0));
        println!(
            "{indent}{:<30} {:>15}  out {:>10}  cache r {:>15}  msgs {:>6}",
            row.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
            n("tokens"),
            n("out"),
            n("cacheRead"),
            n("messages"),
        );
    }
}

fn commas(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    match n < 0 {
        true => format!("-{out}"),
        false => out,
    }
}
