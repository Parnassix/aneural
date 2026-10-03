//! `spore.json` manifests: declarative node/edge types plus harvesters that
//! infer nodes from files. Spores never execute third-party code; every
//! harvester kind is a built-in runner in `aneural-engine`.

use crate::config::NodeTypeDef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SPORE_SCHEMA_URL: &str = "https://aneural.dev/schema/spore-v2.json";

/// Spores published by the Aneural project itself. They predate publishers and
/// keep the short id prefixes and kind names they shipped with.
pub const FIRST_PARTY_PUBLISHER: &str = "aneural";

/// Node id prefixes the engine owns. A third-party spore emitting one of these
/// could overwrite a real file, directory or package node.
pub const RESERVED_ID_PREFIXES: &[&str] = &["file", "dir", "repo", "manifest", "pkg", "sym"];

/// Node kinds the engine owns. A third-party spore declaring one of these would
/// repaint core nodes for the whole graph.
pub const RESERVED_KINDS: &[&str] = &["Directory", "File", "Repo", "Manifest", "Package", "Symbol"];

/// Does allowlist entry `pattern` cover `host`? Exact, or `*.example.com`
/// covering any subdomain but never the bare parent.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let (pattern, host) = (pattern.to_ascii_lowercase(), host.to_ascii_lowercase());
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

/// Every `{prefix<name>}` in a template, e.g. `vars(t, "secret.")`.
pub fn vars(template: &str, prefix: &str) -> Vec<String> {
    let needle = format!("{{{prefix}");
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(at) = rest.find(&needle) {
        rest = &rest[at + needle.len()..];
        match rest.find('}') {
            Some(end) => {
                let name = &rest[..end];
                if !name.is_empty() && !name.contains('{') {
                    out.push(name.to_string());
                }
                rest = &rest[end..];
            }
            None => break,
        }
    }
    out
}

fn first_var(template: &str, prefix: &str) -> Option<String> {
    vars(template, prefix).into_iter().next()
}

/// The literal `<prefix>` of an id template, or `None` when the template does
/// not begin with a literal prefix (`"{kind}:{file}"` is not addressable).
pub fn template_prefix(template: &str) -> Option<&str> {
    let (prefix, _) = template.split_once(':')?;
    if prefix.is_empty() || prefix.contains('{') {
        None
    } else {
        Some(prefix)
    }
}

/// How much of the host a spore needs, derived from its capabilities. Each step
/// up is a louder consent prompt; see `docs/spores.md`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// Pure declarative harvesting. No capabilities, nothing to consent to.
    Declarative,
    /// Declarative plus outbound HTTP to named hosts, with named secrets.
    Http,
    /// A sandboxed WASM component: sockets and graph reads, never the filesystem.
    Sandboxed,
    /// Native code with the user's own privileges. Quarantined and disclaimed.
    Native,
}

impl Tier {
    /// The tiers whose runners this build actually ships.
    pub const SUPPORTED: &'static [Tier] = &[Tier::Declarative, Tier::Http];

    pub fn is_supported(self) -> bool {
        Self::SUPPORTED.contains(&self)
    }

    pub fn label(self) -> &'static str {
        match self {
            Tier::Declarative => "declarative",
            Tier::Http => "http",
            Tier::Sandboxed => "sandboxed",
            Tier::Native => "native",
        }
    }
}

/// Something a spore needs from the host, declared up front so the user can see
/// it before installing. The runners for everything above `Tier::Declarative`
/// are not implemented yet; the vocabulary exists now so that consent does not
/// have to be redesigned when they land.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Capability {
    /// Outbound HTTP to these hosts, and nowhere else.
    Http { hosts: Vec<String> },
    /// Outbound TCP to these `host:port` endpoints.
    Tcp { endpoints: Vec<String> },
    /// Read these named secrets, and send them only to these hosts. Never the
    /// values in the manifest. `hosts` is what turns "may read your token"
    /// into "may read your token *for api.github.com*": a secret can then be
    /// consented to without also consenting to every host the spore reaches.
    Secret {
        names: Vec<String>,
        #[serde(default)]
        hosts: Vec<String>,
    },
    /// Read nodes of these kinds back out of the graph.
    GraphRead { kinds: Vec<String> },
    /// Read files at these paths, which may be outside the workspace.
    ///
    /// `native`, and deliberately so. There is no path analogue of
    /// [`is_private_host`] — `paths: ["~"]` looks like any other entry — and
    /// `FsRead` composed with `Http` is arbitrary exfiltration of anything
    /// readable. So this is vocabulary, not a grant: a spore that wants to read
    /// your home directory can now say so and be refused for the right reason.
    /// The engine's own producers read files without declaring anything,
    /// because the engine is the trusted part, not a thing the registry ships.
    FsRead { paths: Vec<String> },
    /// Write to these workspace-relative paths.
    FsWrite { paths: Vec<String> },
    /// Run these commands on the user's machine.
    Subprocess { commands: Vec<String> },
}

impl Capability {
    pub fn tier(&self) -> Tier {
        match self {
            Capability::Http { .. } | Capability::Secret { .. } => Tier::Http,
            Capability::Tcp { .. } | Capability::GraphRead { .. } => Tier::Sandboxed,
            Capability::FsRead { .. }
            | Capability::FsWrite { .. }
            | Capability::Subprocess { .. } => Tier::Native,
        }
    }

