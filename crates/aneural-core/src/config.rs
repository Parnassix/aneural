//! `.aneural/config.json` and node type definitions.

use crate::kinds::NodeKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONFIG_SCHEMA_URL: &str = "https://aneural.dev/schema/config-v1.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub version: u32,
    /// Display name of the workspace (defaults to the directory name).
    pub name: String,
    /// Directories to index, relative to the workspace root.
    pub roots: Vec<String>,
    pub respect_gitignore: bool,
    /// Extra ignore globs (gitignore syntax) applied on top of `.gitignore`.
    pub ignore: Vec<String>,
    /// Languages to analyse for imports.
    pub languages: Vec<String>,
    pub typescript: TypeScriptConfig,
    pub spores: SporesConfig,
    pub history: HistoryConfig,
    pub runs: RunsConfig,
    /// Per-kind style overrides and lightweight custom kinds.
    pub node_types: BTreeMap<String, NodeTypeStyle>,
    pub gui: GuiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema: Some(CONFIG_SCHEMA_URL.to_string()),
            version: crate::SCHEMA_VERSION,
            name: String::new(),
            roots: vec![".".to_string()],
            respect_gitignore: true,
            ignore: vec![
                "**/node_modules/**".into(),
                "**/target/**".into(),
                "**/dist/**".into(),
                "**/build/**".into(),
                "**/.git/**".into(),
                "**/.turbo/**".into(),
                "**/__pycache__/**".into(),
                "**/.venv/**".into(),
                "**/vendor/**".into(),
            ],
            languages: Language::ALL.iter().map(|s| s.to_string()).collect(),
            typescript: TypeScriptConfig::default(),
            spores: SporesConfig::default(),
            history: HistoryConfig::default(),
            runs: RunsConfig::default(),
            node_types: BTreeMap::new(),
            gui: GuiConfig::default(),
        }
    }
}

impl Config {
    pub fn language_enabled(&self, lang: &str) -> bool {
        self.languages.iter().any(|l| l == lang)
    }
}

/// What this machine is set to run, and whether it may.
///
/// `enabled` is off until the workspace turns it on, and that default *is* the
/// consent: it reads `~/Library/LaunchAgents` and asks `launchctl` what is
/// loaded, which is outside the workspace the user opened. `execute` is a
/// second, separate gate for actually starting a process, because reading what
/// is scheduled and running it are not the same permission.
///
/// This file is committed, so everything here is a *declaration* a team can
/// share. Nothing in it may arm a machine on `git pull`: whether an agent is
/// installed on this particular machine is recorded in gitignored
/// `.aneural/state/`, never here.
///
/// Every switch here defaults to off, and that is the whole point: there is
/// nothing in this struct that opts anyone into anything. The two retention
/// numbers are the exception — they are not permissions, and "keep forever" is
/// a worse default for a committed file than a bounded window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct RunsConfig {
    /// Read the schedules this machine has, and work out what is due. Off until
    /// the workspace says otherwise, and that default *is* the consent.
    pub enabled: bool,
    /// Actually start processes. Reserved; nothing reads it yet.
    pub execute: bool,
    /// Extra cadence words this project uses, to days. Extends the builtin
    /// vocabulary rather than replacing it; an entry here also overrides a
    /// builtin word, which is how a project that means something else by
    /// "continuous" says so.
    pub cadences: BTreeMap<String, u32>,
    /// Override the launch-agents directory. Empty means
    /// `~/Library/LaunchAgents`. Exists so tests never read the real one, the
    /// same reason [`HistoryConfig::claude_root`] does.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub launch_agents_root: String,
    /// What this machine calls itself in the run journal. Empty means the
    /// hostname.
    ///
    /// It is in the committed config rather than derived every time because it
    /// is a *directory name in git history*: the day someone renames their
    /// laptop, a derived name would silently start a second journal alongside
    /// the first and the two would never be recognised as the same machine.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub machine: String,
    /// Drop journal segments older than this. Whole files only — a segment is
    /// never rewritten, because rewriting one breaks both the union merge and
    /// every reader's offset.
    pub keep_days: u32,
    /// Keep at most this many runs per script in the index. Bounds the derived
    /// database without touching the committed journal.
    pub keep_per_script: u32,
}

