//! Spore loading and harvesting. First-party spores are embedded in the binary;
//! workspace spores live in `.aneural/spores/<name>/spore.json`.

pub mod http;
pub mod markdown;
pub mod sqlite;
pub mod template;

use aneural_core::config::{NodeTypeDef, SporesConfig};
use aneural_core::kinds::Source;
use aneural_core::spore::{Emit, Harvester, MarkdownGranularity, SporeInfo, SporeManifest};
use aneural_core::{Edge, Node, NodeId, Workspace};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use template::{Vars, coerce, render};

/// (name, manifest json) for the spores shipped with Aneural.
pub const BUILTIN_SPORES: &[(&str, &str)] = &[
    (
        "comments",
        include_str!("../../../../spores/comments/spore.json"),
    ),
    ("plans", include_str!("../../../../spores/plans/spore.json")),
    (
        "icebox",
        include_str!("../../../../spores/icebox/spore.json"),
    ),
    (
        "wiki-links",
        include_str!("../../../../spores/wiki-links/spore.json"),
    ),
    // Off by default: it walks every `.db` in the workspace, which is worth
    // opting into rather than assuming.
    (
        "database",
        include_str!("../../../../spores/database/spore.json"),
    ),
    // Off by default, and the only shipped spore that needs consent: it makes
    // web requests with the user's own GitHub token.
    (
        "github",
        include_str!("../../../../spores/github/spore.json"),
    ),
    // Off by default: a repository of scripts grows a node per script, which is
    // hundreds in a workspace built around them. Worth opting into.
    (
        "scripts",
        include_str!("../../../../spores/scripts/spore.json"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum SporeError {
    #[error("{name}: {}", problems.join("; "))]
    Invalid { name: String, problems: Vec<String> },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A loaded, compiled spore.
pub struct Spore {
    pub manifest: SporeManifest,
    pub location: String,
    pub path: String,
    pub enabled: bool,
    harvesters: Vec<CompiledHarvester>,
}

struct CompiledHarvester {
    def: Harvester,
    include: GlobSet,
    exclude: GlobSet,
    regex: Option<Regex>,
}

impl Spore {
    /// Node kinds this spore emits without an origin — an unresolved wiki-link
    /// placeholder, a tag.
    ///
    /// Those nodes belong to no one file, so nothing retracts them when a file
    /// changes and the caller has to collect them by kind once nothing points
    /// at them any more.
    pub fn shared_node_kinds(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut add = |kind: &String| {
            if !out.contains(kind) {
                out.push(kind.clone());
            }
        };
        for h in &self.harvesters {
            if let Harvester::Markdown {
                wikilinks, tags, ..
            } = &h.def
            {
                if let Some(spec) = wikilinks.as_ref().and_then(|w| w.unresolved.as_ref()) {
                    add(&spec.kind);
                }
                if let Some(t) = tags {
                    add(&t.node.kind);
                }
            }
        }
        out
    }

    /// A summary for the CLI, the GUI and MCP. `settings` is the workspace's
    /// answers, so the caller learns which required ones are still blank.
    pub fn info_with(&self, settings: &BTreeMap<String, String>) -> SporeInfo {
        let mut info = self.info();
        info.settings = self
            .manifest
            .settings
            .iter()
            .map(|d| aneural_core::spore::SettingValue {
                key: d.key.clone(),
                label: if d.label.is_empty() {
                    d.key.clone()
                } else {
                    d.label.clone()
                },
                description: d.description.clone(),
                example: d.example.clone(),
                required: d.required,
                value: settings
                    .get(&d.key)
                    .filter(|v| !v.trim().is_empty())
                    .cloned(),
            })
            .collect();
        info.missing_settings = info
            .settings
            .iter()
            .filter(|s| s.required && s.value.is_none())
            .map(|s| s.key.clone())
            .collect();
        info
    }

    pub fn info(&self) -> SporeInfo {
        SporeInfo {
            id: self.manifest.id(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            display_name: if self.manifest.display_name.is_empty() {
                self.manifest.name.clone()
            } else {
                self.manifest.display_name.clone()
            },
            description: self.manifest.description.clone(),
            enabled: self.enabled,
            location: self.location.clone(),
            path: self.path.clone(),
            node_kinds: self
                .manifest
                .node_types
                .iter()
                .map(|n| n.kind.clone())
                .collect(),
            tier: self.manifest.tier().label().to_string(),
            consent_lines: self
                .manifest
                .capabilities
                .iter()
                .map(|c| c.consent_line())
                .collect(),
            missing_settings: Vec::new(),
            settings: Vec::new(),
        }
    }

    pub fn node_types(&self) -> Vec<NodeTypeDef> {
        self.manifest
            .node_types
            .iter()
            .cloned()
            .map(|mut d| {
                d.provider = Source::spore(&self.manifest.id());
                if d.label.is_empty() {
                    d.label = d.kind.clone();
                }
                d
            })
            .collect()
    }

    /// This spore's HTTP harvesters. They are not reachable through the
    /// file-driven path at all, so the refresh loop asks for them by name.
    pub fn http_harvesters(&self) -> impl Iterator<Item = &Harvester> {
        self.harvesters
            .iter()
            .map(|h| &h.def)
            .filter(|d| matches!(d, Harvester::Http { .. }))
    }

    /// Whether any harvester of this spore applies to the given path.
    pub fn applies_to(&self, rel: &str) -> bool {
        self.harvesters.iter().any(|h| h.matches(rel))
    }
}

impl CompiledHarvester {
    fn matches(&self, rel: &str) -> bool {
        // An HTTP harvester declares no `include`, and an empty include set
        // means "every file" for the file-driven kinds — so it has to be ruled
        // out explicitly or it would claim every path in the workspace.
        if matches!(self.def, Harvester::Http { .. }) {
            return false;
        }
        let inc = self.include.is_empty() || self.include.is_match(rel);
        inc && !self.exclude.is_match(rel)
    }
}

/// `compile` needs a `&Vec<String>` to hand the glob builder for the kinds that
/// declare no globs at all.
static EMPTY: Vec<String> = Vec::new();

/// Compile a manifest; returns human-readable errors for bad globs/regexes.
pub fn compile(
    manifest: SporeManifest,
    location: &str,
    path: &str,
    enabled: bool,
) -> Result<Spore, SporeError> {
    let mut errs = manifest.validate();
    let mut harvesters = Vec::new();
    for h in &manifest.harvesters {
        let (include, exclude, pattern) = match h {
            Harvester::Regex {
                include,
                exclude,
                pattern,
                ..
            } => (include, exclude, Some(pattern)),
            Harvester::TreeSitter {
                include, exclude, ..
            }
            | Harvester::Markdown {
                include, exclude, ..
            }
            | Harvester::Sqlite {
                include, exclude, ..
            } => (include, exclude, None),
            // An HTTP harvester has no file behind it, so it has no globs. It
            // is still compiled in, because the refresh loop has to find it.
            Harvester::Http { .. } => (&EMPTY, &EMPTY, None),
            Harvester::Wasm { .. } => continue,
        };
        let include = match globset(include) {
            Ok(g) => g,
            Err(e) => {
                errs.push(format!("harvester `{}`: bad include glob: {e}", h.id()));
                continue;
            }
        };
        let exclude = match globset(exclude) {
            Ok(g) => g,
            Err(e) => {
                errs.push(format!("harvester `{}`: bad exclude glob: {e}", h.id()));
                continue;
            }
        };
        let regex = match pattern {
            Some(p) => match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    errs.push(format!("harvester `{}`: bad regex: {e}", h.id()));
                    continue;
                }
            },
            None => None,
        };
        harvesters.push(CompiledHarvester {
            def: h.clone(),
            include,
            exclude,
            regex,
        });
    }
    if !errs.is_empty() {
        return Err(SporeError::Invalid {
            name: manifest.name.clone(),
            problems: errs,
        });
    }
    Ok(Spore {
        manifest,
        location: location.into(),
        path: path.into(),
        enabled,
        harvesters,
    })
}

/// Every problem with a manifest, not just the first. `compile` collapses them
/// into one error for logging; callers that show a user a list want them apart.
pub fn compile_report(manifest: SporeManifest, path: &str) -> Vec<String> {
    match compile(manifest, "check", path, true) {
        Ok(_) => Vec::new(),
        Err(SporeError::Invalid { problems, .. }) => problems,
        Err(e) => vec![e.to_string()],
    }
}

pub fn globset(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
    }
    b.build()
}

/// Load builtin + workspace spores. Invalid workspace spores are returned as errors, not fatal.
pub fn load_all(ws: &Workspace, enabled: &[String]) -> (Vec<Spore>, Vec<SporeError>) {
    let mut spores = Vec::new();
    let mut errors = Vec::new();
    for (name, json) in BUILTIN_SPORES {
        let manifest: SporeManifest = serde_json::from_str(json).expect("builtin spore json");
        let on = enabled
            .iter()
            .any(|e| SporesConfig::entry_matches(e, &manifest.id(), name));
        match compile(
            manifest,
            "builtin",
            &format!("spores/{name}/spore.json"),
            on,
        ) {
            Ok(s) => spores.push(s),
            Err(e) => errors.push(e),
        }
    }
    if let Ok(rd) = std::fs::read_dir(ws.spores_dir()) {
        let mut dirs: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let mf = dir.join("spore.json");
            if !mf.exists() {
                continue;
            }
            let rel = ws.rel(&mf).unwrap_or_else(|| mf.display().to_string());
            let text = match std::fs::read_to_string(&mf) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(SporeError::Io(e));
                    continue;
                }
            };
            let manifest: SporeManifest = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    errors.push(SporeError::Invalid {
                        name: rel.clone(),
                        problems: vec![e.to_string()],
                    });
                    continue;
                }
            };
            let on = enabled
                .iter()
                .any(|e| SporesConfig::entry_matches(e, &manifest.id(), &manifest.name));
            match compile(manifest, "workspace", &rel, on) {
                Ok(s) => {
                    // A workspace spore shadows a builtin only when it is the
                    // same spore: `bob.comments` must not displace
                    // `aneural.comments` just by sharing a name.
                    let id = s.manifest.id();
                    spores.retain(|b: &Spore| b.manifest.id() != id);
                    spores.push(s);
                }
                Err(e) => errors.push(e),
            }
        }
    }
    (spores, errors)
}