    /// One line of plain English for the consent sheet.
    pub fn consent_line(&self) -> String {
        match self {
            Capability::Http { hosts } => format!("make web requests to {}", join(hosts)),
            Capability::Tcp { endpoints } => format!("open connections to {}", join(endpoints)),
            Capability::Secret { names, hosts } => format!(
                "read your saved {} and send {} only to {}",
                join(names),
                if names.len() == 1 { "it" } else { "them" },
                join(hosts)
            ),
            Capability::GraphRead { kinds } => {
                format!("read {} nodes from your graph", join(kinds))
            }
            Capability::FsRead { paths } => format!("read files under {}", join(paths)),
            Capability::FsWrite { paths } => format!("write files under {}", join(paths)),
            Capability::Subprocess { commands } => {
                format!("run {} on your machine", join(commands))
            }
        }
    }

    fn targets(&self) -> &[String] {
        match self {
            Capability::Http { hosts } => hosts,
            Capability::Tcp { endpoints } => endpoints,
            Capability::Secret { names, .. } => names,
            Capability::GraphRead { kinds } => kinds,
            Capability::FsRead { paths } => paths,
            Capability::FsWrite { paths } => paths,
            Capability::Subprocess { commands } => commands,
        }
    }

    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let targets = self.targets();
        if targets.is_empty() {
            errs.push(format!(
                "capability `{}` must name what it needs access to",
                self.tier().label()
            ));
        }
        for t in targets {
            if t == "*" {
                errs.push(format!(
                    "capability `{}` may not request `*`; list what you need",
                    self.tier().label()
                ));
            }
        }
        match self {
            Capability::Http { hosts } => {
                for h in hosts {
                    if is_private_host(h) {
                        errs.push(format!(
                            "capability `http` may not name `{h}`: a listed spore reaches public \
                             hostnames, never this machine or its network"
                        ));
                    }
                }
            }
            Capability::Secret { hosts, .. } => {
                if hosts.is_empty() {
                    errs.push(
                        "capability `secret` must say which hosts its secrets may be sent to"
                            .into(),
                    );
                }
                if hosts.iter().any(|h| h == "*") {
                    errs.push("capability `secret` may not send secrets to `*`".into());
                }
            }
            _ => {}
        }
        errs
    }
}

/// Whether a declared host names this machine or a private network rather
/// than something on the public internet. Checked on the literal pattern, so
/// `*.local` is refused as a whole. A re-check of the address a name actually
/// resolves to belongs with the TCP runner, when there is one.
pub fn is_private_host(pattern: &str) -> bool {
    let host = pattern
        .strip_prefix("*.")
        .unwrap_or(pattern)
        .to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let dotted = format!(".{host}");
    [".localhost", ".local", ".internal", ".home.arpa"]
        .iter()
        .any(|suffix| dotted.ends_with(suffix))
}