impl Default for RunsConfig {
    fn default() -> Self {
        RunsConfig {
            enabled: false,
            execute: false,
            cadences: BTreeMap::new(),
            launch_agents_root: String::new(),
            machine: String::new(),
            keep_days: 90,
            keep_per_script: 200,
        }
    }
}

/// Where the story of a change comes from: the repositories' own history, and
/// the agent sessions that produced it.
///
/// `claude` is off until the workspace turns it on, and that default *is* the
/// consent — it reads transcripts of this workspace's sessions, which contain
/// everything that was said in them. Nothing here is a spore capability: these
/// are engine producers, like the walker, and the tier ladder governs what a
/// *downloadable* spore may reach.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryConfig {
    /// Read each repository's commits. On by default: the repository is part of
    /// the workspace the user already opened.
    pub git: bool,
    /// How far back to read. Commits older than this are not indexed at all.
    pub max_commits: u32,
    /// Read this workspace's Claude Code sessions from `~/.claude`.
    pub claude: bool,
    /// Keep the text of what was typed on the session node. Session props are
    /// served to agents over MCP, so this puts your prompts within reach of
    /// whatever reads the focus.
    pub claude_prompts: bool,
    /// Override the Claude Code state directory. Empty means `~/.claude`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub claude_root: String,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            git: true,
            max_commits: 2_000,
            claude: false,
            claude_prompts: false,
            claude_root: String::new(),
        }
    }
}

/// Language identifiers used throughout (file `lang` prop, config, spores).
pub struct Language;

impl Language {
    pub const TYPESCRIPT: &'static str = "typescript";
    pub const JAVASCRIPT: &'static str = "javascript";
    pub const PYTHON: &'static str = "python";
    pub const RUST: &'static str = "rust";
    pub const GO: &'static str = "go";
    pub const JAVA: &'static str = "java";
    pub const PHP: &'static str = "php";
    pub const RUBY: &'static str = "ruby";
    pub const MARKDOWN: &'static str = "markdown";

