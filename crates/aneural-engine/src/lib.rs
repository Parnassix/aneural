//! Aneural indexing engine: walks a workspace, parses manifests and imports,
//! runs spore harvesters, persists to the SQLite store and streams
//! [`GraphDelta`]s to whoever is listening (the GUI, the CLI, the MCP server).

pub mod analysis;
pub mod doctor;
pub mod history;
pub mod runs;
pub mod spores;
pub mod walk;
pub mod watch;

pub use aneural_core;
pub use aneural_lang;
pub use aneural_store;
pub use doctor::Diagnostic;

use aneural_core::config::{Config, NodeTypeDef};
use aneural_core::graph::{DeltaPhase, IndexStats};
use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::net::{EnvSecrets, Fetcher, SecretStore};
use aneural_core::spore::{Harvester, SporeInfo};
use aneural_core::{Edge, GraphDelta, Node, NodeId, Workspace};
use aneural_lang::Resolver;
use aneural_store::{EdgeQuery, FileRecord, NodeQuery, Store};
use crossbeam_channel::{Receiver, Sender};
use globset::GlobSet;
use serde::{Deserialize, Serialize};
use spores::{MarkdownIndex, Spore};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walk::Entry;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] aneural_core::Error),
    #[error(transparent)]
    Store(#[from] aneural_store::Error),
    #[error("walk: {0}")]
    Walk(#[from] ignore::Error),
    #[error("watch: {0}")]
    Watch(#[from] notify::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Events streamed by the engine.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    Delta(GraphDelta),
    /// The installed spores, emitted on open and after any change to them, so
    /// the GUI never has to hold an `Engine` of its own to see them.
    Spores {
        spores: Vec<SporeInfo>,
        errors: Vec<String>,
    },
    Progress {
        phase: &'static str,
        done: u64,
        total: u64,
    },
    IndexComplete(IndexStats),
    Watching,
    Error(String),
}

/// Commands accepted by [`Engine::watch_loop`].
#[derive(Clone, Debug)]
pub enum EngineCommand {
    /// Re-run a full index (respecting fingerprints unless `force`).
    Reindex {
        force: bool,
    },
    /// Re-read the config and reload spores from disk, then report them. Used
    /// after the marketplace installs or removes one.
    ReloadSpores,
    /// Turn one spore on or off and converge the graph to match.
    SetSporeEnabled {
        id: String,
        on: bool,
    },
    /// Re-fetch HTTP harvesters now. `id` narrows it to one spore; `force`
    /// ignores the refresh interval, which is what a user pressing a button
    /// means.
    RefreshHttp {
        id: Option<String>,
        force: bool,
    },
    Stop,
}

/// What one pass of the HTTP refresh loop did.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefreshStats {
    pub harvesters: u64,
    pub skipped: u64,
    pub nodes: u64,
    pub edges: u64,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarvestStats {
    pub files: u64,
    pub nodes: u64,
    pub edges: u64,
}

pub struct Engine {
    ws: Workspace,
    config: Config,
    store: Store,
    resolver: Resolver,
    spores: Vec<Spore>,
    spore_errors: Vec<String>,
    ignore: GlobSet,
    md_index: MarkdownIndex,
    repos: BTreeSet<String>,
    /// Supplied by the host binary. `None` means this program does not make web
    /// requests, and every HTTP harvester reports that rather than failing
    /// silently — see `aneural_core::net`.
    fetcher: Option<Box<dyn Fetcher>>,
    secrets: Box<dyn SecretStore>,
    /// When each HTTP harvester may next run, keyed by its synthetic origin.
    next_refresh: HashMap<String, Instant>,
    /// When history was last read, and whether a session was being written to
    /// at the time — which is what decides how soon to look again.
    last_history: Option<Instant>,
    history_is_hot: bool,
    /// The run index, opened the first time a run has to be recorded and not
    /// before: a workspace that never turns `runs` on grows no `runs.db`.
    runs_db: Option<runs::store::RunStore>,
    /// What `launchctl list` last said, and when. See [`LAUNCHCTL_FLOOR`].
    last_loaded: Option<(Instant, BTreeMap<String, runs::launchd::Loaded>)>,
}

const BATCH: usize = 200;

/// How often to re-read history while a session is being written to.
const HISTORY_HOT: Duration = Duration::from_secs(1);
/// And how often when nothing is happening.
const HISTORY_IDLE: Duration = Duration::from_secs(30);
/// A transcript touched this recently counts as a session in progress.
const HISTORY_HOT_WINDOW: Duration = Duration::from_secs(60);

/// How often `launchctl list` may be asked, at most.
///
/// `runs` rides the history tick, which runs once a second while a Claude
/// session is being written to — so without a floor this forks a subprocess
/// every second for the length of a session. Measured at 6 ms and 525 lines on
/// this machine, which is not a crisis but is entirely pointless: what it is
/// read for is a pid and a last exit status behind log files whose mtimes
/// cannot move faster than the job writes them.
const LAUNCHCTL_FLOOR: Duration = Duration::from_secs(10);

impl Engine {
    /// Open the engine on a workspace, using the on-disk cache.
    pub fn open(ws: Workspace) -> Result<Self> {
        // before the cache exists, so it is never there unignored
        ws.ensure_ignored()?;
        let store = Store::open(&ws.db_path())?;
        Self::with_store(ws, store)
    }

    /// Open with an in-memory store (tests, one-shot queries).
    pub fn open_in_memory(ws: Workspace) -> Result<Self> {
        Self::with_store(ws, Store::open_in_memory()?)
    }

    fn with_store(ws: Workspace, store: Store) -> Result<Self> {
        let config = ws.load_config()?;
        let resolver = Resolver::new(ws.root(), &config.typescript);
        let (spores, errs) = spores::load_all(&ws, &config.spores.enabled);
        let ignore = spores::globset(&config.ignore)
            .map_err(|e| Error::Other(format!("config.ignore: {e}")))?;
        let mut engine = Engine {
            ws,
            config,
            store,
            resolver,
            spores,
            spore_errors: errs.iter().map(|e| e.to_string()).collect(),
            ignore,
            md_index: MarkdownIndex::default(),
            repos: BTreeSet::new(),
            fetcher: None,
            secrets: Box::new(EnvSecrets),
            next_refresh: HashMap::new(),
            last_history: None,
            history_is_hot: false,
            runs_db: None,
            last_loaded: None,
        };
        engine.warm_from_store()?;
        Ok(engine)
    }

    /// Rebuild the in-memory helpers (repo set, markdown index) from the cache.
    fn warm_from_store(&mut self) -> Result<()> {
        for n in self.store.query_nodes(&NodeQuery {
            kinds: vec![NodeKind::REPO.into()],
            ..Default::default()
        })? {
            if let Some(p) = n.path {
                self.repos.insert(p);
            }
        }
        for rec in self.store.all_files()? {
            if rec.lang.as_deref() == Some("markdown") {
                let path = rec.path.clone();
                self.index_markdown(&path);
            }
        }
        Ok(())
    }

    /// Put a markdown file in the wiki-link index, under its filename and
    /// under any `aliases:` its frontmatter declares.
    ///
    /// A vault links by name, and a note may answer to several — resolving
    /// only the filename silently loses every link written to an alias.
    fn index_markdown(&mut self, rel: &str) {
        self.md_index.insert(rel);
        let aliases = read_head(&self.ws.abs(rel), FRONTMATTER_PEEK)
            .map(|head| spores::markdown::declared_aliases(&head))
            .unwrap_or_default();
        self.md_index.insert_aliases(rel, &aliases);
    }

    /// Node kinds enabled spores emit without an origin — link placeholders,
    /// tags. They belong to no file, so nothing else will ever collect them.
    fn shared_kinds(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for sp in self.spores.iter().filter(|s| s.enabled) {
            for kind in sp.shared_node_kinds() {
                if !out.contains(&kind) {
                    out.push(kind);
                }
            }
        }
        out
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }
    pub fn spore_errors(&self) -> &[String] {
        &self.spore_errors
    }

    pub fn spores(&self) -> Vec<SporeInfo> {
        self.spores
            .iter()
            .map(|sp| {
                let settings = self
                    .config
                    .spores
                    .settings_for(&sp.manifest.id(), &sp.manifest.name);
                sp.info_with(&settings)
            })
            .collect()
    }

    /// Builtin + spore + workspace node types with config overrides applied.
    pub fn node_types(&self) -> Result<Vec<NodeTypeDef>> {
        let extra: Vec<NodeTypeDef> = self
            .spores
            .iter()
            .filter(|s| s.enabled)
            .flat_map(Spore::node_types)
            .collect();
        Ok(self.ws.compose_node_types(&self.config, extra)?)
    }

    // ---- indexing ---------------------------------------------------------

    /// Full index. Emits every node/edge (so a fresh consumer sees the whole
    /// graph), skipping analysis of unchanged files unless `force`.
    pub fn index_full(
        &mut self,
        force: bool,
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<IndexStats> {
        let started = Instant::now();
        let mut stats = IndexStats::default();
        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut files: Vec<Entry> = Vec::new();
        self.repos.clear();

        // Phase 1: structure
        let mut delta = GraphDelta::new(DeltaPhase::Initial);
        let roots: Vec<PathBuf> = self.config.roots.iter().map(|r| self.ws.abs(r)).collect();
        for root in roots {
            for result in walk::walker(&self.ws, &self.config, &root)? {
                let dent = match result {
                    Ok(d) => d,
                    Err(e) => {
                        sink(EngineEvent::Error(e.to_string()));
                        continue;
                    }
                };
                let Some(entry) = walk::entry_for(&self.ws, dent.path()) else {
                    continue;
                };
                if entry.is_repo {
                    self.repos.insert(entry.rel.clone());
                }
                seen_paths.insert(entry.rel.clone());
                let repo = walk::repo_for(&entry.rel, &self.repos);
                delta.nodes.push(walk::node_for(
                    &self.ws,
                    &self.config,
                    &entry,
                    repo.as_ref(),
                ));
                if let Some(e) = walk::contains_edge_for(&entry.rel, entry.is_dir) {
                    delta.edges.push(e);
                }
                if !entry.is_dir {
                    if is_markdown(&entry.rel) {
                        let rel = entry.rel.clone();
                        self.index_markdown(&rel);
                    }
                    files.push(entry);
                }
                if delta.nodes.len() >= BATCH {
                    self.flush(&mut delta, sink)?;
                    sink(EngineEvent::Progress {
                        phase: "walk",
                        done: seen_paths.len() as u64,
                        total: 0,
                    });
                }
            }
        }
        self.flush(&mut delta, sink)?;
        stats.files_scanned = files.len() as u64;

        // Phase 2: content (manifests, imports, spores)
        let total = files.len() as u64;
        for (i, entry) in files.iter().enumerate() {
            match self.process_file(entry, force)? {
                Processed::Indexed(d) => {
                    stats.files_indexed += 1;
                    self.emit(d, sink);
                }
                Processed::Unchanged(d) => {
                    stats.files_skipped += 1;
                    self.emit(d, sink);
                }
            }
            if i % 50 == 0 {
                sink(EngineEvent::Progress {
                    phase: "analyze",
                    done: i as u64 + 1,
                    total,
                });
            }
        }

        // Phase 3: prune what disappeared since the last run
        let mut removed = GraphDelta::new(DeltaPhase::Initial);
        for rec in self.store.all_files()? {
            if !seen_paths.contains(&rec.path) {
                let d = self.remove_path(&rec.path)?;
                removed.merge(d);
            }
        }
        let fs_kinds = vec![
            NodeKind::DIRECTORY.into(),
            NodeKind::REPO.into(),
            NodeKind::FILE.into(),
            NodeKind::MANIFEST.into(),
        ];
        let stale: Vec<NodeId> = self
            .store
            .query_nodes(&NodeQuery {
                kinds: fs_kinds,
                ..Default::default()
            })?
            .into_iter()
            .filter(|n| n.path.as_ref().is_none_or(|p| !seen_paths.contains(p)))
            .map(|n| n.id)
            .collect();
        if !stale.is_empty() {
            self.store.delete_nodes(&stale)?;
            removed.removed_node_ids.extend(stale);
        }
        let kinds = self.shared_kinds();
        removed
            .removed_node_ids
            .extend(self.store.gc_orphan_by_kind(&kinds)?);
        for id in self.store.gc_orphan_packages()? {
            removed.removed_node_ids.push(id);
        }
        removed.initial_complete = true;
        sink(EngineEvent::Delta(removed));

        // After the files exist, so that a commit's edges have somewhere to
        // land — `to_graph` drops any edge whose file is not in the graph yet.
        self.refresh_history(true, sink)?;

        let counts = self.store.counts()?;
        stats.nodes = counts.nodes;
        stats.edges = counts.edges;
        stats.unresolved = counts.unresolved;
        stats.duration_ms = started.elapsed().as_millis() as u64;
        self.store
            .meta_set("last_index_at", &aneural_core::now_rfc3339())?;
        self.store
            .meta_set("engine_version", env!("CARGO_PKG_VERSION"))?;
        sink(EngineEvent::IndexComplete(stats.clone()));
        Ok(stats)
    }

    /// Re-read every repository's history and converge the graph on it.
    ///
    /// Each repository owns one origin, so one of them changing does not
    /// disturb the others, and a repository that disappears is retracted by the
    /// sweep at the end rather than lingering. Commits that reached nothing we
    /// index are collected: they would otherwise float unattached forever,
    /// which no existing rule covers.
    pub fn refresh_git(&mut self, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        if !self.config.history.git {
            return Ok(());
        }
        let max = self.config.history.max_commits as usize;
        let repos: Vec<String> = self.repos.iter().cloned().collect();

        let mut live = HashSet::new();
        for repo_rel in &repos {
            let abs = self.ws.abs(repo_rel);
            let commits = match aneural_git::history(&abs, max) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("git history for {repo_rel}: {e}");
                    continue;
                }
            };
            let (nodes, edges) = {
                let store = &self.store;
                let known = |id: &NodeId| store.get_node(id).ok().flatten().is_some();
                history::git::to_graph(repo_rel, &commits, &known)
            };
            let origin = history::git_origin(repo_rel);
            live.insert(origin.clone());
            let delta = self.store.replace_origin(&origin, &nodes, &edges)?;
            self.emit(delta, sink);
        }

        // Origins from repositories that are no longer here.
        let mut removed = GraphDelta::new(DeltaPhase::Live);
        for origin in self.store.origins_with_prefix(history::GIT_PREFIX)? {
            if !live.contains(&origin) {
                removed.merge(self.store.delete_origin(&origin)?);
            }
        }
        self.emit(removed, sink);
        Ok(())
    }

    /// Read this workspace's Claude Code sessions and converge the graph.
    ///
    /// Off unless the workspace asks for it: these transcripts hold everything
    /// that was said in a session, so the default is the consent. Turning it
    /// back off retracts what it wrote rather than merely stopping.
    pub fn refresh_claude(&mut self, full: bool, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        if !self.config.history.claude {
            let delta = self.store.delete_by_source(Source::CLAUDE)?;
            if !delta.is_empty() {
                for origin in self.store.origins_with_prefix(history::claude::PREFIX)? {
                    self.store.delete_origin(&origin)?;
                }
                self.store.meta_delete_prefix("claude.session.")?;
                self.emit(delta, sink);
            }
            return Ok(());
        }
        let Some(state_dir) = history::claude::state_dir(&self.config.history.claude_root) else {
            return Ok(());
        };

        let mut live = HashSet::new();
        for path in history::claude::transcripts_for(&state_dir, self.ws.root()) {
            let Some(uuid) = history::claude::uuid_of(&path) else {
                continue;
            };
            let key = history::claude::state_key(&uuid);
            live.insert(history::claude::session_origin(&uuid));

            let mut state: history::claude::State = self
                .store
                .meta_get(&key)?
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default();
            state.session.uuid = uuid.clone();

            match history::claude::advance(&path, &mut state) {
                // Nothing new to read. A full index still has to *emit* what it
                // knows — a fresh consumer builds its whole graph from the
                // deltas, exactly as an unchanged file is replayed from the
                // cache rather than skipped.
                Ok(None) => {
                    if full {
                        let origin = history::claude::session_origin(&uuid);
                        self.emit(self.cached_origin(&origin)?, sink);
                    }
                    continue;
                }
                Ok(Some(())) => {}
                Err(e) => {
                    tracing::debug!("claude transcript {}: {e}", path.display());
                    continue;
                }
            }

            let (node, edges) = {
                let (ws, store) = (&self.ws, &self.store);
                // A path a transcript recorded is absolute, or relative to the
                // directory that session ran in — which is not always the
                // workspace root.
                let base = state
                    .session
                    .cwd
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| ws.root().to_path_buf());
                let rel = |p: &str| {
                    let path = Path::new(p);
                    let abs = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        base.join(path)
                    };
                    ws.rel(&abs)
                };
                let known = |id: &NodeId| store.get_node(id).ok().flatten().is_some();
                history::claude::to_graph(&state.session, &rel, &known)
            };

            let mut delta = GraphDelta::new(DeltaPhase::Live);
            delta.nodes.push(node);
            delta.edges = edges;
            let raw = serde_json::to_string(&state)
                .map_err(|e| Error::Other(format!("claude state: {e}")))?;
            self.store.apply_delta_with_meta(&delta, &[(&key, &raw)])?;
            self.emit(delta, sink);
        }

        // Transcripts that are gone: retract the session and forget the offset.
        let mut removed = GraphDelta::new(DeltaPhase::Live);
        for origin in self
            .store
            .origins_with_prefix(history::claude::SESSION_PREFIX)?
        {
            if live.contains(&origin) {
                continue;
            }
            removed.merge(self.store.delete_origin(&origin)?);
            if let Some(uuid) = origin.rsplit('/').next() {
                self.store.meta_delete(&history::claude::state_key(uuid))?;
            }
        }
        self.emit(removed, sink);
        Ok(())
    }

    /// How long until history is worth re-reading, or `None` when neither
    /// producer is on and the engine should not wake for this at all.
    ///
    /// Two speeds. A session written to in the last minute is being used right
    /// now, and that is the case worth following closely; everything else can
    /// wait. Checking costs a `stat` per transcript, so the fast rate is
    /// affordable — it is the parsing that is expensive, and a transcript that
    /// has not grown is never opened.
    /// What this machine is scheduled to run, and whether that matches what the
    /// workspace says it should.
    ///
    /// Shaped like [`Engine::refresh_usage`]: gate at the top, retract on the
    /// off branch, gather, `replace_origin`, emit. The declarations are read
    /// back off the graph rather than re-parsed, because the spore has already
    /// read them and a second reading could disagree with the first.
    pub fn refresh_runs(&mut self, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        if !self.config.runs.enabled {
            for origin in self.store.origins_with_prefix(runs::PREFIX)? {
                let delta = self.store.delete_origin(&origin)?;
                self.emit(delta, sink);
            }
            // The index goes too, but the journal never does: it is committed
            // history, and switching a producer off is not a reason to throw
            // away what a machine has already recorded.
            self.runs_db = None;
            return Ok(());
        }

        let schedules = self.store.query_nodes(&NodeQuery {
            kinds: vec![NodeKind::SCHEDULE.into()],
            ..Default::default()
        })?;

        // Which scripts each declaration reaches, from the edges it drew.
        let mut reaches: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in self.store.get_edges(&EdgeQuery {
            kinds: vec![EdgeKind::ANNOTATES.into()],
            ..Default::default()
        })? {
            if edge.dst.prefix() == "script" {
                reaches.entry(edge.src).or_default().push(edge.dst);
            }
        }
        let declared = runs::declared_from(&schedules, &|id| {
            reaches.get(id).cloned().unwrap_or_default()
        });

        let installed = match self.launch_agents_dir() {
            // Ask launchd what is loaded only when reading the directory launchd
            // actually reads. Pointed somewhere else, those agents are files on
            // disk that launchd has never seen, and reporting one as "loaded"
            // because a label of the same name happens to be running elsewhere
            // would be a straight falsehood.
            Some((dir, true)) => {
                let fresh = self
                    .last_loaded
                    .as_ref()
                    .is_none_or(|(at, _)| at.elapsed() >= LAUNCHCTL_FLOOR);
                if fresh {
                    self.last_loaded = Some((Instant::now(), runs::launchd::loaded()));
                }
                let loaded = self
                    .last_loaded
                    .as_ref()
                    .map(|(_, l)| l.clone())
                    .unwrap_or_default();
                runs::launchd::installed(&dir, self.ws.root(), &loaded)
            }
            Some((dir, false)) => {
                runs::launchd::installed(&dir, self.ws.root(), &Default::default())
            }
            None => Vec::new(),
        };

        let now = time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
            .date();
        let (nodes, edges) = runs::to_graph(&declared, &installed, now, &self.config.runs.cadences);

        // Two origins, so re-reading what is installed does not disturb the
        // verdicts and vice versa.
        for origin in [runs::INSTALLED_ORIGIN, runs::VERDICT_ORIGIN] {
            let mine: Vec<Node> = nodes
                .iter()
                .filter(|n| n.origin.as_deref() == Some(origin))
                .cloned()
                .collect();
            let ours: Vec<Edge> = edges
                .iter()
                .filter(|e| e.origin.as_deref() == Some(origin))
                .cloned()
                .collect();
            let delta = self.store.replace_origin(origin, &mine, &ours)?;
            self.emit(delta, sink);
        }

        self.refresh_latest_runs(&installed, now, sink)
    }

    /// The most recent run of each script: observe, record, index, publish.
    ///
    /// Observation is the *only* source here. Nothing in this method starts a
    /// process; it reads the traces of runs that happened anyway, which is why it
    /// is on the same gate as reading what is installed rather than behind
    /// [`RunsConfig::execute`](aneural_core::config::RunsConfig::execute).
    fn refresh_latest_runs(
        &mut self,
        installed: &[runs::Installed],
        now: time::Date,
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<()> {
        let machine = runs::journal::machine_id(&self.config.runs.machine);
        let dir = self.ws.runs_dir();
        let keep_days = self.config.runs.keep_days;
        let keep_per_script = self.config.runs.keep_per_script;
        let attributes = self.ws.gitattributes_path();
        let observed = runs::launchd::observe(installed, &machine);

        let latest = {
            let db = match self.runs_db {
                Some(ref mut db) => db,
                None => {
                    let db = runs::store::RunStore::open(&self.ws.runs_db_path())?;
                    self.runs_db.insert(db)
                }
            };
            // Read the journal **before** deciding anything is new. The index is
            // a cache and can be missing, and asking a cache "have you seen
            // this?" before it has caught up answers no — which would append a
            // duplicate line to a committed file every time the cache was lost.
            let ingested = db.ingest(&dir)?;
            for path in &ingested.reingested {
                tracing::debug!("run journal rewritten, re-read in full: {}", path.display());
            }

            // Now only what the journal does not already hold. The id is derived
            // from the instant observed, so re-observing an unchanged agent
            // produces the same id, and this is what turns that into "nothing to
            // write" rather than "the same line again".
            let mut appended = false;
            for record in &observed {
                if db.has(&record.run_id)? {
                    continue;
                }
                aneural_core::workspace::write_gitattributes(&attributes)?;
                runs::journal::append(&dir, record)?;
                appended = true;
            }
            if appended {
                db.ingest(&dir)?;
            }

            // Retention, in the order that keeps the index honest: drop whole
            // segments first, then forget the rows that came out of them, and
            // only then bound what is left per script.
            for path in runs::journal::prune(&dir, keep_days, now) {
                tracing::debug!("run journal segment retired: {}", path.display());
            }
            db.forget_missing_segments(&dir)?;
            db.prune(keep_per_script)?;
            db.latest_per_script()?
        };

        // A run whose script is not on the canvas would be a node with nothing
        // to grow off — the `scripts` spore is off, or the script was deleted.
        // The record stays in the journal either way; only the node is withheld.
        let mut live: Vec<runs::journal::Record> = Vec::new();
        for record in latest {
            if self
                .store
                .get_node(&runs::script_id(&record.script_key))?
                .is_some()
            {
                live.push(record);
            }
        }
        let (nodes, edges) = runs::runs_to_graph(&live);

        let mut wanted: Vec<String> = Vec::new();
        for node in &nodes {
            let Some(origin) = node.origin.clone() else {
                continue;
            };
            let ours: Vec<Edge> = edges
                .iter()
                .filter(|e| e.origin.as_deref() == Some(origin.as_str()))
                .cloned()
                .collect();
            let delta = self
                .store
                .replace_origin(&origin, std::slice::from_ref(node), &ours)?;
            self.emit(delta, sink);
            wanted.push(origin);
        }
        // Scripts that had a Run node and no longer do.
        for origin in self.store.origins_with_prefix(runs::LATEST_PREFIX)? {
            if !wanted.contains(&origin) {
                let delta = self.store.delete_origin(&origin)?;
                self.emit(delta, sink);
            }
        }
        Ok(())
    }

    /// Where to read launch agents from, and whether it is the directory launchd
    /// itself reads. `None` when there is nowhere to look, which is every
    /// platform but macOS unless the workspace names a directory.
    fn launch_agents_dir(&self) -> Option<(std::path::PathBuf, bool)> {
        let configured = self.config.runs.launch_agents_root.trim();
        if !configured.is_empty() {
            return Some((std::path::PathBuf::from(configured), false));
        }
        if !cfg!(target_os = "macos") {
            return None;
        }
        std::env::var_os("HOME").map(|h| {
            (
                std::path::Path::new(&h)
                    .join("Library")
                    .join("LaunchAgents"),
                true,
            )
        })
    }

    fn next_history_due(&self) -> Option<Duration> {
        // `runs` rides the history tick rather than adding a third timer: its
        // work is a few queries and one directory read, and a workspace with
        // history off but schedules on still has to be told what is due.
        if !self.config.history.git && !self.config.history.claude && !self.config.runs.enabled {
            return None;
        }
        let due = match self.last_history {
            Some(at) => self
                .history_period()
                .checked_sub(at.elapsed())
                .unwrap_or_default(),
            None => Duration::ZERO,
        };
        Some(due.max(Duration::from_secs(1)))
    }

    fn history_period(&self) -> Duration {
        if self.history_is_hot {
            HISTORY_HOT
        } else {
            HISTORY_IDLE
        }
    }

    /// One pass of both producers plus the join between them.
    pub fn refresh_history(&mut self, full: bool, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        // Timed per step. This runs once a second for as long as a session is
        // being written to, and the GUI's live view leans on that rate, so a
        // producer that quietly grew expensive needs to show up as a number
        // here rather than as a warm laptop. `tracing` evaluates the fields
        // only if the level is on, so an unset `RUST_LOG` pays for the laps and
        // nothing else.
        let mut laps: Vec<(&'static str, u128)> = Vec::with_capacity(7);
        let mut at = Instant::now();
        macro_rules! lap {
            ($name:literal) => {{
                laps.push(($name, at.elapsed().as_micros()));
                #[allow(unused_assignments)]
                {
                    at = Instant::now();
                }
            }};
        }
        self.refresh_git(sink)?;
        lap!("git");
        self.refresh_claude(full, sink)?;
        lap!("claude");
        self.link_sessions(sink)?;
        lap!("links");
        self.refresh_plans(sink)?;
        lap!("plans");
        // Before the tallies, after every edge has been drawn: a session that
        // approved a plan but edited nothing still belongs in the graph, and
        // until the plans are read there is nothing holding it there.
        let mut swept = GraphDelta::new(DeltaPhase::Live);
        swept.removed_node_ids = self.store.gc_disconnected_history()?;
        self.emit(swept, sink);
        lap!("sweep");
        // Last of all, so it can see which sessions survived that sweep and
        // say how much of its total came from ones that did not.
        self.refresh_usage(sink)?;
        lap!("usage");
        // After the spore has harvested, because it reads the declarations off
        // the graph rather than re-reading the files they came from.
        self.refresh_runs(sink)?;
        lap!("runs");
        self.last_history = Some(Instant::now());
        self.history_is_hot = self.a_session_is_live();
        lap!("live?");
        let total: u128 = laps.iter().map(|(_, us)| us).sum();
        tracing::debug!(
            "history tick {}ms: {}",
            total / 1000,
            laps.iter()
                .filter(|(_, us)| *us > 500)
                .map(|(name, us)| format!("{name} {}ms", us / 1000))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(())
    }

    /// Whether any transcript for this workspace was written to just now.
    fn a_session_is_live(&self) -> bool {
        if !self.config.history.claude {
            return false;
        }
        let Some(state_dir) = history::claude::state_dir(&self.config.history.claude_root) else {
            return false;
        };
        history::claude::transcripts_for(&state_dir, self.ws.root())
            .iter()
            .any(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|since| since < HISTORY_HOT_WINDOW)
            })
    }

    /// Join each commit to the session that produced it.
    ///
    /// Runs after both producers, because the answer depends on all of them:
    /// a commit's trailer names a bridge, and a bridge can span several
    /// sessions, so which one wrote it is decided by when they ran and what
    /// they touched. See [`history::attribute`].
    ///
    /// The edge is `RELATES_TO`, which the GUI draws as a float rather than a
    /// strand — both ends sit off the folder tree, and a commit belongs *near*
    /// its session rather than being wired to it.
    pub fn link_sessions(&mut self, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        // One query for every MODIFIES edge, grouped by source, rather than one
        // query per commit: a busy repository has thousands of them.
        let mut touched: HashMap<NodeId, BTreeSet<String>> = HashMap::new();
        for edge in self.store.get_edges(&aneural_store::EdgeQuery {
            kinds: vec![aneural_core::kinds::EdgeKind::MODIFIES.into()],
            ..Default::default()
        })? {
            touched
                .entry(edge.src)
                .or_default()
                .insert(edge.dst.path_part().to_string());
        }

        let sessions: Vec<history::attribute::Candidate> = self
            .store
            .query_nodes(&NodeQuery {
                kinds: vec![NodeKind::SESSION.into()],
                ..Default::default()
            })?
            .into_iter()
            .map(|n| history::attribute::Candidate {
                bridge: n.prop_str("bridge").map(String::from),
                started: n.prop_str("startedAt").unwrap_or_default().to_string(),
                ended: n.prop_str("endedAt").unwrap_or_default().to_string(),
                touched: touched.get(&n.id).cloned().unwrap_or_default(),
                uuid: n.id.path_part().to_string(),
            })
            .collect();

        let mut edges = Vec::new();
        if !sessions.is_empty() {
            for commit in self.store.query_nodes(&NodeQuery {
                kinds: vec![NodeKind::COMMIT.into()],
                ..Default::default()
            })? {
                let subject = history::attribute::Subject {
                    session: commit.prop_str("session").map(String::from),
                    at: commit.prop_str("at").unwrap_or_default().to_string(),
                    changed: touched.get(&commit.id).cloned().unwrap_or_default(),
                };
                let Some(found) = history::attribute::best(&subject, &sessions) else {
                    continue;
                };
                edges.push(
                    Edge::new(
                        aneural_core::kinds::EdgeKind::RELATES_TO,
                        commit.id.clone(),
                        NodeId::session(&found.uuid),
                        Source::CLAUDE,
                    )
                    .with_origin(history::LINK_ORIGIN)
                    .with_prop("via", "trailer"),
                );
            }
        }

        let delta = self
            .store
            .replace_origin(history::LINK_ORIGIN, &[], &edges)?;
        self.emit(delta, sink);
        Ok(())
    }

    /// Read the plans this workspace's sessions approved, and work out how far
    /// each one got.
    ///
    /// Runs last, because a plan's state is read off everything else: which
    /// sessions named it, which commits those sessions produced, and whether
    /// the files involved are still uncommitted.
    pub fn refresh_plans(&mut self, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        let state_dir = match self.config.history.claude {
            true => history::claude::state_dir(&self.config.history.claude_root),
            false => None,
        };
        let Some(state_dir) = state_dir else {
            let delta = self.store.delete_origin(history::plans::ORIGIN)?;
            self.emit(delta, sink);
            return Ok(());
        };

        // Which sessions approved which plan, and which commits came out of
        // those sessions.
        let sessions = self.store.query_nodes(&NodeQuery {
            kinds: vec![NodeKind::SESSION.into()],
            ..Default::default()
        })?;
        let mut approved_by: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        let mut approved_at: BTreeMap<String, String> = BTreeMap::new();
        // What ran under each plan, added up across every session that
        // approved it. A plan picked up again in a later session is one plan
        // with one bill.
        let mut spent_on: BTreeMap<String, history::claude::ByModel> = BTreeMap::new();
        for session in &sessions {
            // When each plan was approved comes from the transcript, not from
            // the session: a session runs for days and approves several plans,
            // so its start time would credit a plan with everything that came
            // before it.
            let uuid = session.id.path_part();
            let Some(raw) = self.store.meta_get(&history::claude::state_key(uuid))? else {
                continue;
            };
            let Ok(state) = serde_json::from_str::<history::claude::State>(&raw) else {
                continue;
            };
            for (name, at) in &state.session.plans {
                approved_by
                    .entry(name.clone())
                    .or_default()
                    .push(session.id.clone());
                let seen = approved_at.entry(name.clone()).or_default();
                if seen.is_empty() || (!at.is_empty() && at.as_str() < seen.as_str()) {
                    *seen = at.clone();
                }
            }
            for (name, by_model) in &state.session.plan_usage {
                history::claude::merge(spent_on.entry(name.clone()).or_default(), by_model);
            }
        }
        if approved_by.is_empty() {
            let delta = self.store.delete_origin(history::plans::ORIGIN)?;
            self.emit(delta, sink);
            return Ok(());
        }

        let commits_of = self.commits_by_session()?;
        let dirty = self.dirty_paths();
        let resolve = self.path_resolver()?;

        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for (name, by) in &approved_by {
            let Some(plan) = history::plans::read(&state_dir, name) else {
                continue;
            };
            let approved = approved_at
                .get(name)
                .map(String::as_str)
                .unwrap_or_default();
            // A session runs for days and approves several plans, so being
            // attributed to the session is not enough: a commit made before the
            // plan existed cannot have carried it out.
            let commits: Vec<NodeId> = by
                .iter()
                .filter_map(|s| commits_of.get(s))
                .flatten()
                .filter(|(_, at)| approved.is_empty() || at.as_str() >= approved)
                .map(|(id, _)| id.clone())
                .collect();
            let state = self.plan_state(&plan, by, &commits, &dirty)?;

            let used = spent_on.get(name).cloned().unwrap_or_default();
            let (node, plan_edges) =
                history::plans::to_graph(&plan, state, approved, by, &used, &resolve);
            edges.extend(plan_edges);
            // A commit realizes the plan its session was working to. The chain
            // is commit → session → plan; this is the shortcut, so the GUI can
            // ask "what carried this out" without walking it.
            for commit in commits {
                edges.push(
                    Edge::new(
                        aneural_core::kinds::EdgeKind::REALIZES,
                        commit,
                        node.id.clone(),
                        Source::CLAUDE,
                    )
                    .with_origin(history::plans::ORIGIN)
                    .with_prop("via", "session"),
                );
            }
            nodes.push(node);
        }

        let delta = self
            .store
            .replace_origin(history::plans::ORIGIN, &nodes, &edges)?;
        self.emit(delta, sink);
        Ok(())
    }

    /// Roll every session's tally up to the repository it ran in, and to the
    /// workspace as a whole.
    ///
    /// Read from the stored session states rather than from the nodes, for the
    /// same reason the counters live off the node in the first place: the
    /// state is what was parsed, and a number recomputed from it cannot drift
    /// away from the transcript it came from.
    pub fn refresh_usage(&mut self, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        let state_dir = match self.config.history.claude {
            true => history::claude::state_dir(&self.config.history.claude_root),
            false => None,
        };
        let Some(state_dir) = state_dir else {
            let delta = self.store.delete_origin(history::usage::ORIGIN)?;
            self.emit(delta, sink);
            return Ok(());
        };

        let mut whole = history::usage::Scope {
            repo: None,
            label: self.config.name.clone(),
            ..Default::default()
        };
        let mut by_repo: BTreeMap<String, history::usage::Scope> = BTreeMap::new();
        for repo in &self.repos {
            by_repo.insert(
                repo.clone(),
                history::usage::Scope {
                    repo: Some(repo.clone()),
                    label: match repo.as_str() {
                        "." => self.config.name.clone(),
                        rel => rel.rsplit('/').next().unwrap_or(rel).to_string(),
                    },
                    ..Default::default()
                },
            );
        }

        // Every transcript, not every Session node: a session that edited
        // nothing and approved nothing has just been swept off the canvas,
        // and it still spent what it spent. Leaving it out would make the
        // total quietly smaller than the bill.
        for path in history::claude::transcripts_for(&state_dir, self.ws.root()) {
            let Some(uuid) = history::claude::uuid_of(&path) else {
                continue;
            };
            let Some(raw) = self.store.meta_get(&history::claude::state_key(&uuid))? else {
                continue;
            };
            let Ok(state) = serde_json::from_str::<history::claude::State>(&raw) else {
                continue;
            };
            let spent = state.session.spent();
            if spent.is_zero() {
                continue;
            }
            let on_canvas = self.store.get_node(&NodeId::session(&uuid))?.is_some();
            whole.add(&state.session, spent, on_canvas);

            // Where it was working, which is the one thing about a session's
            // whereabouts that is a fact rather than an inference.
            let cwd = state
                .session
                .cwd
                .as_deref()
                .and_then(|cwd| self.ws.rel(std::path::Path::new(cwd)))
                .unwrap_or_else(|| ".".into());
            let home = history::usage::repo_of(&cwd, self.repos.iter());
            if let Some(home) = &home
                && let Some(scope) = by_repo.get_mut(home)
            {
                scope.add(&state.session, spent, on_canvas);
            }
            // Every other repository it had its hands in gets the footnote,
            // so a coarse number is at least a visibly coarse one.
            let mut elsewhere: std::collections::BTreeSet<String> = Default::default();
            for path in state.session.touched.keys() {
                let Some(rel) = self
                    .ws
                    .rel(std::path::Path::new(path))
                    .or_else(|| (!path.starts_with('/')).then(|| path.clone()))
                else {
                    continue;
                };
                let Some(touched) = history::usage::repo_of(&rel, self.repos.iter()) else {
                    continue;
                };
                if Some(&touched) != home.as_ref() {
                    elsewhere.insert(touched);
                }
            }
            for repo in elsewhere {
                if let Some(scope) = by_repo.get_mut(&repo) {
                    scope.sessions_elsewhere += 1;
                }
            }
        }

        // A lone repository at the root would otherwise say the same thing
        // twice, in two nodes sitting on top of each other.
        let mut scopes: Vec<history::usage::Scope> = Vec::new();
        let root_is_everything = by_repo.len() == 1 && by_repo.contains_key(".");
        if !root_is_everything {
            scopes.push(whole);
        }
        scopes.extend(by_repo.into_values());

        let (nodes, edges) = history::usage::to_graph(&scopes);
        let delta = self
            .store
            .replace_origin(history::usage::ORIGIN, &nodes, &edges)?;
        self.emit(delta, sink);
        Ok(())
    }

    /// How far a plan got, read off the evidence rather than recorded anywhere.
    fn plan_state(
        &self,
        plan: &history::plans::Plan,
        sessions: &[NodeId],
        commits: &[NodeId],
        dirty: &HashSet<String>,
    ) -> Result<history::plans::State> {
        if !commits.is_empty() {
            return Ok(history::plans::State::Built);
        }
        // Nothing committed yet, but the files a session touched are still
        // uncommitted: work is in flight.
        if !dirty.is_empty() {
            for session in sessions {
                for edge in self.store.get_edges(&aneural_store::EdgeQuery {
                    src: Some(session.clone()),
                    kinds: vec![aneural_core::kinds::EdgeKind::MODIFIES.into()],
                    ..Default::default()
                })? {
                    if dirty.contains(edge.dst.path_part()) {
                        return Ok(history::plans::State::InProgress);
                    }
                }
            }
        }
        let _ = plan;
        Ok(history::plans::State::Proposed)
    }

    /// The commits attributed to each session, with when each was made, from
    /// the links already drawn.
    fn commits_by_session(&self) -> Result<HashMap<NodeId, Vec<(NodeId, String)>>> {
        let mut when: HashMap<NodeId, String> = HashMap::new();
        for commit in self.store.query_nodes(&NodeQuery {
            kinds: vec![NodeKind::COMMIT.into()],
            ..Default::default()
        })? {
            when.insert(
                commit.id.clone(),
                commit.prop_str("at").unwrap_or_default().to_string(),
            );
        }
        let mut out: HashMap<NodeId, Vec<(NodeId, String)>> = HashMap::new();
        for edge in self.store.get_edges(&aneural_store::EdgeQuery {
            kinds: vec![aneural_core::kinds::EdgeKind::RELATES_TO.into()],
            ..Default::default()
        })? {
            if edge.src.prefix() == "commit" && edge.dst.prefix() == "session" {
                let at = when.get(&edge.src).cloned().unwrap_or_default();
                out.entry(edge.dst).or_default().push((edge.src, at));
            }
        }
        Ok(out)
    }

    /// Workspace-relative paths with uncommitted changes, across every repo.
    fn dirty_paths(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        if !self.config.history.git {
            return out;
        }
        for repo_rel in &self.repos {
            let Ok(paths) = aneural_git::dirty(&self.ws.abs(repo_rel)) else {
                continue;
            };
            for path in paths {
                out.insert(match repo_rel.as_str() {
                    "." => path,
                    prefix => format!("{prefix}/{path}"),
                });
            }
        }
        out
    }

    /// Resolve a path a plan named to one in the graph.
    ///
    /// Exact first. Failing that, a bare file name is accepted when exactly one
    /// file in the workspace has it — a plan says `ui.rs:607`, and there is
    /// only one `ui.rs`. Ambiguous names are dropped rather than guessed at.
    fn path_resolver(&self) -> Result<impl Fn(&str) -> Option<String> + use<>> {
        let mut exact: HashSet<String> = HashSet::new();
        let mut by_name: HashMap<String, Option<String>> = HashMap::new();
        for node in self.store.query_nodes(&NodeQuery {
            kinds: vec![NodeKind::FILE.into(), NodeKind::MANIFEST.into()],
            ..Default::default()
        })? {
            let Some(path) = node.path else { continue };
            if let Some(name) = Path::new(&path).file_name().and_then(|n| n.to_str()) {
                by_name
                    .entry(name.to_string())
                    // Seen twice: no longer unambiguous, so no longer usable.
                    .and_modify(|slot| *slot = None)
                    .or_insert_with(|| Some(path.clone()));
            }
            exact.insert(path);
        }
        Ok(move |candidate: &str| {
            let trimmed = candidate.trim_start_matches("./");
            if exact.contains(trimmed) {
                return Some(trimmed.to_string());
            }
            if !trimmed.contains('/') {
                return by_name.get(trimmed).cloned().flatten();
            }
            // A plan often writes a path relative to a package rather than the
            // workspace; accept it when exactly one file ends that way.
            let suffix = format!("/{trimmed}");
            let mut hits = exact.iter().filter(|p| p.ends_with(&suffix));
            match (hits.next(), hits.next()) {
                (Some(only), None) => Some(only.clone()),
                _ => None,
            }
        })
    }

    fn flush(&mut self, delta: &mut GraphDelta, sink: &mut dyn FnMut(EngineEvent)) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        self.store.apply_delta(delta)?;
        let out = std::mem::replace(delta, GraphDelta::new(delta.phase));
        sink(EngineEvent::Delta(out));
        Ok(())
    }

    fn emit(&self, delta: GraphDelta, sink: &mut dyn FnMut(EngineEvent)) {
        if !delta.is_empty() {
            sink(EngineEvent::Delta(delta));
        }
    }

    /// Whether a workspace-relative path is excluded by config or internal.
    pub fn is_ignored(&self, rel: &str) -> bool {
        walk::is_internal(rel)
            || rel.split('/').any(|c| c == ".git")
            || self.ignore.is_match(rel)
            || rel.split('/').any(|c| c == ".DS_Store")
    }

    /// Everything derived from one file's contents.
    fn process_file(&mut self, entry: &Entry, force: bool) -> Result<Processed> {
        let rel = entry.rel.as_str();
        let lang = walk::node_for(&self.ws, &self.config, entry, None)
            .prop_str("lang")
            .map(String::from);
        let record = self.store.file_record(rel)?;
        let unchanged_meta = record
            .as_ref()
            .is_some_and(|r| r.mtime == entry.mtime && r.size == entry.size as i64);
        if unchanged_meta && !force {
            return Ok(Processed::Unchanged(self.cached_delta(rel)?));
        }
        if entry.size > walk::MAX_PARSE_BYTES {
            self.store.upsert_file(&FileRecord {
                path: rel.into(),
                mtime: entry.mtime,
                size: entry.size as i64,
                fingerprint: "skipped".into(),
                lang,
                indexed_at: aneural_core::now_millis(),
            })?;
            // Some harvesters open the file themselves instead of being handed
            // its bytes — a dev database is routinely past this cap and is still
            // worth indexing, because reading its schema costs a few queries.
            if spores::has_large_file_harvester(&self.spores, rel) {
                let h = spores::harvest_large(&self.spores, rel, &entry.abs);
                let repo = walk::repo_for(rel, &self.repos);
                let mut nodes = h.nodes;
                nodes.push(walk::node_for(&self.ws, &self.config, entry, repo.as_ref()));
                let mut delta = self.store.replace_origin(rel, &nodes, &h.edges)?;
                delta.phase = DeltaPhase::Live;
                return Ok(Processed::Indexed(delta));
            }
            return Ok(Processed::Unchanged(GraphDelta::new(DeltaPhase::Live)));
        }
        let bytes = match std::fs::read(&entry.abs) {
            Ok(b) => b,
            Err(_) => return Ok(Processed::Unchanged(GraphDelta::new(DeltaPhase::Live))),
        };
        let fingerprint = blake3::hash(&bytes).to_hex().to_string();
        if !force
            && record
                .as_ref()
                .is_some_and(|r| r.fingerprint == fingerprint)
        {
            self.store.upsert_file(&FileRecord {
                path: rel.into(),
                mtime: entry.mtime,
                size: entry.size as i64,
                fingerprint,
                lang,
                indexed_at: aneural_core::now_millis(),
            })?;
            return Ok(Processed::Unchanged(self.cached_delta(rel)?));
        }

        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();
        let mut unresolved = Vec::new();

        if let Some(l) = lang.as_deref()
            && l != "markdown"
            && self.config.language_enabled(l)
        {
            let a = analysis::analyze(&self.ws, &self.resolver, rel, &entry.abs, &bytes);
            nodes.extend(a.nodes);
            edges.extend(a.edges);
            unresolved = a.unresolved;
        }
        if self.spores.iter().any(|s| s.enabled && s.applies_to(rel)) {
            let h = spores::harvest_file(&self.spores, &self.md_index, rel, &entry.abs, &bytes);
            nodes.extend(h.nodes);
            edges.extend(h.edges);
        }
        // refresh the file node's fingerprint + repo
        let repo = walk::repo_for(rel, &self.repos);
        let mut file_node = walk::node_for(&self.ws, &self.config, entry, repo.as_ref());
        file_node.fingerprint = Some(fingerprint.clone());
        nodes.push(file_node);

        let mut delta = self.store.replace_origin(rel, &nodes, &edges)?;
        delta.phase = DeltaPhase::Live;
        self.store.record_unresolved(&unresolved)?;
        self.store.upsert_file(&FileRecord {
            path: rel.into(),
            mtime: entry.mtime,
            size: entry.size as i64,
            fingerprint,
            lang,
            indexed_at: aneural_core::now_millis(),
        })?;
        Ok(Processed::Indexed(delta))
    }

    /// The nodes/edges previously derived from `rel`, as additions.
    fn cached_delta(&self, rel: &str) -> Result<GraphDelta> {
        let mut d = GraphDelta::new(DeltaPhase::Initial);
        d.nodes = self.store.nodes_by_origin(rel)?;
        d.edges = self.store.edges_by_origin(rel)?;
        // Nodes shared between files — a package, an unresolved wiki-link
        // placeholder — live without an origin, so they are not in
        // `nodes_by_origin` and have to be fetched by what points at them.
        let own: HashSet<&str> = d.nodes.iter().map(|n| n.id.as_str()).collect();
        let referenced: Vec<NodeId> = d
            .edges
            .iter()
            .filter(|e| !own.contains(e.dst.as_str()))
            .map(|e| e.dst.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let shared = self.store.shared_nodes(&referenced)?;
        d.nodes.extend(shared);
        Ok(d)
    }

    /// Everything one synthetic origin put in the store, as a delta to replay.
    fn cached_origin(&self, origin: &str) -> Result<GraphDelta> {
        let mut d = GraphDelta::new(DeltaPhase::Initial);
        d.nodes = self.store.nodes_by_origin(origin)?;
        d.edges = self.store.edges_by_origin(origin)?;
        Ok(d)
    }

    /// Remove a file or directory (and everything derived from it) from the graph.
    fn remove_path(&mut self, rel: &str) -> Result<GraphDelta> {
        let mut delta = GraphDelta::new(DeltaPhase::Live);
        for origin in self.store.origins_under(rel)? {
            let d = self.store.delete_origin(&origin)?;
            delta.merge(d);
            self.store.delete_file(&origin)?;
            self.md_index.remove(&origin);
        }
        let ids = self.store.node_ids_under(rel)?;
        if !ids.is_empty() {
            self.store.delete_nodes(&ids)?;
            delta.removed_node_ids.extend(ids);
        }
        self.repos
            .retain(|r| r != rel && !r.starts_with(&format!("{rel}/")));
        Ok(delta)
    }

    /// Make sure every ancestor directory of `rel` exists as a node.
    fn ensure_ancestors(&mut self, rel: &str, delta: &mut GraphDelta) -> Result<()> {
        let mut chain: Vec<String> = Vec::new();
        let mut cur = rel
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| ".".into());
        loop {
            chain.push(cur.clone());
            if cur == "." {
                break;
            }
            cur = cur
                .rsplit_once('/')
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| ".".into());
        }
        chain.reverse();
        for dir in chain {
            if self.store.get_node(&NodeId::dir(&dir))?.is_some() {
                continue;
            }
            let Some(entry) = walk::entry_for(&self.ws, &self.ws.abs(&dir)) else {
                continue;
            };
            if entry.is_repo {
                self.repos.insert(entry.rel.clone());
            }
            let repo = walk::repo_for(&entry.rel, &self.repos);
            delta.nodes.push(walk::node_for(
                &self.ws,
                &self.config,
                &entry,
                repo.as_ref(),
            ));
            if let Some(e) = walk::contains_edge_for(&entry.rel, true) {
                delta.edges.push(e);
            }
        }
        Ok(())
    }

    /// React to filesystem changes (absolute paths).
    pub fn index_paths(
        &mut self,
        paths: &[PathBuf],
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<()> {
        let mut delta = GraphDelta::new(DeltaPhase::Live);
        let mut rels: Vec<String> = paths
            .iter()
            .filter_map(|p| self.ws.rel(p))
            .filter(|r| !self.is_ignored(r))
            .collect();
        rels.sort();
        rels.dedup();
        for rel in rels {
            let abs = self.ws.abs(&rel);
            if !abs.exists() {
                let d = self.remove_path(&rel)?;
                delta.merge(d);
                continue;
            }
            let Some(entry) = walk::entry_for(&self.ws, &abs) else {
                continue;
            };
            self.ensure_ancestors(&rel, &mut delta)?;
            if entry.is_dir {
                // a new directory: index its subtree
                if self.store.get_node(&NodeId::dir(&rel))?.is_none() {
                    let mut sub = GraphDelta::new(DeltaPhase::Live);
                    let mut files = Vec::new();
                    for result in walk::walker(&self.ws, &self.config, &abs)? {
                        let Ok(dent) = result else { continue };
                        let Some(e) = walk::entry_for(&self.ws, dent.path()) else {
                            continue;
                        };
                        if e.is_repo {
                            self.repos.insert(e.rel.clone());
                        }
                        let repo = walk::repo_for(&e.rel, &self.repos);
                        sub.nodes
                            .push(walk::node_for(&self.ws, &self.config, &e, repo.as_ref()));
                        if let Some(edge) = walk::contains_edge_for(&e.rel, e.is_dir) {
                            sub.edges.push(edge);
                        }
                        if !e.is_dir {
                            if is_markdown(&e.rel) {
                                let rel = e.rel.clone();
                                self.index_markdown(&rel);
                            }
                            files.push(e);
                        }
                    }
                    self.store.apply_delta(&sub)?;
                    delta.merge(sub);
                    for f in files {
                        delta.merge(self.process_file(&f, false)?.into_delta());
                    }
                }
                continue;
            }
            if is_markdown(&rel) {
                self.index_markdown(&rel);
            }
            let is_new = self.store.get_node(&NodeId::file(&rel))?.is_none();
            if is_new && let Some(e) = walk::contains_edge_for(&rel, false) {
                delta.edges.push(e);
            }
            delta.merge(self.process_file(&entry, is_new)?.into_delta());
        }
        if !delta.is_empty() {
            // structural additions collected above (ancestors, CONTAINS) need persisting too
            let structural = GraphDelta {
                nodes: delta
                    .nodes
                    .iter()
                    .filter(|n| n.origin.is_none())
                    .cloned()
                    .collect(),
                edges: delta
                    .edges
                    .iter()
                    .filter(|e| e.origin.is_none())
                    .cloned()
                    .collect(),
                ..GraphDelta::new(DeltaPhase::Live)
            };
            self.store.apply_delta(&structural)?;
            let kinds = self.shared_kinds();
            delta
                .removed_node_ids
                .extend(self.store.gc_orphan_by_kind(&kinds)?);
            for id in self.store.gc_orphan_packages()? {
                delta.removed_node_ids.push(id);
            }
            sink(EngineEvent::Delta(delta));
        }
        Ok(())
    }

    /// Block, applying filesystem changes as they happen, until `Stop`.
    pub fn watch_loop(
        &mut self,
        sink: &mut dyn FnMut(EngineEvent),
        commands: Receiver<EngineCommand>,
    ) -> Result<()> {
        let watcher = watch::Watcher::new(self.ws.root(), Duration::from_millis(300))?;
        sink(EngineEvent::Watching);
        // A first pass so an HTTP spore has data without waiting an interval.
        if self.has_http_spores()
            && let Err(e) = self.refresh_http(None, false, sink)
        {
            sink(EngineEvent::Error(e.to_string()));
        }
        loop {
            // Sleep until the next harvester is due rather than polling: a
            // workspace with no HTTP spores must not wake up at all.
            let tick = match self.next_http_due() {
                Some(d) => crossbeam_channel::after(d.max(Duration::from_secs(1))),
                None => crossbeam_channel::never(),
            };
            // History gets its own arm rather than sharing the HTTP timer, so
            // the two schedules stay independent. It is a timer and not a
            // watcher deliberately: a watcher would only tell us a transcript
            // grew, and we would then do the same seek-and-read anyway — while
            // `~/.claude` churns constantly, and the file being appended to
            // every few hundred milliseconds is exactly the one we are reading.
            let history_tick = match self.next_history_due() {
                Some(d) => crossbeam_channel::after(d),
                None => crossbeam_channel::never(),
            };
            crossbeam_channel::select! {
                recv(tick) -> _ => {
                    if let Err(e) = self.refresh_http(None, false, sink) {
                        sink(EngineEvent::Error(e.to_string()));
                    }
                }
                recv(history_tick) -> _ => {
                    if let Err(e) = self.refresh_history(false, sink) {
                        sink(EngineEvent::Error(e.to_string()));
                    }
                }
                recv(watcher.rx) -> msg => match msg {
                    Ok(paths) => {
                        if let Err(e) = self.index_paths(&paths, sink) {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                    }
                    Err(_) => return Ok(()),
                },
                recv(commands) -> cmd => match cmd {
                    Ok(EngineCommand::Reindex { force }) => {
                        if let Err(e) = self.index_full(force, sink) {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                    }
                    Ok(EngineCommand::ReloadSpores) => {
                        if let Err(e) = self.reload_spores() {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                        sink(EngineEvent::Spores {
                            spores: self.spores(),
                            errors: self.spore_errors.clone(),
                        });
                        if let Err(e) = self.index_full(false, sink) {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                    }
                    Ok(EngineCommand::SetSporeEnabled { id, on }) => {
                        if let Err(e) = self.set_spore_enabled(&id, on, sink) {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                    }
                    Ok(EngineCommand::RefreshHttp { id, force }) => {
                        if let Err(e) = self.refresh_http(id.as_deref(), force, sink) {
                            sink(EngineEvent::Error(e.to_string()));
                        }
                    }
                    Ok(EngineCommand::Stop) | Err(_) => return Ok(()),
                },
            }
        }
    }

    /// Reload spores from disk after the config or `.aneural/spores` changed.
    pub fn reload_spores(&mut self) -> Result<()> {
        self.config = self.ws.load_config()?;
        let (spores, errs) = spores::load_all(&self.ws, &self.config.spores.enabled);
        self.spores = spores;
        self.spore_errors = errs.iter().map(|e| e.to_string()).collect();
        // Settings may have changed under a spore that is already installed, so
        // a reload is a reason to re-fetch rather than wait out the interval.
        self.next_refresh.clear();
        Ok(())
    }

    /// Turn a spore on or off, converging the graph without a full reindex.
    ///
    /// The two directions are not symmetric: enabling *adds* nodes, so it
    /// re-harvests the files the spore applies to; disabling has to *retract*
    /// them, which is a delete by producer.
    pub fn set_spore_enabled(
        &mut self,
        id: &str,
        on: bool,
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<()> {
        if on {
            self.harvest_spore(id, sink)?;
            // An HTTP spore has no files to walk, so enabling it means fetching.
            self.refresh_http(Some(id), true, sink)?;
        } else {
            let Some(idx) = self
                .spores
                .iter()
                .position(|s| s.manifest.id() == id || s.manifest.name == id)
            else {
                return Err(Error::Other(format!("unknown spore `{id}`")));
            };
            self.spores[idx].enabled = false;
            let source = aneural_core::kinds::Source::spore(&self.spores[idx].manifest.id());
            let prefix = format!("spore://{}/", self.spores[idx].manifest.id());
            self.next_refresh.retain(|org, _| !org.starts_with(&prefix));
            let delta = self.store.delete_by_source(&source)?;
            self.emit(delta, sink);
        }
        sink(EngineEvent::Spores {
            spores: self.spores(),
            errors: self.spore_errors.clone(),
        });
        Ok(())
    }

    // ---- HTTP (tier 1) harvesters -----------------------------------------

    /// Supply the thing that is actually allowed to open a socket.
    ///
    /// Without one, HTTP harvesters report that this program cannot make web
    /// requests. That is deliberate: the engine is linked by the MCP server and
    /// by one-shot CLI queries, and neither should acquire the ability to call
    /// out just by being linked.
    pub fn set_fetcher(&mut self, fetcher: Box<dyn Fetcher>) {
        self.fetcher = Some(fetcher);
    }

    /// Override where `{secret.*}` comes from. Defaults to the environment.
    pub fn set_secrets(&mut self, secrets: Box<dyn SecretStore>) {
        self.secrets = secrets;
    }

    /// Whether any enabled spore has an HTTP harvester at all.
    pub fn has_http_spores(&self) -> bool {
        self.spores
            .iter()
            .filter(|s| s.enabled)
            .any(|s| s.http_harvesters().next().is_some())
    }

    /// Re-fetch every HTTP harvester that is due (or all of `only`'s, ignoring
    /// the schedule, when a user asked for it explicitly).
    pub fn refresh_http(
        &mut self,
        only: Option<&str>,
        force: bool,
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<RefreshStats> {
        let mut stats = RefreshStats::default();
        let now = Instant::now();

        // Snapshot the work first: the harvest borrows the store immutably and
        // applying the result borrows it mutably.
        struct Job {
            manifest: aneural_core::SporeManifest,
            harvester: Harvester,
        }
        let jobs: Vec<Job> = self
            .spores
            .iter()
            .filter(|s| s.enabled)
            .filter(|s| only.is_none_or(|id| s.manifest.id() == id || s.manifest.name == id))
            .flat_map(|s| {
                s.http_harvesters().map(|h| Job {
                    manifest: s.manifest.clone(),
                    harvester: h.clone(),
                })
            })
            .collect();

        for job in jobs {
            let Harvester::Http {
                id,
                request,
                select,
                max_pages,
                refresh_seconds,
                emit,
                expand,
            } = &job.harvester
            else {
                continue;
            };
            let org = spores::http::origin(&job.manifest.id(), id);
            if !force && self.next_refresh.get(&org).is_some_and(|due| *due > now) {
                stats.skipped += 1;
                continue;
            }

            let settings = self
                .config
                .spores
                .settings_for(&job.manifest.id(), &job.manifest.name);
            let source = aneural_core::kinds::Source::spore(&job.manifest.id());

            let report = {
                let store = &self.store;
                let known = |id: &str| {
                    NodeId::parse(id)
                        .ok()
                        .and_then(|n| store.get_node(&n).ok().flatten())
                        .is_some()
                };
                let fetcher: &dyn Fetcher = match &self.fetcher {
                    Some(f) => f.as_ref(),
                    None => &aneural_core::net::NoFetcher,
                };
                let cx = spores::http::Context {
                    fetcher,
                    secrets: self.secrets.as_ref(),
                    settings: &settings,
                    known: &known,
                };
                spores::http::harvest(
                    &job.manifest,
                    id,
                    request,
                    select,
                    *max_pages,
                    emit,
                    expand.as_ref(),
                    &source,
                    &cx,
                )
            };

            self.next_refresh.insert(
                org.clone(),
                now + Duration::from_secs(*refresh_seconds).max(Duration::from_secs(
                    aneural_core::spore::MIN_REFRESH_SECONDS,
                )),
            );

            for problem in &report.problems {
                let line = format!("{}/{id}: {problem}", job.manifest.id());
                sink(EngineEvent::Error(line.clone()));
                stats.problems.push(line);
            }

            // A failed fetch must not wipe what the last good one found: only
            // converge the graph when we actually have an answer.
            if !report.problems.is_empty() && report.harvest.nodes.is_empty() {
                continue;
            }

            let delta =
                self.store
                    .replace_origin(&org, &report.harvest.nodes, &report.harvest.edges)?;
            stats.harvesters += 1;
            stats.nodes += report.harvest.nodes.len() as u64;
            stats.edges += report.harvest.edges.len() as u64;
            self.emit(delta, sink);
        }
        Ok(stats)
    }

    /// How long until the soonest HTTP harvester is due, if any.
    pub fn next_http_due(&self) -> Option<Duration> {
        let now = Instant::now();
        self.spores
            .iter()
            .filter(|s| s.enabled)
            .flat_map(|s| {
                let id = s.manifest.id();
                s.http_harvesters().map(move |h| {
                    let org = spores::http::origin(&id, h.id());
                    match self.next_refresh.get(&org) {
                        Some(due) => due.saturating_duration_since(now),
                        // Never fetched in this session: due immediately.
                        None => Duration::ZERO,
                    }
                })
            })
            .min()
    }

    /// Re-run one spore's harvesters across every indexed file it applies to.
    pub fn harvest_spore(
        &mut self,
        name: &str,
        sink: &mut dyn FnMut(EngineEvent),
    ) -> Result<HarvestStats> {
        let Some(idx) = self
            .spores
            .iter()
            .position(|s| s.manifest.id() == name || s.manifest.name == name)
        else {
            return Err(Error::Other(format!("unknown spore `{name}`")));
        };
        let source = aneural_core::kinds::Source::spore(&self.spores[idx].manifest.id());
        self.spores[idx].enabled = true;
        let mut stats = HarvestStats::default();
        for rec in self.store.all_files()? {
            if !self.spores[idx].applies_to(&rec.path) {
                continue;
            }
            let Some(entry) = walk::entry_for(&self.ws, &self.ws.abs(&rec.path)) else {
                continue;
            };
            if let Processed::Indexed(d) = self.process_file(&entry, true)? {
                stats.files += 1;
                stats.nodes += d.nodes.iter().filter(|n| n.source == source).count() as u64;
                stats.edges += d.edges.iter().filter(|e| e.source == source).count() as u64;
                self.emit(d, sink);
            }
        }
        Ok(stats)
    }

    // ---- doctor -----------------------------------------------------------

    pub fn doctor(&self) -> Result<Vec<Diagnostic>> {
        let mut out = Vec::new();
        if let Err(e) = aneural_lang::abi_check() {
            out.push(Diagnostic::new("error", "grammar", e.to_string()));
        }
        for e in &self.spore_errors {
            out.push(Diagnostic::new("error", "spore", e.clone()));
        }
        for l in &self.config.languages {
            if !aneural_core::config::Language::ALL.contains(&l.as_str()) {
                out.push(Diagnostic::new(
                    "warning",
                    "config",
                    format!("unknown language `{l}` in config.languages"),
                ));
            }
        }
        for s in &self.config.spores.enabled {
            if !self.spores.iter().any(|sp| &sp.manifest.name == s) {
                out.push(Diagnostic::new(
                    "warning",
                    "spore",
                    format!("enabled spore `{s}` is not installed"),
                ));
            }
        }
        for def in self.node_types()? {
            if !aneural_icons::is_valid(&def.icon) {
                out.push(Diagnostic::new(
                    "warning",
                    "icon",
                    format!(
                        "node type `{}` uses unknown icon `{}` (falling back to {})",
                        def.kind,
                        def.icon,
                        NodeTypeDef::FALLBACK_ICON
                    ),
                ));
            }
        }
        for rec in self.store.all_files()? {
            if !self.ws.abs(&rec.path).exists() {
                out.push(
                    Diagnostic::new(
                        "warning",
                        "cache",
                        "indexed file no longer exists (run `aneural index`)",
                    )
                    .at(rec.path),
                );
            }
        }
        for u in self.store.list_unresolved(Some(500))? {
            out.push(
                Diagnostic::new(
                    "info",
                    "unresolved",
                    format!("`{}` (line {}): {}", u.specifier, u.line, u.reason),
                )
                .at(u.origin),
            );
        }
        Ok(out)
    }
}

enum Processed {
    Indexed(GraphDelta),
    Unchanged(GraphDelta),
}

impl Processed {
    fn into_delta(self) -> GraphDelta {
        match self {
            Processed::Indexed(d) | Processed::Unchanged(d) => d,
        }
    }
}

/// How much of a file to read when learning its aliases. Frontmatter sits at
/// the very top; anything past this is body.
const FRONTMATTER_PEEK: usize = 8 * 1024;

/// The first `limit` bytes of a file, as text. `None` if it cannot be read.
fn read_head(abs: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(abs).ok()?;
    let mut buf = Vec::with_capacity(limit.min(4096));
    f.take(limit as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn is_markdown(rel: &str) -> bool {
    matches!(rel.rsplit('.').next(), Some("md" | "mdx" | "markdown"))
}

/// Convenience for the GUI: run a full index then watch, forwarding events on
/// `tx`, until `commands` yields `Stop` or is dropped.
pub fn run(root: &Path, tx: Sender<EngineEvent>, commands: Receiver<EngineCommand>) {
    run_with(root, tx, commands, None)
}

/// As [`run`], but with something that can make web requests on a spore's
/// behalf. A host that passes `None` — the MCP server, a one-shot query — simply
/// has no tier-1 spores, and they say so rather than reporting an empty result.
pub fn run_with(
    root: &Path,
    tx: Sender<EngineEvent>,
    commands: Receiver<EngineCommand>,
    fetcher: Option<Box<dyn Fetcher>>,
) {
    let ws = Workspace::at(root);
    let mut sink = |ev: EngineEvent| {
        let _ = tx.send(ev);
    };
    let mut engine = match Engine::open(ws) {
        Ok(e) => e,
        Err(e) => {
            sink(EngineEvent::Error(e.to_string()));
            return;
        }
    };
    if let Some(f) = fetcher {
        engine.set_fetcher(f);
    }
    sink(EngineEvent::Spores {
        spores: engine.spores(),
        errors: engine.spore_errors().to_vec(),
    });
    if let Err(e) = engine.index_full(false, &mut sink) {
        sink(EngineEvent::Error(e.to_string()));
    }
    if let Err(e) = engine.watch_loop(&mut sink, commands) {
        sink(EngineEvent::Error(e.to_string()));
    }
}