/// Resolves `[[Wiki Link]]` names to workspace-relative markdown paths.
#[derive(Default, Clone)]
pub struct MarkdownIndex {
    by_stem: HashMap<String, Vec<String>>,
    /// Names a note answers to besides its filename, from its frontmatter.
    /// Kept apart from `by_stem` so a real file always wins over an alias.
    by_alias: HashMap<String, Vec<String>>,
}

impl MarkdownIndex {
    pub fn insert(&mut self, rel: &str) {
        if let Some(stem) = stem_of(rel) {
            push_unique(self.by_stem.entry(stem).or_default(), rel);
        }
    }

    /// Record the `aliases:` a note declares, replacing any it declared before.
    pub fn insert_aliases(&mut self, rel: &str, aliases: &[String]) {
        for v in self.by_alias.values_mut() {
            v.retain(|p| p != rel);
        }
        for a in aliases {
            let key = normalise(a);
            if !key.is_empty() {
                push_unique(self.by_alias.entry(key).or_default(), rel);
            }
        }
        self.by_alias.retain(|_, v| !v.is_empty());
    }

    pub fn remove(&mut self, rel: &str) {
        if let Some(stem) = stem_of(rel)
            && let Some(v) = self.by_stem.get_mut(&stem)
        {
            v.retain(|p| p != rel);
        }
        for v in self.by_alias.values_mut() {
            v.retain(|p| p != rel);
        }
        self.by_alias.retain(|_, v| !v.is_empty());
    }

    /// Prefer a file in the same directory as `from`, then the shortest path.
    ///
    /// The link text may carry a path (`[[../../Notes|Notes]]` is what Obsidian
    /// writes when a link crosses folders), but resolution is by name: that
    /// path is Obsidian's own bookkeeping and goes stale the moment a note is
    /// moved, whereas the name is what the author typed.
    pub fn resolve(&self, target: &str, from: &str) -> Option<String> {
        let key = normalise(target);
        let candidates = self.by_stem.get(&key).or_else(|| self.by_alias.get(&key))?;
        let from_dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        candidates
            .iter()
            .find(|c| c.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == from_dir)
            .or_else(|| candidates.iter().min_by_key(|c| c.len()))
            .cloned()
    }
}

fn push_unique(v: &mut Vec<String>, rel: &str) {
    if !v.iter().any(|p| p == rel) {
        v.push(rel.to_string());
        v.sort();
    }
}

/// A link target or an alias reduced to the key both sides of the index use.
fn normalise(name: &str) -> String {
    let lower = name.trim().trim_end_matches(".md").to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    base.trim().to_string()
}