    pub const ALL: &'static [&'static str] = &[
        Self::TYPESCRIPT,
        Self::JAVASCRIPT,
        Self::PYTHON,
        Self::RUST,
        Self::GO,
        Self::JAVA,
        Self::PHP,
        Self::RUBY,
    ];

    /// Language for a file extension (lowercase, without the dot).
    pub fn from_extension(ext: &str) -> Option<&'static str> {
        Some(match ext {
            "ts" | "tsx" | "mts" | "cts" => Self::TYPESCRIPT,
            "js" | "jsx" | "mjs" | "cjs" => Self::JAVASCRIPT,
            "py" | "pyi" => Self::PYTHON,
            "rs" => Self::RUST,
            "go" => Self::GO,
            "java" => Self::JAVA,
            "php" => Self::PHP,
            "rb" | "rake" | "gemspec" => Self::RUBY,
            "md" | "mdx" | "markdown" => Self::MARKDOWN,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TypeScriptConfig {
    /// `"auto"` (nearest tsconfig.json per file) or a workspace-relative path.
    pub tsconfig: String,
    /// `exports`/`imports` condition names used during resolution.
    pub condition_names: Vec<String>,
}

impl Default for TypeScriptConfig {
    fn default() -> Self {
        TypeScriptConfig {
            tsconfig: "auto".into(),
            condition_names: ["import", "require", "node", "types", "default"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }
}

/// One marketplace index. The list is ordered: the first registry that lists an
/// id provides it, so a team registry can deliberately shadow the official one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySource {
    /// Short label shown beside a search result, e.g. `official` or `acme`.
    pub name: String,
    /// `https://…/index.json`, or an absolute path for a private registry.
    pub url: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// The registry every client is seeded with. It is one static file in the
/// product repo, served raw from GitHub; see "How it is hosted" in
/// `docs/marketplace.md`. Moving it is a change to this constant alone,
/// because `migrate` re-points the entry named [`OFFICIAL_REGISTRY_NAME`].
pub const OFFICIAL_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/Parnassix/aneural/main/registry/index.json";

/// The name the client owns in `registries`. A user may rename their entry to
/// pin whatever URL they like; an entry still called this follows the build.
pub const OFFICIAL_REGISTRY_NAME: &str = "official";

/// Every URL the official registry has ever lived at. A v1 `registry` field
/// naming one of these folds into the official entry instead of surviving as
/// a `legacy` source that would 404 forever.
const RETIRED_OFFICIAL_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/aneural/spores/main/index.json",
    "https://raw.githubusercontent.com/aneural/spores/main/registry.json",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct SporesConfig {
    /// Enabled spore ids. Publisher-qualified (`aneural.comments`) from v2; bare
    /// names are still honoured so an unmigrated workspace keeps working.
    pub enabled: Vec<String>,
    /// Deprecated single registry URL. Read for migration, never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    pub registries: Vec<RegistrySource>,
    /// Whether `spores add <url>` may install from outside any index.
    pub allow_direct_url_install: bool,
    /// Per-spore values for the `settings` a manifest declares, keyed by spore
    /// id: `{"acme.gh": {"repo": "acme/widget"}}`. Credentials never live here —
    /// those are `{secret.*}` and come from the environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, BTreeMap<String, String>>,
}

impl SporesConfig {
    /// The settings recorded for one spore, by id or by bare name.
    pub fn settings_for(&self, id: &str, name: &str) -> BTreeMap<String, String> {
        self.settings
            .get(id)
            .or_else(|| self.settings.get(name))
            .cloned()
            .unwrap_or_default()
    }
}

impl Default for SporesConfig {
    fn default() -> Self {
        SporesConfig {
            enabled: [
                "aneural.comments",
                "aneural.plans",
                "aneural.icebox",
                "aneural.wiki-links",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            registry: None,
            registries: vec![RegistrySource {
                name: OFFICIAL_REGISTRY_NAME.into(),
                url: OFFICIAL_REGISTRY_URL.into(),
                disabled: false,
            }],
            allow_direct_url_install: true,
            settings: BTreeMap::new(),
        }
    }
}

/// One thing `migrate` changed, so the caller can report it.
#[derive(Clone, Debug, PartialEq)]
pub struct Migration {
    pub from: String,
    pub to: String,
}

impl SporesConfig {
    /// Does this config entry name that spore? An entry may be the qualified id
    /// or the bare name it had before publishers existed.
    ///
    /// This one function is why migration is never load-bearing: a workspace
    /// that is never migrated keeps working forever.
    pub fn entry_matches(entry: &str, id: &str, name: &str) -> bool {
        entry == id || entry == name
    }

    pub fn is_enabled(&self, id: &str, name: &str) -> bool {
        self.enabled
            .iter()
            .any(|e| Self::entry_matches(e, id, name))
    }

    /// Bring a v1 `spores` block up to date. Pure: it returns what changed and
    /// leaves persisting to the caller, so merely reading a config never dirties
    /// the user's working tree.
    pub fn migrate(&mut self, first_party: &[&str]) -> Vec<Migration> {
        let mut changes = Vec::new();

        // Seed the official registry first, so a custom one is kept *alongside*
        // it rather than replacing it.
        if self.registries.is_empty() {
            self.registries = Self::default().registries;
        }
        // The official URL belongs to the build, not the config: a workspace
        // written against an older location follows the move. Only the entry
        // *named* official is touched, so a renamed entry pins what it likes.
        for source in &mut self.registries {
            if source.name == OFFICIAL_REGISTRY_NAME && source.url != OFFICIAL_REGISTRY_URL {
                changes.push(Migration {
                    from: format!("registries[{OFFICIAL_REGISTRY_NAME}]: {}", source.url),
                    to: format!("registries[{OFFICIAL_REGISTRY_NAME}]: {OFFICIAL_REGISTRY_URL}"),
                });
                source.url = OFFICIAL_REGISTRY_URL.into();
            }
        }
        if let Some(url) = self.registry.take() {
            let was_official =
                url == OFFICIAL_REGISTRY_URL || RETIRED_OFFICIAL_URLS.contains(&url.as_str());
            if !url.is_empty() && !was_official && !self.has_url(&url) {
                self.registries.push(RegistrySource {
                    name: "legacy".into(),
                    url: url.clone(),
                    disabled: false,
                });
                changes.push(Migration {
                    from: format!("registry: {url}"),
                    to: "registries[legacy]".into(),
                });
            } else {
                changes.push(Migration {
                    from: format!("registry: {url}"),
                    to: "registries[official]".into(),
                });
            }
        }

        for entry in &mut self.enabled {
            if !entry.contains('.') && first_party.contains(&entry.as_str()) {
                let qualified = format!("{}.{entry}", crate::spore::FIRST_PARTY_PUBLISHER);
                changes.push(Migration {
                    from: entry.clone(),
                    to: qualified.clone(),
                });
                *entry = qualified;
            }
        }

        let mut seen = std::collections::HashSet::new();
        self.enabled.retain(|e| seen.insert(e.clone()));
        changes
    }

    fn has_url(&self, url: &str) -> bool {
        self.registries.iter().any(|r| r.url == url)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct GuiConfig {
    pub theme: String,
    /// Camera zoom above which node labels are drawn.
    pub label_zoom_threshold: f32,
    /// Maximum nodes applied per frame during growth (keeps the animation legible).
    pub growth_budget_per_frame: u32,
    /// How the app's look follows the time of day: `"auto"` takes it from the
    /// clock — daylit through working hours, bioluminescent after dark —
    /// while `"day"` and `"night"` hold it at one end.
    pub circadian: String,
    /// React to what is happening on this machine while you work: a glow on
    /// the nodes involved and a portal onto them. `false` is today's app,
    /// pixel for pixel.
    pub live: bool,
    /// How many portals may be open at once. `0` keeps the glow and the
    /// top-bar count and draws no apertures.
    pub portals: u32,
    /// How hard to look: `"auto"` samples eagerly when the machine is idle and
    /// backs off under load or when the window is behind something else.
    /// `"eager"` and `"calm"` pin the two ends; `"off"` never starts the
    /// sampler, so nothing outside the graph is read at all.
    pub live_pace: String,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            theme: "mycelium-dark".into(),
            label_zoom_threshold: 0.8,
            growth_budget_per_frame: 200,
            circadian: "auto".into(),
            live: true,
            portals: 3,
            live_pace: "auto".into(),
        }
    }
}

/// Visual style overrides for a kind, as used in `config.json#nodeTypes`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeTypeStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A full node type definition (`.aneural/nodes/<kind>.json`, spore manifests, builtins).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeTypeDef {
    pub kind: String,
    #[serde(default)]
    pub label: String,
    /// icondata static name, e.g. `LuFolder`, `SiTypescript`.
    #[serde(default = "NodeTypeDef::default_icon")]
    pub icon: String,
    /// Hex colour.
    #[serde(default = "NodeTypeDef::default_color")]
    pub color: String,
    /// `circle` | `hexagon` | `pill` | `square` | `diamond`.
    #[serde(default = "NodeTypeDef::default_shape")]
    pub shape: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Who provides this kind: `builtin`, `workspace`, or `spore:<name>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
}

impl NodeTypeDef {
    pub const FALLBACK_ICON: &'static str = "LuCircleDot";

    fn default_icon() -> String {
        Self::FALLBACK_ICON.into()
    }
    fn default_color() -> String {
        "#9aa0a6".into()
    }
    fn default_shape() -> String {
        "circle".into()
    }

    pub fn new(
        kind: &str,
        label: &str,
        icon: &str,
        color: &str,
        shape: &str,
        description: &str,
    ) -> Self {
        NodeTypeDef {
            kind: kind.into(),
            label: label.into(),
            icon: icon.into(),
            color: color.into(),
            shape: shape.into(),
            description: description.into(),
            provider: "builtin".into(),
        }
    }

    pub fn apply(&mut self, style: &NodeTypeStyle) {
        if let Some(i) = &style.icon {
            self.icon = i.clone();
        }
        if let Some(c) = &style.color {
            self.color = c.clone();
        }
        if let Some(s) = &style.shape {
            self.shape = s.clone();
        }
        if let Some(l) = &style.label {
            self.label = l.clone();
        }
    }
}

/// Node types the engine produces itself, with the default mycelium palette.
pub fn builtin_node_types() -> Vec<NodeTypeDef> {
    vec![
        NodeTypeDef::new(
            NodeKind::DIRECTORY,
            "Directory",
            "LuFolder",
            "#8fae6b",
            "circle",
            "A folder in the workspace",
        ),
        NodeTypeDef::new(
            NodeKind::FILE,
            "File",
            "LuFile",
            "#d9d2c5",
            "circle",
            "A source or asset file",
        ),
        NodeTypeDef::new(
            NodeKind::REPO,
            "Repo",
            "VsRepo",
            "#e0a458",
            "hexagon",
            "A git repository root",
        ),
        NodeTypeDef::new(
            NodeKind::MANIFEST,
            "Manifest",
            "VsPackage",
            "#c78b5e",
            "square",
            "A package manifest (package.json, Cargo.toml, ...)",
        ),
        NodeTypeDef::new(
            NodeKind::PACKAGE,
            "Package",
            "LuPackage",
            "#7d8aa5",
            "diamond",
            "An external dependency",
        ),
        NodeTypeDef::new(
            NodeKind::SYMBOL,
            "Symbol",
            "VsSymbolMethod",
            "#b0b0b0",
            "circle",
            "An exported symbol (reserved)",
        ),
        // The `plans` spore emits Plan nodes too, but the type is builtin so
        // that the `claude` producer's plans stay styled when it is disabled.
        NodeTypeDef::new(
            NodeKind::PLAN,
            "Plan",
            "LuMap",
            "#a58cd6",
            "hexagon",
            "A plan document",
        ),
        NodeTypeDef::new(
            NodeKind::SESSION,
            "Session",
            "LuMessageSquare",
            "#6fa8a0",
            "pill",
            "A Claude Code session that worked here",
        ),
        NodeTypeDef::new(
            NodeKind::COMMIT,
            "Commit",
            "VsSourceControl",
            "#b08968",
            "diamond",
            "A commit in one of the workspace's repositories",
        ),
        NodeTypeDef::new(
            NodeKind::USAGE,
            "Usage",
            "LuGauge",
            "#c0843f",
            "diamond",
            "What the agent sessions here have cost in tokens",
        ),
        // The `scripts` spore emits these, but the types are builtin for the
        // same reason `Plan` is: the `runs` producer names them too, and a Run
        // hanging off an untyped Script tells the reader nothing.
        NodeTypeDef::new(
            NodeKind::SCRIPT,
            "Script",
            "LuTerminal",
            "#c9736b",
            "square",
            "Something in this workspace you can run",
        ),
        NodeTypeDef::new(
            NodeKind::SCHEDULE,
            "Schedule",
            "LuCalendarClock",
            "#7d9ec0",
            "pill",
            "When a script is declared to run",
        ),
        NodeTypeDef::new(
            NodeKind::RUN,
            "Run",
            "LuHistory",
            "#a8a15f",
            "diamond",
            "The most recent run of a script",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips() {
        let c = Config::default();
        let json = serde_json::to_string_pretty(&c).unwrap();
        assert!(json.contains("\"$schema\""));
        assert!(json.contains("respectGitignore"));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn partial_config_fills_defaults() {
        let c: Config = serde_json::from_str(r#"{"name":"x","languages":["rust"]}"#).unwrap();
        assert_eq!(c.name, "x");
        assert_eq!(c.languages, vec!["rust"]);
        assert!(c.respect_gitignore);
        assert!(!c.ignore.is_empty());
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(Language::from_extension("tsx"), Some("typescript"));
        assert_eq!(Language::from_extension("rb"), Some("ruby"));
        assert_eq!(Language::from_extension("png"), None);
    }

    #[test]
    fn migration_is_idempotent_and_preserves_a_custom_registry() {
        let mut spores = SporesConfig {
            enabled: vec!["comments".into(), "plans".into(), "acme.adr".into()],
            registry: Some("https://acme.internal/spores.json".into()),
            registries: Vec::new(),
            allow_direct_url_install: true,
            settings: BTreeMap::new(),
        };
        let first_party = ["comments", "plans", "icebox", "wiki-links"];

        let changes = spores.migrate(&first_party);
        assert_eq!(
            spores.enabled,
            ["aneural.comments", "aneural.plans", "acme.adr"]
        );
        assert_eq!(spores.registry, None);
        // The custom registry survives as an extra source alongside the default.
        assert_eq!(spores.registries.len(), 2);
        assert!(spores.registries.iter().any(|r| r.name == "legacy"));
        assert_eq!(changes.len(), 3);

        // Migrating again changes nothing.
        let mut again = spores.clone();
        assert!(again.migrate(&first_party).is_empty());
        assert_eq!(again, spores);
    }

    #[test]
    fn bare_names_keep_working_without_migration() {
        // This is the whole compatibility promise: a workspace that is never
        // migrated must keep enabling the spores it always did.
        assert!(SporesConfig::entry_matches(
            "comments",
            "aneural.comments",
            "comments"
        ));
        assert!(SporesConfig::entry_matches(
            "aneural.comments",
            "aneural.comments",
            "comments"
        ));
        assert!(!SporesConfig::entry_matches(
            "bob.comments",
            "aneural.comments",
            "comments"
        ));

        let legacy = SporesConfig {
            enabled: vec!["comments".into()],
            ..Default::default()
        };
        assert!(legacy.is_enabled("aneural.comments", "comments"));
        assert!(!legacy.is_enabled("aneural.plans", "plans"));
    }

    #[test]
    fn the_official_registry_follows_the_build_but_a_renamed_one_does_not() {
        let first_party = ["comments"];
        let old = "https://raw.githubusercontent.com/aneural/spores/main/index.json";
        let mut spores = SporesConfig {
            registries: vec![
                RegistrySource {
                    name: OFFICIAL_REGISTRY_NAME.into(),
                    url: old.into(),
                    disabled: false,
                },
                RegistrySource {
                    name: "pinned".into(),
                    url: old.into(),
                    disabled: false,
                },
            ],
            ..Default::default()
        };

        let changes = spores.migrate(&first_party);
        assert_eq!(spores.registries[0].url, OFFICIAL_REGISTRY_URL);
        assert_eq!(
            spores.registries[1].url, old,
            "a renamed entry is the user's"
        );
        assert_eq!(changes.len(), 1);
        assert!(changes[0].from.contains(old), "{:?}", changes[0]);

        assert!(spores.migrate(&first_party).is_empty());

        // The v1 field pointing at the old official location folds into the
        // official entry rather than surviving as a `legacy` duplicate.
        let mut v1 = SporesConfig {
            registry: Some(old.into()),
            registries: Vec::new(),
            ..Default::default()
        };
        v1.migrate(&first_party);
        assert_eq!(v1.registries.len(), 1);
        assert_eq!(v1.registries[0].url, OFFICIAL_REGISTRY_URL);
    }

    #[test]
    fn the_default_registry_is_the_only_one_configured() {
        let spores = SporesConfig::default();
        assert_eq!(spores.registries.len(), 1);
        assert_eq!(spores.registries[0].name, "official");
        assert!(
            spores.registry.is_none(),
            "the v1 field is never written back"
        );
    }

    #[test]
    fn schema_exports() {
        let schema = schemars::schema_for!(Config);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v["properties"]["languages"].is_object());
    }
}