fn join(items: &[String]) -> String {
    items.join(", ")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SporeManifest {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Lowercase kebab-case publisher, unique in the marketplace. Combined with
    /// `name` it forms the spore id (`acme.adr`). Empty for legacy v1 manifests,
    /// which are treated as unqualified.
    #[serde(default)]
    pub publisher: String,
    /// Lowercase kebab-case identifier, unique within the publisher.
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Source repository, shown in the marketplace listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// What the spore needs from the host. Empty means it is purely
    /// declarative and needs nothing; see [`Tier`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// Values the user must fill in before the spore can do anything. An HTTP
    /// harvester is almost always parameterised by *whose* data to fetch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingDef>,
    /// Required Aneural version range (semver).
    #[serde(default = "SporeManifest::default_range")]
    pub aneural: String,
    #[serde(default)]
    pub node_types: Vec<NodeTypeDef>,
    #[serde(default)]
    pub edge_types: Vec<EdgeTypeDef>,
    #[serde(default)]
    pub harvesters: Vec<Harvester>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<Panel>,
}

impl SporeManifest {
    fn default_range() -> String {
        ">=0.1".into()
    }

    /// Marketplace id: `publisher.name`, or bare `name` for legacy manifests
    /// that predate publishers.
    pub fn id(&self) -> String {
        if self.publisher.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.publisher, self.name)
        }
    }

    /// First-party spores may keep the short id prefixes and kind names they
    /// shipped with; everything else must namespace under its own id.
    pub fn is_first_party(&self) -> bool {
        self.publisher == FIRST_PARTY_PUBLISHER
    }

    /// The highest tier any declared capability demands. Derived, never
    /// declared, so a manifest cannot understate what it will do.
    pub fn tier(&self) -> Tier {
        self.capabilities
            .iter()
            .map(Capability::tier)
            .max()
            .unwrap_or(Tier::Declarative)
    }

    /// Structural validation (regexes and queries are checked by the engine).
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.name.is_empty() || crate::slug(&self.name) != self.name {
            errs.push(format!("name `{}` must be lowercase kebab-case", self.name));
        }
        if !self.publisher.is_empty() && crate::slug(&self.publisher) != self.publisher {
            errs.push(format!(
                "publisher `{}` must be lowercase kebab-case",
                self.publisher
            ));
        }
        if semver::Version::parse(&self.version).is_err() {
            errs.push(format!("version `{}` is not valid semver", self.version));
        }
        if semver::VersionReq::parse(&self.aneural).is_err() {
            errs.push(format!(
                "aneural range `{}` is not a valid semver requirement",
                self.aneural
            ));
        }
        for cap in &self.capabilities {
            errs.extend(cap.validate());
            if let Capability::Secret { hosts, .. } = cap {
                for h in hosts {
                    if !self.host_allowed(h) {
                        errs.push(format!(
                            "capability `secret` names host `{h}`, which the `http` capability \
                             does not cover"
                        ));
                    }
                }
            }
        }
        let mut ids = std::collections::HashSet::new();
        for h in &self.harvesters {
            if !ids.insert(h.id()) {
                errs.push(format!("duplicate harvester id `{}`", h.id()));
            }
            if h.id().is_empty() {
                errs.push("harvester id must not be empty".into());
            }
            if let Harvester::Wasm { .. } = h {
                errs.push(format!(
                    "harvester `{}`: wasm harvesters are reserved and not yet supported",
                    h.id()
                ));
            }
            if let Harvester::Http {
                request,
                max_pages,
                refresh_seconds,
                expand,
                ..
            } = h
            {
                errs.extend(self.check_http(h.id(), request, *max_pages));
                if *refresh_seconds < MIN_REFRESH_SECONDS {
                    errs.push(format!(
                        "harvester `{}`: refreshSeconds {refresh_seconds} is below the {MIN_REFRESH_SECONDS}s floor",
                        h.id()
                    ));
                }
                if let Some(x) = expand {
                    errs.extend(self.check_http(h.id(), &x.request, x.max_pages));
                }
            }
            for e in h.emit_blocks() {
                errs.extend(self.check_emit(h.id(), e));
            }
        }
        for set in &self.settings {
            if set.key.is_empty()
                || !set
                    .key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                errs.push(format!("setting key `{}` must be alphanumeric", set.key));
            }
        }
        for nt in &self.node_types {
            if nt.kind.is_empty() {
                errs.push("node type kind must not be empty".into());
            } else if !self.may_use_kind(&nt.kind) {
                errs.push(format!(
                    "node type kind `{}` is reserved; use a kind of your own",
                    nt.kind
                ));
            }
        }
        errs
    }

    /// A spore may only emit ids under a prefix it owns. Core prefixes belong
    /// to the engine; the short spore prefixes belong to the first-party set.
    fn may_use_prefix(&self, prefix: &str) -> bool {
        if self.is_first_party() {
            return true;
        }
        if RESERVED_ID_PREFIXES.contains(&prefix) {
            return false;
        }
        let own = format!("{}.", self.id());
        prefix.starts_with(&own)
    }

    fn may_use_kind(&self, kind: &str) -> bool {
        if self.is_first_party() {
            return true;
        }
        !RESERVED_KINDS.contains(&kind)
    }

    /// Every rule that makes a declared web request safe to consent to.
    ///
    /// The point of an allowlist is that a user can read it and know where
    /// their token is going. That only holds if the host is literal, the scheme
    /// is TLS, and the secret cannot be smuggled into a URL — so all three are
    /// refused here rather than at request time.
    fn check_http(&self, hid: &str, req: &HttpRequest, max_pages: u32) -> Vec<String> {
        let mut errs = Vec::new();

        if !self
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::Http { .. }))
        {
            errs.push(format!(
                "harvester `{hid}`: makes web requests but the manifest declares no `http` capability, \
                 so the consent sheet would claim it needs nothing"
            ));
        }

        match req.literal_host() {
            None => errs.push(format!(
                "harvester `{hid}`: url must be `https://` with a literal host (found `{}`)",
                req.url
            )),
            Some(host) => {
                if !self.host_allowed(host) {
                    errs.push(format!(
                        "harvester `{hid}`: host `{host}` is not in the declared `http` capability"
                    ));
                }
            }
        }

        if let Some(name) = first_var(&req.url, "secret.") {
            errs.push(format!(
                "harvester `{hid}`: `{{secret.{name}}}` may not appear in a url; \
                 URLs are logged and cached — put it in a header"
            ));
        }
        for (header, value) in &req.headers {
            for name in vars(value, "secret.") {
                if !self.secret_declared(&name) {
                    errs.push(format!(
                        "harvester `{hid}`: header `{header}` uses secret `{name}`, \
                         which the manifest does not declare"
                    ));
                } else if let Some(host) = req.literal_host()
                    && !self.secret_allowed(&name, host)
                {
                    errs.push(format!(
                        "harvester `{hid}`: header `{header}` would send secret `{name}` \
                         to `{host}`, which that secret's hosts do not include"
                    ));
                }
            }
        }

        for text in std::iter::once(&req.url).chain(req.headers.values()) {
            for key in vars(text, "setting.") {
                if !self.settings.iter().any(|s| s.key == key) {
                    errs.push(format!(
                        "harvester `{hid}`: uses setting `{key}`, which the manifest does not declare"
                    ));
                }
            }
        }

        if max_pages == 0 || max_pages > MAX_PAGES_CEILING {
            errs.push(format!(
                "harvester `{hid}`: maxPages must be between 1 and {MAX_PAGES_CEILING}"
            ));
        }
        errs
    }

    /// A declared host matches exactly, or as `*.suffix` covering subdomains.
    pub fn host_allowed(&self, host: &str) -> bool {
        self.capabilities.iter().any(|c| match c {
            Capability::Http { hosts } => hosts.iter().any(|h| host_matches(h, host)),
            _ => false,
        })
    }

    pub fn secret_declared(&self, name: &str) -> bool {
        self.capabilities.iter().any(|c| match c {
            Capability::Secret { names, .. } => names.iter().any(|n| n == name),
            _ => false,
        })
    }

    /// Whether `name` may travel to `host`: declared, and the host is one the
    /// secret was scoped to. The runner asks this before every request,
    /// including each `Link: rel="next"` hop, because the server picks those.
    pub fn secret_allowed(&self, name: &str, host: &str) -> bool {
        self.capabilities.iter().any(|c| match c {
            Capability::Secret { names, hosts } => {
                names.iter().any(|n| n == name) && hosts.iter().any(|h| host_matches(h, host))
            }
            _ => false,
        })
    }

    fn check_emit(&self, hid: &str, emit: &Emit) -> Vec<String> {
        let mut errs = Vec::new();
        if let Some(prefix) = template_prefix(&emit.node.id) {
            if !self.may_use_prefix(prefix) {
                // Two different refusals, and saying "reserved" for both sends
                // the reader to `RESERVED_ID_PREFIXES` to look for a prefix that
                // was never in it. Emitting a builtin *kind* is fine — only the
                // id namespace is owned — so the message has to say which.
                let why = match RESERVED_ID_PREFIXES.contains(&prefix) {
                    true => format!("`{prefix}:` is reserved for the engine"),
                    false => format!("`{prefix}:` belongs to another producer"),
                };
                errs.push(format!(
                    "harvester `{hid}`: node id prefix {why}; ids must start with `{}.`                      (the node's `kind` may still be a builtin one)",
                    self.id()
                ));
            }
        } else {
            errs.push(format!(
                "harvester `{hid}`: node id `{}` must start with a literal `<prefix>:`",
                emit.node.id
            ));
        }
        if !emit.node.kind.is_empty() && !self.may_use_kind(&emit.node.kind) {
            errs.push(format!(
                "harvester `{hid}`: node kind `{}` is reserved",
                emit.node.kind
            ));
        }
        errs
    }

    /// Whether this spore supports the given Aneural version.
    pub fn supports(&self, aneural_version: &str) -> bool {
        match (
            semver::VersionReq::parse(&self.aneural),
            semver::Version::parse(aneural_version),
        ) {
            (Ok(req), Ok(v)) => req.matches(&v),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EdgeTypeDef {
    pub kind: String,
    #[serde(default)]
    pub label: String,
    /// `solid` | `dotted` | `dashed`.
    #[serde(default = "EdgeTypeDef::default_style")]
    pub style: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl EdgeTypeDef {
    fn default_style() -> String {
        "solid".into()
    }
}

/// A harvester turns files into nodes and edges. Templates may reference
/// `{file}` (workspace-relative path), `{line}`, named captures like `{text}`,
/// and the functions `{hash(text)}` and `{slug(text)}`. `$node` inside an
/// edge refers to the node emitted by the same harvester.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
// `rename_all` on an enum renames the *variants*; without `rename_all_fields`
// the struct-variant fields keep their Rust spelling, so `maxPages` and
// `refreshSeconds` in a manifest were silently dropped and the defaults used.
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Harvester {
    /// Line-oriented regex over files matching `include` globs.
    Regex {
        id: String,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        /// Rust regex with named captures. Applied per line.
        pattern: String,
        /// Stop at the first matching line. For a harvester whose pattern is a
        /// marker that a file *is* something — a shebang, a module constant —
        /// rather than a thing to collect one of per occurrence. Without it the
        /// node is emitted once per match and survives only because ids dedupe,
        /// which quietly makes the result depend on harvester order.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        once: bool,
        emit: Emit,
    },
    /// tree-sitter query over a supported language; captures become template vars.
    TreeSitter {
        id: String,
        language: String,
        query: String,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        emit: Emit,
    },
    /// Markdown documents or headings; also wiki-links and frontmatter targets.
    Markdown {
        id: String,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        #[serde(default)]
        granularity: MarkdownGranularity,
        /// Emit for each document/heading. Vars: `{file}`, `{title}`, `{heading}`,
        /// `{line}`, `{body}` plus frontmatter keys as `{fm.key}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        emit: Option<Emit>,
        /// Emit `RELATES_TO`-style edges for `[[Wiki Links]]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wikilinks: Option<WikiLinks>,
        /// Emit edges from a frontmatter list of paths.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotate: Option<Annotate>,
        /// Turn a frontmatter tag list into shared nodes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tags: Option<Tags>,
        /// Which table to read, for `granularity: table-row`. A document holds
        /// several tables about several things, so a harvester says which one it
        /// means by naming columns it must have.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<TableSelect>,
    },
    /// Schema of a local SQLite database. Unlike every other kind this one is
    /// given the file's *path* rather than its bytes, so it can open a database
    /// far larger than the engine is willing to read into memory.
    ///
    /// The database is opened read-only; a spore can never write to it.
    Sqlite {
        id: String,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        /// Emit for each table. Vars: `{file}`, `{table}`, `{columns}`,
        /// `{columnCount}`, `{rowCount}`, `{sql}`, `{mtime}`.
        emit: Emit,
        /// Emit edges from a table to the tables its foreign keys point at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        references: Option<TableReferences>,
        /// Rows to sample into a `sample` prop. Zero — the default — reads no
        /// data at all. Anything above zero puts real rows into the graph, which
        /// the MCP server then serves to a coding agent, so it is opt-in and
        /// capped rather than unbounded.
        #[serde(default, skip_serializing_if = "is_zero")]
        sample_rows: u32,
    },
    /// A JSON web API. The one harvester with no file behind it at all: it is
    /// driven by a refresh interval rather than by the walker, and its nodes
    /// hang off a synthetic origin instead of a path.
    ///
    /// Everything about the request is declared here, so a T1 spore still ships
    /// no code. The host allowlist in the manifest's `capabilities` is what the
    /// user consents to, and both this validator and the runner refuse a URL
    /// that falls outside it.
    Http {
        id: String,
        request: HttpRequest,
        /// RFC 6901 JSON pointer to the array of records in the response.
        /// Empty means the body is itself the array.
        #[serde(default)]
        select: String,
        /// Follow `Link: rel="next"` at most this many times. One page only by
        /// default: a spore has to ask before it walks an entire API.
        #[serde(default = "one")]
        max_pages: u32,
        /// How often to re-fetch. The floor is [`MIN_REFRESH_SECONDS`].
        #[serde(default = "default_refresh")]
        refresh_seconds: u64,
        /// Vars: `{item.<dotted.path>}` from each record, `{setting.<key>}`,
        /// and `{readAt}`.
        emit: Emit,
        /// A second request per record, for APIs that will not tell you in one
        /// call what a record touches. One level only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expand: Option<Expand>,
    },
    /// Reserved for a future WASM component runtime.
    Wasm { id: String, module: String },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn one() -> u32 {
    1
}

fn default_refresh() -> u64 {
    DEFAULT_REFRESH_SECONDS
}

/// Polling faster than this is not a graph, it is a denial-of-service against
/// someone else's API with the user's own credentials attached.
pub const MIN_REFRESH_SECONDS: u64 = 60;
pub const DEFAULT_REFRESH_SECONDS: u64 = 300;

/// Follow at most this many `Link: rel="next"` hops however many a manifest asks
/// for, so a paginating API cannot be turned into an unbounded download.
pub const MAX_PAGES_CEILING: u32 = 10;

/// A record's expansion may not fan out past this many follow-up requests.
pub const MAX_EXPAND_ITEMS: usize = 50;

/// One declared request. `url` must be literal-hosted `https://`; only header
/// values may carry a `{secret.*}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl HttpRequest {
    /// The literal host, or `None` when the authority is templated — which is
    /// exactly the case the allowlist cannot be checked for and so is rejected.
    pub fn literal_host(&self) -> Option<&str> {
        let rest = self.url.strip_prefix("https://")?;
        let authority = rest.split(['/', '?', '#']).next()?;
        if authority.is_empty() || authority.contains('{') {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(authority);
        Some(host.split(':').next().unwrap_or(host))
    }
}

/// A follow-up request made once per record of the outer response.
///
/// It may only emit **edges**, never nodes. That is the whole point: the second
/// call is how a pull request finds the files it changes, and those files are
/// already nodes in the graph. Letting an API invent `file:` nodes for paths
/// that are not in the checkout would quietly fill the graph with things the
/// user does not have.
///
/// In here `{item.*}` is the *inner* record and `{parent.*}` the outer one;
/// `$parent` is the node the outer record emitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Expand {
    pub request: HttpRequest,
    #[serde(default)]
    pub select: String,
    #[serde(default = "one")]
    pub max_pages: u32,
    /// Edges joining `$parent` to whatever each inner record names.
    pub edges: Vec<EmitEdge>,
}

/// A value the user must supply before an HTTP harvester can run — the
/// repository to watch, the JIRA site, the project key. Declared so the
/// marketplace can ask for it instead of the spore silently fetching nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingDef {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// The most rows a `sqlite` harvester may sample, however much it asks for.
pub const MAX_SAMPLE_ROWS: u32 = 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TableReferences {
    #[serde(default = "TableReferences::default_kind")]
    pub edge_kind: String,
}

impl TableReferences {
    fn default_kind() -> String {
        crate::kinds::EdgeKind::REFERENCES.to_string()
    }
}

impl Default for TableReferences {
    fn default() -> Self {
        TableReferences {
            edge_kind: Self::default_kind(),
        }
    }
}

impl Harvester {
    pub fn id(&self) -> &str {
        match self {
            Harvester::Regex { id, .. }
            | Harvester::TreeSitter { id, .. }
            | Harvester::Markdown { id, .. }
            | Harvester::Sqlite { id, .. }
            | Harvester::Http { id, .. }
            | Harvester::Wasm { id, .. } => id,
        }
    }

    /// Every `emit` block this harvester declares. Markdown harvesters may omit
    /// theirs entirely and only contribute wiki-link or frontmatter edges.
    pub fn emit_blocks(&self) -> Vec<&Emit> {
        match self {
            Harvester::Regex { emit, .. }
            | Harvester::TreeSitter { emit, .. }
            | Harvester::Sqlite { emit, .. } => vec![emit],
            Harvester::Markdown { emit, .. } => emit.iter().collect(),
            Harvester::Http { emit, .. } => vec![emit],
            Harvester::Wasm { .. } => Vec::new(),
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum MarkdownGranularity {
    /// One node per document.
    #[default]
    Document,
    /// One node per `## heading` (level 2 by default).
    Heading,
    /// One node per row of a pipe table. Vars: `{col.<key>}` per column, plus
    /// `{table}` (the headers, joined), `{headers}`, `{row}` (1-based within the
    /// table) and `{line}`.
    ///
    /// Column keys are the header text slugged with `_` rather than `-`, so
    /// `last_refreshed`, not `last-refreshed`. That is forced, not chosen: a
    /// template var is matched by `[A-Za-z_][A-Za-z0-9_.]*`, which admits `.`
    /// but not `-`, so a hyphenated key would render as the empty string and
    /// say nothing about why.
    TableRow,
}

/// Which table a `table-row` harvester means.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TableSelect {
    /// Only tables whose header row carries every one of these column keys.
    /// Empty means every table in the file, which is rarely what anyone wants.
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WikiLinks {
    #[serde(default = "WikiLinks::default_kind")]
    pub edge_kind: String,
    /// Placeholder node for a link whose target does not exist.
    ///
    /// A wiki link is a claim that a note *should* exist, and in a real vault a
    /// good fraction of them point at notes nobody has written yet — Obsidian
    /// keeps those in its graph rather than discarding them. Without this the
    /// link is dropped and the asking is lost with it.
    ///
    /// The node is shared by every file that links to the same missing target,
    /// so it is emitted with no origin of its own (the same arrangement as a
    /// `Package`) and collected once nothing points at it any more. Vars:
    /// `{target}` as written, and `{slug}` for the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<EmitNode>,
}

impl WikiLinks {
    fn default_kind() -> String {
        crate::kinds::EdgeKind::RELATES_TO.into()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tags {
    /// Frontmatter key holding the tags. Written `[a, b]`, as a `- item` block
    /// or bare `a, b` — all three are read.
    #[serde(default = "Tags::default_key")]
    pub frontmatter_key: String,
    #[serde(default = "Tags::default_kind")]
    pub edge_kind: String,
    /// The node one tag becomes. Shared by every file carrying that tag, so it
    /// is emitted with no origin of its own and collected once nothing carries
    /// it any more — the same arrangement as a `Package`. Vars: `{tag}` as
    /// written, and `{slug}` for the id, which makes tags case-insensitive.
    pub node: EmitNode,
}

impl Tags {
    fn default_key() -> String {
        "tags".into()
    }
    fn default_kind() -> String {
        crate::kinds::EdgeKind::RELATES_TO.into()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Annotate {
    /// Frontmatter key holding a list of workspace-relative paths.
    #[serde(default = "Annotate::default_key")]
    pub frontmatter_key: String,
    #[serde(default = "Annotate::default_kind")]
    pub edge_kind: String,
}

impl Annotate {
    fn default_key() -> String {
        "targets".into()
    }
    fn default_kind() -> String {
        crate::kinds::EdgeKind::ANNOTATES.into()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Emit {
    pub node: EmitNode,
    #[serde(default)]
    pub edges: Vec<EmitEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmitNode {
    pub kind: String,
    /// Id template, e.g. `comment:{file}#{hash(text)}`.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub props: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmitEdge {
    pub kind: String,
    /// `$node` or an id template.
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub props: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Panel {
    pub title: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

/// Information about an installed/available spore, for CLI/MCP listings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SporeInfo {
    /// `publisher.name`, the marketplace identity.
    pub id: String,
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub description: String,
    pub enabled: bool,
    /// `builtin` or `workspace`.
    pub location: String,
    pub path: String,
    pub node_kinds: Vec<String>,
    /// `declarative` | `http` | `sandboxed` | `native`.
    pub tier: String,
    /// One plain-English line per declared capability. Empty for a spore that
    /// needs nothing, which is most of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consent_lines: Vec<String>,
    /// Declared setting keys that have no value yet. A spore with any of these
    /// cannot do its job, and every surface should say so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_settings: Vec<String>,
    /// Every declared setting, with whatever this workspace has recorded for it,
    /// so a UI can render the form without re-reading the manifest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingValue>,
}

/// A declared setting joined to its current value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingValue {
    pub key: String,
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTS: &str = r##"{
      "publisher": "aneural", "name": "comments", "version": "0.1.0", "displayName": "Code Comments",
      "nodeTypes": [{ "kind": "Comment", "icon": "LuMessageSquare", "color": "#e0a458", "shape": "pill" }],
      "edgeTypes": [{ "kind": "ANNOTATES", "style": "dotted" }],
      "harvesters": [{
        "id": "todo", "kind": "regex", "include": ["**/*.ts"],
        "pattern": "(?i)\\b(?P<tag>TODO|FIXME)\\b:?\\s*(?P<text>.+)$",
        "emit": { "node": { "kind": "Comment", "id": "comment:{file}#{hash(text)}", "label": "{text}", "props": { "tag": "{tag}" } },
                  "edges": [{ "kind": "ANNOTATES", "src": "$node", "dst": "file:{file}", "props": { "line": "{line}" } }] }
      }]
    }"##;

    #[test]
    fn parses_and_validates() {
        let m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        assert_eq!(m.harvesters.len(), 1);
        assert!(matches!(m.harvesters[0], Harvester::Regex { .. }));
        assert!(m.validate().is_empty(), "{:?}", m.validate());
        assert!(m.supports("0.1.0"));
        assert_eq!(m.node_types[0].label, "");
    }

    #[test]
    fn rejects_bad_name_and_wasm() {
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        m.name = "Bad Name".into();
        m.harvesters.push(Harvester::Wasm {
            id: "w".into(),
            module: "x.wasm".into(),
        });
        let errs = m.validate();
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    #[test]
    fn id_is_publisher_qualified() {
        let m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        assert_eq!(m.id(), "aneural.comments");
        assert!(m.is_first_party());

        // A legacy manifest with no publisher keeps its bare name.
        let mut legacy = m.clone();
        legacy.publisher = String::new();
        assert_eq!(legacy.id(), "comments");
        assert!(!legacy.is_first_party());
    }

    #[test]
    fn third_party_may_not_squat_core_ids_or_kinds() {
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        m.publisher = "acme".into();
        m.name = "adr".into();

        // `comment:` belongs to the first-party set, so this must be rejected.
        let errs = m.validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("must start with `acme.adr.`"), "{errs:?}");

        // Namespaced under its own id, it is fine.
        if let Harvester::Regex { emit, .. } = &mut m.harvesters[0] {
            emit.node.id = "acme.adr.decision:{file}#{hash(text)}".into();
            emit.node.kind = "Decision".into();
        }
        m.node_types[0].kind = "Decision".into();
        assert!(m.validate().is_empty(), "{:?}", m.validate());

        // Core kinds stay reserved even under a namespaced id.
        m.node_types[0].kind = "File".into();
        assert!(m.validate().iter().any(|e| e.contains("reserved")));
    }

    #[test]
    fn tier_is_derived_from_capabilities() {
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        assert_eq!(m.tier(), Tier::Declarative);
        assert!(m.tier().is_supported());

        m.capabilities.push(Capability::Http {
            hosts: vec!["api.github.com".into()],
        });
        assert_eq!(m.tier(), Tier::Http);
        assert!(m.tier().is_supported(), "the declarative HTTP runner ships");

        // The highest capability wins, whatever order they are declared in.
        m.capabilities.push(Capability::Subprocess {
            commands: vec!["make".into()],
        });
        m.capabilities.push(Capability::Secret {
            names: vec!["GITHUB_TOKEN".into()],
            hosts: vec!["api.github.com".into()],
        });
        assert_eq!(m.tier(), Tier::Native);
        assert!(m.validate().is_empty(), "{:?}", m.validate());
    }

    #[test]
    fn listed_hosts_are_public_hostnames() {
        for bad in [
            "localhost",
            "*.localhost",
            "127.0.0.1",
            "[::1]",
            "169.254.169.254",
            "10.0.0.8",
            "redis.local",
            "*.internal",
            "printer.home.arpa",
        ] {
            let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
            m.capabilities.push(Capability::Http {
                hosts: vec![bad.into()],
            });
            let errs = m.validate();
            assert!(
                errs.iter().any(|e| e.contains("never this machine")),
                "{bad}: {errs:?}"
            );
        }
        assert!(!is_private_host("api.github.com"));
        assert!(!is_private_host("*.atlassian.net"));
        assert!(
            !is_private_host("internal.example.com"),
            "a label, not a TLD"
        );
    }

    #[test]
    fn a_secret_is_scoped_to_hosts_the_spore_may_reach() {
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        m.capabilities.push(Capability::Http {
            hosts: vec!["*.github.com".into()],
        });
        m.capabilities.push(Capability::Secret {
            names: vec!["githubToken".into()],
            hosts: vec![],
        });
        let errs = m.validate();
        assert!(
            errs.iter().any(|e| e.contains("which hosts its secrets")),
            "{errs:?}"
        );

        m.capabilities.pop();
        m.capabilities.push(Capability::Secret {
            names: vec!["githubToken".into()],
            hosts: vec!["cdn.example.com".into()],
        });
        let errs = m.validate();
        assert!(
            errs.iter().any(|e| e.contains("does not cover")),
            "{errs:?}"
        );

        m.capabilities.pop();
        m.capabilities.push(Capability::Secret {
            names: vec!["githubToken".into()],
            hosts: vec!["*.github.com".into()],
        });
        assert!(m.validate().is_empty(), "{:?}", m.validate());
        assert!(m.secret_allowed("githubToken", "api.github.com"));
        assert!(!m.secret_allowed("githubToken", "api.example.com"));
        assert!(!m.secret_allowed("other", "api.github.com"));
        assert!(
            m.capabilities[1]
                .consent_line()
                .ends_with("send it only to *.github.com")
        );
    }

    #[test]
    fn capabilities_must_be_specific() {
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        m.capabilities.push(Capability::Http { hosts: vec![] });
        m.capabilities.push(Capability::FsWrite {
            paths: vec!["*".into()],
        });
        let errs = m.validate();
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("must name what it needs")));
        assert!(errs.iter().any(|e| e.contains("may not request `*`")));
    }

    #[test]
    fn node_kinds_are_checked_without_harvesters() {
        // Regression: the kind check used to be nested inside the harvester
        // loop, so a manifest with no harvesters never validated its kinds and
        // one with N harvesters reported each problem N times.
        let mut m: SporeManifest = serde_json::from_str(COMMENTS).unwrap();
        m.harvesters.clear();
        m.node_types[0].kind = String::new();
        assert_eq!(m.validate().len(), 1, "{:?}", m.validate());
    }

    /// Rebuild every object with its keys in sorted order. `serde_json`'s map
    /// preserves insertion order whenever some other crate in the workspace
    /// turns on `preserve_order`, so without this the generated schema differs
    /// between `cargo test -p aneural-core` and `cargo test --workspace`.
    fn sorted(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    let v = map.get(&k).cloned().unwrap();
                    out.insert(k, sorted(v));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(sorted).collect())
            }
            other => other,
        }
    }

    /// The published contract third parties code against. Regenerate with
    /// `UPDATE_SCHEMA=1 cargo test -p aneural-core schema_matches_checked_in`.
    #[test]
    fn schema_matches_checked_in() {
        let schema = schemars::schema_for!(SporeManifest);
        let value = sorted(serde_json::to_value(&schema).unwrap());
        let generated = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/spore-v2.json");
        if std::env::var("UPDATE_SCHEMA").is_ok() {
            std::fs::write(&path, &generated).unwrap();
            return;
        }
        let checked_in = std::fs::read_to_string(&path).expect("schema/spore-v2.json is missing");
        assert_eq!(
            checked_in, generated,
            "schema/spore-v2.json is stale; rerun with UPDATE_SCHEMA=1"
        );
    }

    fn http_manifest(caps: serde_json::Value, req: serde_json::Value) -> SporeManifest {
        serde_json::from_value(serde_json::json!({
            "publisher": "acme",
            "name": "gh",
            "version": "0.1.0",
            "capabilities": caps,
            "settings": [{ "key": "repo" }],
            "harvesters": [{
                "kind": "http",
                "id": "pulls",
                "request": req,
                "emit": {
                    "node": { "kind": "Pull", "id": "acme.gh.pull:{item.number}", "label": "{item.title}" },
                    "edges": []
                }
            }]
        }))
        .unwrap()
    }

    #[test]
    fn a_web_request_must_be_covered_by_a_declared_capability() {
        let ok = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({ "url": "https://api.github.com/repos/{setting.repo}/pulls" }),
        );
        assert_eq!(ok.validate(), Vec::<String>::new());
        assert_eq!(ok.tier(), Tier::Http);
        assert!(ok.tier().is_supported(), "the T1 runner ships");

        // Declaring nothing means the consent sheet would say "needs nothing".
        let undeclared = http_manifest(
            serde_json::json!([]),
            serde_json::json!({ "url": "https://api.github.com/x" }),
        );
        assert!(
            undeclared
                .validate()
                .iter()
                .any(|e| e.contains("no `http` capability")),
            "{:?}",
            undeclared.validate()
        );

        // Declaring one host does not licence another.
        let wrong_host = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({ "url": "https://evil.example/x" }),
        );
        assert!(
            wrong_host
                .validate()
                .iter()
                .any(|e| e.contains("not in the declared")),
            "{:?}",
            wrong_host.validate()
        );
    }

    #[test]
    fn a_templated_host_is_refused_because_an_allowlist_cannot_check_it() {
        let m = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({ "url": "https://{setting.repo}/pulls" }),
        );
        assert!(
            m.validate().iter().any(|e| e.contains("literal host")),
            "{:?}",
            m.validate()
        );

        // Plaintext is refused by the same rule: no `https://` prefix, no host.
        let plain = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({ "url": "http://api.github.com/x" }),
        );
        assert!(
            plain.validate().iter().any(|e| e.contains("literal host")),
            "{:?}",
            plain.validate()
        );
    }

    #[test]
    fn a_secret_may_not_be_smuggled_into_a_url() {
        let caps = serde_json::json!([
            { "kind": "http", "hosts": ["api.github.com"] },
            { "kind": "secret", "names": ["githubToken"], "hosts": ["api.github.com"] }
        ]);
        let in_url = http_manifest(
            caps.clone(),
            serde_json::json!({ "url": "https://api.github.com/x?t={secret.githubToken}" }),
        );
        assert!(
            in_url
                .validate()
                .iter()
                .any(|e| e.contains("may not appear in a url")),
            "{:?}",
            in_url.validate()
        );

        // In a header it is fine — but only a secret the manifest declared.
        let in_header = http_manifest(
            caps,
            serde_json::json!({
                "url": "https://api.github.com/x",
                "headers": { "Authorization": "Bearer {secret.githubToken}" }
            }),
        );
        assert_eq!(in_header.validate(), Vec::<String>::new());

        let undeclared = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({
                "url": "https://api.github.com/x",
                "headers": { "Authorization": "Bearer {secret.sneaky}" }
            }),
        );
        assert!(
            undeclared
                .validate()
                .iter()
                .any(|e| e.contains("does not declare")),
            "{:?}",
            undeclared.validate()
        );

        // Declared, but for a different host than the one this request dials.
        let elsewhere = http_manifest(
            serde_json::json!([
                { "kind": "http", "hosts": ["api.github.com", "cdn.example.com"] },
                { "kind": "secret", "names": ["githubToken"], "hosts": ["api.github.com"] }
            ]),
            serde_json::json!({
                "url": "https://cdn.example.com/x",
                "headers": { "Authorization": "Bearer {secret.githubToken}" }
            }),
        );
        assert!(
            elsewhere
                .validate()
                .iter()
                .any(|e| e.contains("that secret's hosts do not include")),
            "{:?}",
            elsewhere.validate()
        );
    }

    #[test]
    fn polling_has_a_floor_and_pagination_a_ceiling() {
        let mut m = http_manifest(
            serde_json::json!([{ "kind": "http", "hosts": ["api.github.com"] }]),
            serde_json::json!({ "url": "https://api.github.com/x" }),
        );
        let Harvester::Http {
            refresh_seconds,
            max_pages,
            ..
        } = &mut m.harvesters[0]
        else {
            panic!("http harvester")
        };
        // Defaults are sane without the manifest saying anything.
        assert_eq!(*refresh_seconds, DEFAULT_REFRESH_SECONDS);
        assert_eq!(*max_pages, 1);
        *refresh_seconds = 5;
        *max_pages = 99;
        let errs = m.validate();
        assert!(errs.iter().any(|e| e.contains("floor")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("maxPages")), "{errs:?}");
    }

    #[test]
    fn wildcard_hosts_cover_subdomains_only() {
        assert!(host_matches("api.github.com", "api.github.com"));
        assert!(!host_matches("api.github.com", "evil.api.github.com"));
        assert!(host_matches("*.atlassian.net", "acme.atlassian.net"));
        // The bare parent is not a subdomain of itself.
        assert!(!host_matches("*.atlassian.net", "atlassian.net"));
        // ...and it must not match a lookalike suffix.
        assert!(!host_matches("*.atlassian.net", "evilatlassian.net"));
    }

    #[test]
    fn id_templates_need_a_literal_prefix() {
        assert_eq!(template_prefix("comment:{file}#{h}"), Some("comment"));
        assert_eq!(
            template_prefix("acme.adr.decision:{file}"),
            Some("acme.adr.decision")
        );
        assert_eq!(template_prefix("{kind}:{file}"), None);
        assert_eq!(template_prefix("nocolon"), None);
        assert_eq!(template_prefix(":x"), None);
    }
}