fn stem_of(rel: &str) -> Option<String> {
    let base = rel.rsplit('/').next()?;
    let (stem, ext) = base.rsplit_once('.')?;
    if !matches!(ext, "md" | "mdx" | "markdown") {
        return None;
    }
    Some(stem.to_lowercase())
}

/// Whether a harvester opens the file itself rather than being handed its bytes.
/// These run even for files too large for the engine to read.
pub fn opens_its_own_file(h: &Harvester) -> bool {
    matches!(h, Harvester::Sqlite { .. })
}

/// Run only the harvesters that open the file themselves. Used for files past
/// `walk::MAX_PARSE_BYTES`, where a dev database routinely lands.
pub fn harvest_large(spores: &[Spore], rel: &str, abs: &std::path::Path) -> Harvest {
    let mut out = Harvest::default();
    for spore in spores.iter().filter(|s| s.enabled) {
        let src = Source::spore(&spore.manifest.id());
        for h in spore
            .harvesters
            .iter()
            .filter(|h| opens_its_own_file(&h.def) && h.matches(rel))
        {
            if let Harvester::Sqlite {
                emit,
                references,
                sample_rows,
                ..
            } = &h.def
            {
                sqlite::harvest(
                    abs,
                    rel,
                    emit,
                    references.as_ref(),
                    *sample_rows,
                    &src,
                    &mut out,
                );
            }
        }
    }
    dedupe(&mut out);
    out
}

/// Whether any enabled spore would harvest this path without reading it.
pub fn has_large_file_harvester(spores: &[Spore], rel: &str) -> bool {
    spores.iter().filter(|s| s.enabled).any(|s| {
        s.harvesters
            .iter()
            .any(|h| opens_its_own_file(&h.def) && h.matches(rel))
    })
}

/// Output of harvesting one file with one or more spores.
#[derive(Default, Debug)]
pub struct Harvest {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Run every enabled harvester of every enabled spore over one file.
pub fn harvest_file(
    spores: &[Spore],
    md_index: &MarkdownIndex,
    rel: &str,
    abs: &std::path::Path,
    source: &[u8],
) -> Harvest {
    let mut out = Harvest::default();
    let text = String::from_utf8_lossy(source);
    for spore in spores.iter().filter(|s| s.enabled) {
        let src = Source::spore(&spore.manifest.id());
        for h in spore.harvesters.iter().filter(|h| h.matches(rel)) {
            match &h.def {
                Harvester::Regex { emit, once, .. } => {
                    let re = h.regex.as_ref().expect("compiled regex");
                    harvest_regex(re, emit, *once, &src, rel, &text, &mut out);
                }
                Harvester::TreeSitter {
                    language,
                    query,
                    emit,
                    ..
                } => {
                    harvest_tree_sitter(language, query, emit, &src, rel, source, &mut out);
                }
                Harvester::Markdown {
                    granularity,
                    emit,
                    wikilinks,
                    annotate,
                    tags,
                    table,
                    ..
                } => {
                    harvest_markdown(
                        *granularity,
                        emit.as_ref(),
                        wikilinks.as_ref(),
                        annotate.as_ref(),
                        tags.as_ref(),
                        table.as_ref(),
                        &src,
                        rel,
                        &text,
                        md_index,
                        &mut out,
                    );
                }
                Harvester::Sqlite {
                    emit,
                    references,
                    sample_rows,
                    ..
                } => {
                    sqlite::harvest(
                        abs,
                        rel,
                        emit,
                        references.as_ref(),
                        *sample_rows,
                        &src,
                        &mut out,
                    );
                }
                // Driven by the refresh loop, not by the walker.
                Harvester::Http { .. } | Harvester::Wasm { .. } => {}
            }
        }
    }
    dedupe(&mut out);
    out
}

/// A node that belongs to no single file.
///
/// Emitted with **no origin**, so `replace_origin` never retracts it when one
/// of the files pointing at it changes; `Store::gc_orphan_by_kind` collects it
/// once nothing points at it at all. The same arrangement as a `Package`.
fn shared_node(
    spec: &aneural_core::spore::EmitNode,
    source: &str,
    v: &Vars,
    fallback_label: &str,
) -> Option<Node> {
    let id = NodeId::parse(&render(&spec.id, v)).ok()?;
    let mut node = Node::new(
        id,
        spec.kind.clone(),
        render(&spec.label, v).trim().to_string(),
        source,
    );
    if node.label.is_empty() {
        node.label = fallback_label.to_string();
    }
    if let serde_json::Value::Object(map) = &mut node.props {
        for (k, tpl) in &spec.props {
            let rendered = render(tpl, v);
            if !rendered.is_empty() {
                map.insert(k.clone(), coerce(&rendered));
            }
        }
    }
    Some(node)
}

/// Collapse repeats within one file's harvest.
///
/// Two harvesters describing the same thing from different places is a
/// composition pattern, not a mistake: one reads a script's id out of a module
/// constant, another notices which CLI library it imports, and together they
/// describe one script. So a later node **fills in props the first one did not
/// have** rather than being dropped. It never overwrites: the first writer owns
/// the kind, the label and every prop it set, so the result does not depend on
/// which order the harvesters happen to run in.
fn dedupe(h: &mut Harvest) {
    let mut at: std::collections::HashMap<NodeId, usize> = std::collections::HashMap::new();
    let mut merged: Vec<Node> = Vec::with_capacity(h.nodes.len());
    for node in std::mem::take(&mut h.nodes) {
        match at.get(&node.id) {
            Some(&i) => fill_props(&mut merged[i], node),
            None => {
                at.insert(node.id.clone(), merged.len());
                merged.push(node);
            }
        }
    }
    h.nodes = merged;

    let mut seen_e = std::collections::HashSet::new();
    h.edges.retain(|e| {
        seen_e.insert((
            e.kind.clone(),
            e.src.clone(),
            e.dst.clone(),
            e.source.clone(),
        ))
    });
}

/// Add whatever `later` says that `kept` does not already say.
fn fill_props(kept: &mut Node, later: Node) {
    let serde_json::Value::Object(extra) = later.props else {
        return;
    };
    let serde_json::Value::Object(into) = &mut kept.props else {
        return;
    };
    for (key, value) in extra {
        into.entry(key).or_insert(value);
    }
}

pub(crate) fn base_vars(rel: &str) -> Vars {
    let mut v = Vars::new();
    v.insert("file".into(), rel.to_string());
    v.insert(
        "basename".into(),
        rel.rsplit('/').next().unwrap_or(rel).to_string(),
    );
    v
}

pub(crate) fn emit_node(
    emit: &Emit,
    source: &str,
    rel: &str,
    vars: &Vars,
    out: &mut Harvest,
) -> Option<NodeId> {
    let id_str = render(&emit.node.id, vars);
    let id = NodeId::parse(&id_str).ok()?;
    let mut node = Node::new(
        id.clone(),
        emit.node.kind.clone(),
        render(&emit.node.label, vars).trim().to_string(),
        source,
    )
    .with_origin(rel);
    if let serde_json::Value::Object(map) = &mut node.props {
        for (k, tpl) in &emit.node.props {
            let v = render(tpl, vars);
            if !v.is_empty() {
                map.insert(k.clone(), coerce(&v));
            }
        }
    }
    if node.label.is_empty() {
        node.label = id.fragment().unwrap_or(id.path_part()).to_string();
    }
    out.nodes.push(node);
    for e in &emit.edges {
        let src = if e.src == "$node" {
            id.clone()
        } else {
            NodeId::new(render(&e.src, vars))
        };
        let dst = if e.dst == "$node" {
            id.clone()
        } else {
            NodeId::new(render(&e.dst, vars))
        };
        if NodeId::parse(src.as_str()).is_err() || NodeId::parse(dst.as_str()).is_err() {
            continue;
        }
        let mut edge = Edge::new(e.kind.clone(), src, dst, source).with_origin(rel);
        if let serde_json::Value::Object(map) = &mut edge.props {
            for (k, tpl) in &e.props {
                let v = render(tpl, vars);
                if !v.is_empty() {
                    map.insert(k.clone(), coerce(&v));
                }
            }
        }
        out.edges.push(edge);
    }
    Some(id)
}

fn harvest_regex(
    re: &Regex,
    emit: &Emit,
    once: bool,
    source: &str,
    rel: &str,
    text: &str,
    out: &mut Harvest,
) {
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let mut vars = base_vars(rel);
            vars.insert("line".into(), (i + 1).to_string());
            vars.insert(
                "match".into(),
                caps.get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            );
            for name in re.capture_names().flatten() {
                if let Some(m) = caps.name(name) {
                    vars.insert(name.to_string(), m.as_str().trim().to_string());
                }
            }
            emit_node(emit, source, rel, &vars, out);
            if once {
                return;
            }
        }
    }
}

fn harvest_tree_sitter(
    language: &str,
    query: &str,
    emit: &Emit,
    source: &str,
    rel: &str,
    bytes: &[u8],
    out: &mut Harvest,
) {
    let path = Path::new(rel);
    let Ok(matches) = aneural_lang::run_query(language, Some(path), query, bytes) else {
        return;
    };
    for caps in matches {
        let mut vars = base_vars(rel);
        if let Some(first) = caps.first() {
            vars.insert("line".into(), first.line.to_string());
        }
        for c in caps {
            vars.insert(c.name.clone(), c.text.trim().to_string());
        }
        emit_node(emit, source, rel, &vars, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn harvest_markdown(
    granularity: MarkdownGranularity,
    emit: Option<&Emit>,
    wikilinks: Option<&aneural_core::spore::WikiLinks>,
    annotate: Option<&aneural_core::spore::Annotate>,
    tags: Option<&aneural_core::spore::Tags>,
    table: Option<&aneural_core::spore::TableSelect>,
    source: &str,
    rel: &str,
    text: &str,
    md_index: &MarkdownIndex,
    out: &mut Harvest,
) {
    let fallback = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".md")
        .to_string();
    let doc = markdown::parse(text, &fallback);
    let mut vars = base_vars(rel);
    vars.insert("title".into(), doc.title.clone());
    vars.insert("body".into(), doc.body.trim().to_string());
    vars.insert("line".into(), (doc.body_offset + 1).to_string());
    for (k, v) in &doc.frontmatter.scalars {
        vars.insert(format!("fm.{k}"), v.clone());
    }
    for (k, v) in &doc.frontmatter.lists {
        vars.insert(format!("fm.{k}"), v.join(", "));
    }
    let file_id = NodeId::file(rel);

    let link_edge = |from: &NodeId, target: &str, line: u32, out: &mut Harvest| {
        let Some(wl) = wikilinks else { return };
        if let Some(dst_rel) = md_index.resolve(target, rel) {
            let dst = NodeId::file(&dst_rel);
            if &dst == from {
                return;
            }
            out.edges.push(
                Edge::new(wl.edge_kind.clone(), from.clone(), dst, source)
                    .with_origin(rel)
                    .with_prop("via", "wikilink")
                    .with_prop("line", line as i64),
            );
            return;
        }
        // Nothing of that name exists. The note was still asked for, and by how
        // many callers, which is worth more than a dropped edge.
        let Some(spec) = &wl.unresolved else { return };
        let label = target.trim();
        let slug = aneural_core::slug(label);
        if slug.is_empty() {
            return;
        }
        let mut v: Vars = Vars::new();
        v.insert("target".into(), label.to_string());
        v.insert("slug".into(), slug);
        let Some(mut node) = shared_node(spec, source, &v, label) else {
            return;
        };
        let dst = node.id.clone();
        if &dst == from {
            return;
        }
        if let serde_json::Value::Object(map) = &mut node.props {
            // Stamped by the engine, not the manifest: a reader has to be able
            // to tell a placeholder from a real node without knowing which
            // spore made it or what it chose to call the kind.
            map.insert("unresolved".into(), true.into());
        }
        out.nodes.push(node);
        out.edges.push(
            Edge::new(wl.edge_kind.clone(), from.clone(), dst, source)
                .with_origin(rel)
                .with_prop("via", "unresolved")
                .with_prop("line", line as i64),
        );
    };

    // Frontmatter tags. A tag is a name many files share, so like a missing
    // note it is one node with no origin rather than one per file — which is
    // also what makes "everything tagged fda" a thing you can see.
    let tag_edges = |from: &NodeId, out: &mut Harvest| {
        let Some(t) = tags else { return };
        for tag in markdown::declared_list(&doc.frontmatter, &t.frontmatter_key) {
            let slug = aneural_core::slug(&tag);
            if slug.is_empty() {
                continue;
            }
            let mut v: Vars = Vars::new();
            v.insert("tag".into(), tag.clone());
            v.insert("slug".into(), slug);
            let Some(node) = shared_node(&t.node, source, &v, &tag) else {
                continue;
            };
            let dst = node.id.clone();
            if &dst == from {
                continue;
            }
            out.nodes.push(node);
            out.edges.push(
                Edge::new(t.edge_kind.clone(), from.clone(), dst, source)
                    .with_origin(rel)
                    .with_prop("via", "tag"),
            );
        }
    };

    match granularity {
        MarkdownGranularity::Document => {
            let anchor = match emit {
                Some(e) => emit_node(e, source, rel, &vars, out).unwrap_or_else(|| file_id.clone()),
                None => file_id.clone(),
            };
            for l in &doc.links {
                link_edge(&anchor, &l.target, l.line, out);
            }
            tag_edges(&anchor, out);
            if let (Some(a), Some(_)) = (annotate, emit)
                && let Some(targets) = doc.frontmatter.lists.get(&a.frontmatter_key)
            {
                for t in targets {
                    let dst = NodeId::file(t.trim_start_matches("./"));
                    out.edges.push(
                        Edge::new(a.edge_kind.clone(), anchor.clone(), dst, source)
                            .with_origin(rel)
                            .with_prop("via", "frontmatter"),
                    );
                }
            }
        }
        MarkdownGranularity::Heading => {
            // Frontmatter describes the document, so its tags attach to the
            // file rather than being copied onto every heading in it.
            tag_edges(&file_id, out);
            let Some(e) = emit else { return };
            let mut names: BTreeMap<String, usize> = BTreeMap::new();
            for (i, section) in doc.sections.iter().enumerate() {
                let mut v = vars.clone();
                let mut heading = section.heading.clone();
                // disambiguate duplicate headings
                let n = names.entry(aneural_core::slug(&heading)).or_insert(0);
                *n += 1;
                if *n > 1 {
                    heading = format!("{heading} ({n})");
                }
                v.insert("heading".into(), heading);
                v.insert("line".into(), section.line.to_string());
                v.insert("body".into(), section.body.clone());
                if let Some(id) = emit_node(e, source, rel, &v, out) {
                    let next = doc.sections.get(i + 1).map(|s| s.line);
                    for l in markdown::links_in(&doc, section, next) {
                        link_edge(&id, &l.target, l.line, out);
                    }
                }
            }
        }
        MarkdownGranularity::TableRow => {
            // As with headings, frontmatter describes the document, so its tags
            // attach to the file rather than to every row of every table.
            tag_edges(&file_id, out);
            let Some(e) = emit else { return };
            let required = table.map(|t| t.requires.as_slice()).unwrap_or(&[]);
            for t in markdown::tables(&doc) {
                if !t.has_columns(required) {
                    continue;
                }
                for row in &t.rows {
                    let mut v = vars.clone();
                    v.insert("table".into(), t.heading.clone());
                    v.insert("headers".into(), t.headers.join(", "));
                    v.insert("row".into(), row.row.to_string());
                    v.insert("line".into(), row.line.to_string());
                    for (key, text) in &row.cells {
                        v.insert(format!("col.{key}"), text.clone());
                    }
                    for (i, text) in row.positional.iter().enumerate() {
                        v.insert(format!("col.{}", i + 1), text.clone());
                    }
                    if let Some(id) = emit_node(e, source, rel, &v, out) {
                        // A link written in a cell is about that row, not about
                        // the document, so it hangs off the row's own node.
                        for l in doc.links.iter().filter(|l| l.line == row.line) {
                            link_edge(&id, &l.target, l.line, out);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a workspace spore and load everything with the given `enabled` list.
    fn load_with(enabled: &[&str], extra: &[(&str, &str)]) -> (Vec<Spore>, Vec<SporeError>) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::at(tmp.path());
        ws.init(Some("t"), false).unwrap();
        for (dir, json) in extra {
            let d = ws.spores_dir().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("spore.json"), json).unwrap();
        }
        let enabled: Vec<String> = enabled.iter().map(|s| s.to_string()).collect();
        load_all(&ws, &enabled)
    }

    fn is_on(spores: &[Spore], id: &str) -> bool {
        spores.iter().any(|s| s.manifest.id() == id && s.enabled)
    }

    #[test]
    fn builtins_are_first_party_and_enable_by_bare_or_qualified_name() {
        let (spores, errs) = load_with(&["comments", "aneural.plans"], &[]);
        assert!(errs.is_empty(), "{errs:?}");

        // The bare name is what every existing workspace has on disk.
        assert!(is_on(&spores, "aneural.comments"));
        // The qualified id is what new workspaces write.
        assert!(is_on(&spores, "aneural.plans"));
        assert!(!is_on(&spores, "aneural.icebox"));
    }

    #[test]
    fn a_third_party_spore_cannot_shadow_a_builtin_by_name() {
        // Same `name`, different publisher: it must load *alongside*
        // `aneural.comments`, not replace it.
        let impostor = r##"{
          "publisher": "bob", "name": "comments", "version": "0.1.0",
          "displayName": "Not the real one", "description": "d",
          "nodeTypes": [{ "kind": "BobComment", "icon": "LuCircleDot", "color": "#fff" }],
          "harvesters": [{
            "id": "h", "kind": "markdown", "include": ["**/*.md"],
            "emit": { "node": { "kind": "BobComment", "id": "bob.comments.item:{file}", "label": "{title}" }, "edges": [] }
          }]
        }"##;
        let (spores, errs) =
            load_with(&["comments", "bob.comments"], &[("bob-comments", impostor)]);
        assert!(errs.is_empty(), "{errs:?}");

        assert!(is_on(&spores, "aneural.comments"), "the builtin survives");
        assert!(is_on(&spores, "bob.comments"), "and the impostor loads too");
    }

    #[test]
    fn a_workspace_spore_still_shadows_the_builtin_it_replaces() {
        let fork = r##"{
          "publisher": "aneural", "name": "comments", "version": "9.9.9",
          "displayName": "Local fork", "description": "d",
          "nodeTypes": [{ "kind": "Comment", "icon": "LuCircleDot", "color": "#fff" }],
          "harvesters": [{
            "id": "h", "kind": "markdown", "include": ["**/*.md"],
            "emit": { "node": { "kind": "Comment", "id": "comment:{file}", "label": "{title}" }, "edges": [] }
          }]
        }"##;
        let (spores, errs) = load_with(&["aneural.comments"], &[("comments", fork)]);
        assert!(errs.is_empty(), "{errs:?}");

        let loaded: Vec<&Spore> = spores
            .iter()
            .filter(|s| s.manifest.id() == "aneural.comments")
            .collect();
        assert_eq!(loaded.len(), 1, "exactly one wins");
        assert_eq!(loaded[0].manifest.version, "9.9.9");
        assert_eq!(loaded[0].location, "workspace");
    }

    fn builtins() -> Vec<Spore> {
        BUILTIN_SPORES
            .iter()
            .map(|(name, json)| {
                compile(serde_json::from_str(json).unwrap(), "builtin", name, true).unwrap()
            })
            .collect()
    }

    #[test]
    fn builtin_spores_compile() {
        let s = builtins();
        assert_eq!(s.len(), BUILTIN_SPORES.len());
        assert!(s.iter().any(|s| s.manifest.name == "comments"));
        assert!(s.iter().all(|s| s.manifest.is_first_party()));
    }

    #[test]
    fn comments_harvest() {
        let spores = builtins();
        let src = b"const a = 1; // TODO: make it two\n# not code\n/* FIXME slow */\n// claude: keep this\n";
        let h = harvest_file(
            &spores,
            &MarkdownIndex::default(),
            "src/a.ts",
            std::path::Path::new("src/a.ts"),
            src,
        );
        let labels: Vec<_> = h.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["make it two", "slow", "keep this"]);
        assert_eq!(h.nodes[0].props["tag"], "TODO");
        assert_eq!(h.nodes[0].props["line"], 1);
        assert_eq!(h.nodes[2].props["tag"], "CLAUDE");
        assert_eq!(h.edges.len(), 3);
        assert_eq!(h.edges[0].dst, NodeId::file("src/a.ts"));
        assert_eq!(h.nodes[0].origin.as_deref(), Some("src/a.ts"));
    }

    /// Only the `scripts` spore, so a script's nodes are not mixed with a
    /// comment harvested from the same bytes.
    fn scripts_only() -> Vec<Spore> {
        BUILTIN_SPORES
            .iter()
            .filter(|(name, _)| *name == "scripts")
            .map(|(name, json)| {
                compile(serde_json::from_str(json).unwrap(), "builtin", name, true).unwrap()
            })
            .collect()
    }

    fn harvest(spores: &[Spore], rel: &str, src: &[u8]) -> Harvest {
        harvest_file(
            spores,
            &MarkdownIndex::default(),
            rel,
            std::path::Path::new(rel),
            src,
        )
    }

    #[test]
    fn a_file_in_a_scripts_directory_is_a_script_even_with_no_shebang() {
        // The shape of the repository this was written for: 206 Python files in
        // `scripts/`, 128 of them with no shebang and not one marked
        // executable, all run as `uv run python scripts/<name>.py`.
        let h = harvest(
            &scripts_only(),
            "acme-ingest/scripts/download_pubmed.py",
            b"\"\"\"Download PubMed update files.\"\"\"\nSOURCE_ID = \"pubmed_updates\"\n",
        );
        assert_eq!(h.nodes.len(), 1, "{:?}", h.nodes);
        let n = &h.nodes[0];
        assert_eq!(n.kind, "Script");
        assert_eq!(
            n.id,
            NodeId::script("acme-ingest/scripts/download_pubmed.py", None)
        );
        assert_eq!(n.label, "download_pubmed");
        assert_eq!(n.props["found"], "directory");
        assert!(n.props.get("interpreter").is_none());

        // and it is tethered to the file it was found in
        assert_eq!(h.edges.len(), 1);
        assert_eq!(h.edges[0].kind, "REFERENCES");
        assert_eq!(
            h.edges[0].dst,
            NodeId::file("acme-ingest/scripts/download_pubmed.py")
        );
        assert_eq!(h.edges[0].props["via"], "source");
    }

    #[test]
    fn two_harvesters_finding_one_script_describe_it_together() {
        // `shebang` and `script-directory` both emit `script:{file}`. Before
        // dedupe merged props the second was silently dropped, so whichever
        // harvester happened to run second contributed nothing.
        let h = harvest(
            &scripts_only(),
            "bin/release",
            b"#!/usr/bin/env bash\nset -euo pipefail\n",
        );
        assert_eq!(h.nodes.len(), 1, "one script, not two: {:?}", h.nodes);
        let n = &h.nodes[0];
        // the first writer owns `found`, the second still contributes what it knows
        assert_eq!(n.props["found"], "shebang");
        assert_eq!(n.props["interpreter"], "/usr/bin/env");
        assert_eq!(n.label, "release");
        assert_eq!(h.edges.len(), 1, "and the edge is not doubled");
    }

    #[test]
    fn a_document_next_to_the_scripts_is_not_a_script() {
        let spores = scripts_only();
        for rel in [
            "scripts/README.md",
            "scripts/requirements.txt",
            "scripts/config.json",
            "bin/logo.svg",
        ] {
            let h = harvest(&spores, rel, b"anything at all\n");
            assert!(h.nodes.is_empty(), "{rel} became {:?}", h.nodes);
        }
    }

    #[test]
    fn a_crontab_line_and_a_workflow_cron_both_become_schedules() {
        let spores = scripts_only();
        let h = harvest(
            &spores,
            "deploy/crontab",
            b"# comment\n0 4 * * * /usr/local/bin/ingest --since=yesterday\n*/15 * * * * ping\n",
        );
        assert_eq!(h.nodes.len(), 2, "{:?}", h.nodes);
        assert_eq!(h.nodes[0].kind, "Schedule");
        assert_eq!(h.nodes[0].props["cron"], "0 4 * * *");
        assert_eq!(
            h.nodes[0].props["command"],
            "/usr/local/bin/ingest --since=yesterday"
        );
        assert_eq!(h.nodes[0].props["declaredBy"], "crontab");
        assert_eq!(h.nodes[0].props["line"], 2);
        assert_eq!(h.nodes[1].props["cron"], "*/15 * * * *");
        assert_eq!(h.edges[0].props["via"], "declared");

        let h = harvest(
            &spores,
            ".github/workflows/nightly.yml",
            b"on:\n  schedule:\n    - cron: '0 3 * * *'\n",
        );
        assert_eq!(h.nodes.len(), 1, "{:?}", h.nodes);
        assert_eq!(h.nodes[0].props["cron"], "0 3 * * *");
        assert_eq!(h.nodes[0].props["declaredBy"], "github-actions");
        assert_eq!(h.nodes[0].props["workflow"], "nightly");
    }

    #[test]
    fn a_table_of_cadences_becomes_a_schedule_per_row() {
        // The per-project half: a workspace spore reads a project's own table.
        // Two tables in one file, keyed the same way, describe one schedule
        // between them.
        let spore = r##"{
          "publisher": "acme", "name": "cadences", "version": "0.1.0",
          "harvesters": [
            {
              "id": "index", "kind": "markdown", "include": ["**/Cadences.md"],
              "granularity": "table-row",
              "table": { "requires": ["source_id", "cadence"] },
              "emit": { "node": {
                "kind": "Schedule", "id": "acme.cadences.schedule:{file}#{slug(col.source_id)}",
                "label": "{col.source_id}",
                "props": { "cadence": "{col.cadence}", "lastRefreshed": "{col.last_refreshed}" }
              }, "edges": [
                { "kind": "ANNOTATES", "src": "$node",
                  "dst": "script:scripts/download_{col.source_id}.py",
                  "props": { "via": "schedule" } }
              ] }
            },
            {
              "id": "shapes", "kind": "markdown", "include": ["**/Cadences.md"],
              "granularity": "table-row",
              "table": { "requires": ["source_id", "refresh_shape"] },
              "emit": { "node": {
                "kind": "Schedule", "id": "acme.cadences.schedule:{file}#{slug(col.source_id)}",
                "label": "{col.source_id}",
                "props": { "refreshShape": "{col.refresh_shape}" }
              } }
            }
          ]
        }"##;
        let (spores, errs) = load_with(&["acme.cadences"], &[("cadences", spore)]);
        assert!(errs.is_empty(), "{errs:?}");

        let src = concat!(
            "## The index\n\n",
            "| source_id | cadence | last_refreshed |\n",
            "|---|---|---|\n",
            "| `aact` | daily | 2026-06-07 |\n",
            "| `gdelt` | weekly-mon | 2026-06-01 |\n\n",
            "## Refresh shapes\n\n",
            "| source_id | refresh_shape |\n",
            "|---|---|\n",
            "| `aact` | full-re-pull |\n",
        );
        let h = harvest(&spores, "docs/Cadences.md", src.as_bytes());

        assert_eq!(h.nodes.len(), 2, "one per source_id: {:?}", h.nodes);
        let aact = h.nodes.iter().find(|n| n.label == "aact").expect("aact");
        assert_eq!(aact.props["cadence"], "daily");
        assert_eq!(aact.props["lastRefreshed"], "2026-06-07");
        // the second table's column arrived without displacing the first's
        assert_eq!(aact.props["refreshShape"], "full-re-pull");

        let gdelt = h.nodes.iter().find(|n| n.label == "gdelt").unwrap();
        assert_eq!(gdelt.props["cadence"], "weekly-mon");
        assert!(
            gdelt.props.get("refreshShape").is_none(),
            "not in that table"
        );

        // each row is tethered to the script its key names
        assert!(h.edges.iter().any(
            |e| e.dst == NodeId::script("scripts/download_aact.py", None)
                && e.props["via"] == "schedule"
        ));
    }

    #[test]
    fn plans_and_icebox_and_links() {
        let spores = builtins();
        let mut idx = MarkdownIndex::default();
        idx.insert("README.md");
        idx.insert(".aneural/notes/Architecture.md");
        idx.insert(".aneural/plans/p.md");
        idx.insert(".aneural/icebox/ideas.md");

        let plan = b"---\ntitle: Plan A\nstatus: open\ntargets:\n  - apps/web/src/index.ts\n---\nSee [[Architecture]]\n";
        let h = harvest_file(
            &spores,
            &idx,
            ".aneural/plans/p.md",
            std::path::Path::new(".aneural/plans/p.md"),
            plan,
        );
        let plan_node = h.nodes.iter().find(|n| n.kind == "Plan").unwrap();
        assert_eq!(plan_node.id, NodeId::new("plan:.aneural/plans/p.md"));
        assert_eq!(plan_node.label, "Plan A");
        assert_eq!(plan_node.props["status"], "open");
        assert!(
            h.edges
                .iter()
                .any(|e| e.kind == "ANNOTATES" && e.dst == NodeId::file("apps/web/src/index.ts"))
        );
        assert!(h.edges.iter().any(|e| e.kind == "RELATES_TO"
            && e.src == plan_node.id
            && e.dst == NodeId::file(".aneural/notes/Architecture.md")));
        // the generic links harvester also links the plan *file* to Architecture
        assert!(
            h.edges
                .iter()
                .any(|e| e.src == NodeId::file(".aneural/plans/p.md")
                    && e.dst == NodeId::file(".aneural/notes/Architecture.md"))
        );

        let ice =
            b"# Icebox\n\n## Replace router\nSee [[README]].\n\n## Rate limit\n\n## Rate limit\n";
        let h = harvest_file(
            &spores,
            &idx,
            ".aneural/icebox/ideas.md",
            std::path::Path::new(".aneural/icebox/ideas.md"),
            ice,
        );
        let ideas: Vec<_> = h.nodes.iter().filter(|n| n.kind == "Idea").collect();
        assert_eq!(ideas.len(), 3);
        assert_eq!(
            ideas[0].id,
            NodeId::new("idea:.aneural/icebox/ideas.md#replace-router")
        );
        assert_eq!(ideas[2].label, "Rate limit (2)");
        assert!(h.edges.iter().any(|e| e.src == ideas[0].id
            && e.dst == NodeId::file("README.md")
            && e.props["via"] == "wikilink"));

        let note = b"# Architecture\n\nBack to [[README]] and [[Missing]].\n";
        let h = harvest_file(
            &spores,
            &idx,
            ".aneural/notes/Architecture.md",
            std::path::Path::new(".aneural/notes/Architecture.md"),
            note,
        );
        let n = h.nodes.iter().find(|n| n.kind == "Note").unwrap();
        assert_eq!(n.label, "Architecture");
        assert_eq!(
            h.edges
                .iter()
                .filter(|e| e.props["via"] == "wikilink")
                .count(),
            1
        );
    }

    #[test]
    fn a_link_resolves_by_name_through_a_stale_path_or_an_alias() {
        let mut idx = MarkdownIndex::default();
        idx.insert("README.md");
        idx.insert("docs/Architecture Decision Records.md");
        idx.insert_aliases(
            "docs/Architecture Decision Records.md",
            &["ADR".into(), "Decision Log".into()],
        );

        // Obsidian writes the path it saw at the time; the name is what the
        // author typed, and it is the name that survives a move.
        assert_eq!(
            idx.resolve("../../README", "docs/deep/x.md").as_deref(),
            Some("README.md")
        );
        // An alias is a name the note answers to.
        assert_eq!(
            idx.resolve("adr", "README.md").as_deref(),
            Some("docs/Architecture Decision Records.md")
        );
        assert_eq!(
            idx.resolve("Decision Log", "README.md").as_deref(),
            Some("docs/Architecture Decision Records.md")
        );

        // A real file of that name always beats somebody else's alias.
        idx.insert("notes/ADR.md");
        assert_eq!(
            idx.resolve("ADR", "README.md").as_deref(),
            Some("notes/ADR.md")
        );

        // Re-declaring replaces: the old alias stops resolving.
        idx.insert_aliases(
            "docs/Architecture Decision Records.md",
            &["Decisions".into()],
        );
        assert_eq!(idx.resolve("Decision Log", "README.md"), None);
        assert_eq!(
            idx.resolve("Decisions", "README.md").as_deref(),
            Some("docs/Architecture Decision Records.md")
        );

        // And a deleted note takes its aliases with it.
        idx.remove("docs/Architecture Decision Records.md");
        assert_eq!(idx.resolve("Decisions", "README.md"), None);
    }

    #[test]
    fn a_link_to_a_note_nobody_wrote_is_kept_as_a_placeholder() {
        let spores = builtins();
        let mut idx = MarkdownIndex::default();
        idx.insert("README.md");
        idx.insert(".aneural/notes/Architecture.md");

        let note = b"# Architecture\n\nSee [[README]] and [[Not Written Yet]].\n";
        let h = harvest_file(
            &spores,
            &idx,
            ".aneural/notes/Architecture.md",
            std::path::Path::new(".aneural/notes/Architecture.md"),
            note,
        );

        let missing: Vec<_> = h.nodes.iter().filter(|n| n.kind == "MissingNote").collect();
        assert_eq!(
            missing.len(),
            1,
            "one per distinct target, not one per link"
        );
        let m = missing[0];
        assert_eq!(m.id, NodeId::new("missing:not-written-yet"));
        assert_eq!(m.label, "Not Written Yet");
        assert_eq!(m.props["target"], "Not Written Yet");
        // Recognisable as a placeholder without knowing the spore or the kind.
        assert_eq!(m.props["unresolved"], true);
        // Shared by every file that asks for it, so owned by none of them.
        assert!(
            m.origin.is_none(),
            "a placeholder must outlive the file that named it"
        );

        assert!(
            h.edges
                .iter()
                .any(|e| e.dst == m.id && e.props["via"] == "unresolved")
        );
        // The link that did resolve is unaffected and still points at the file.
        assert!(
            h.edges
                .iter()
                .any(|e| e.dst == NodeId::file("README.md") && e.props["via"] == "wikilink")
        );
        // No placeholder for a target that exists.
        assert!(!h.nodes.iter().any(|n| n.label == "README"));
    }

    #[test]
    fn frontmatter_tags_become_one_shared_node_each() {
        let spores = builtins();
        let mut idx = MarkdownIndex::default();
        idx.insert("Data Sources/FDA 510k.md");

        // The shape every note in a real vault uses.
        let note = b"---\ntags: [data-source, medical-device, FDA]\n---\n# FDA 510k\n";
        let h = harvest_file(
            &spores,
            &idx,
            "Data Sources/FDA 510k.md",
            std::path::Path::new("Data Sources/FDA 510k.md"),
            note,
        );

        let tags: Vec<&Node> = h.nodes.iter().filter(|n| n.kind == "Tag").collect();
        let mut ids: Vec<&str> = tags.iter().map(|n| n.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["tag:data-source", "tag:fda", "tag:medical-device"]);

        // The id is slugged so `FDA` and `fda` are one tag, but the label keeps
        // what the author typed.
        let fda = tags.iter().find(|n| n.id.as_str() == "tag:fda").unwrap();
        assert_eq!(fda.label, "FDA");
        assert_eq!(fda.props["tag"], "FDA");
        assert!(
            fda.origin.is_none(),
            "a tag is shared by every file carrying it"
        );

        for id in ids {
            assert!(
                h.edges
                    .iter()
                    .any(|e| e.src == NodeId::file("Data Sources/FDA 510k.md")
                        && e.dst.as_str() == id
                        && e.props["via"] == "tag"),
                "{id} is joined to the file that carries it"
            );
        }
    }

    #[test]
    fn a_tag_list_is_read_however_it_was_written() {
        let spores = builtins();
        let idx = MarkdownIndex::default();
        let tags_of = |body: &str| {
            let h = harvest_file(
                &spores,
                &idx,
                "n.md",
                std::path::Path::new("n.md"),
                body.as_bytes(),
            );
            let mut v: Vec<String> = h
                .nodes
                .iter()
                .filter(|n| n.kind == "Tag")
                .map(|n| n.id.to_string())
                .collect();
            v.sort();
            v
        };

        let want = vec!["tag:a".to_string(), "tag:b".to_string()];
        assert_eq!(tags_of("---\ntags: [a, b]\n---\n"), want, "inline");
        assert_eq!(tags_of("---\ntags:\n  - a\n  - b\n---\n"), want, "block");
        assert_eq!(tags_of("---\ntags: a, b\n---\n"), want, "bare");
        assert!(tags_of("---\ntags:\n---\n").is_empty(), "empty key");
        assert!(tags_of("# no frontmatter\n").is_empty());
    }

    #[test]
    fn the_database_spore_harvests_a_real_sqlite_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("app.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));",
        )
        .unwrap();
        drop(conn);

        let spores = builtins();
        let h = harvest_file(&spores, &MarkdownIndex::default(), "app.db", &db, b"");

        let tables: Vec<&str> = h
            .nodes
            .iter()
            .filter(|n| n.kind == "Table")
            .map(|n| n.label.as_str())
            .collect();
        assert_eq!(tables, vec!["posts", "users"]);

        let posts = h.nodes.iter().find(|n| n.label == "posts").unwrap();
        assert_eq!(posts.props["columns"], "id, user_id");
        assert_eq!(posts.props["rowCount"], 0);
        assert!(posts.props.get("sample").is_none(), "rows stay opt-in");
        assert_eq!(posts.origin.as_deref(), Some("app.db"));

        assert!(h.edges.iter().any(|e| e.kind == "REFERENCES"
            && e.src == NodeId::new("aneural.database.table:app.db#posts")
            && e.dst == NodeId::new("aneural.database.table:app.db#users")));
    }

    #[test]
    fn markdown_index_prefers_same_dir() {
        let mut idx = MarkdownIndex::default();
        idx.insert("docs/README.md");
        idx.insert("README.md");
        assert_eq!(
            idx.resolve("readme", "docs/x.md").as_deref(),
            Some("docs/README.md")
        );
        assert_eq!(
            idx.resolve("README", "src/x.md").as_deref(),
            Some("README.md")
        );
        idx.remove("README.md");
        assert_eq!(
            idx.resolve("README", "src/x.md").as_deref(),
            Some("docs/README.md")
        );
    }
}
