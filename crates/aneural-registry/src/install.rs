//! Resolving, consenting to, and installing a spore.
//!
//! [`plan`] and [`commit`] are deliberately separate: everything that can fail
//! or surprise the user happens in `plan`, which writes nothing, and the consent
//! sheet sits between the two. Both the GUI and the CLI render the same plan.

use crate::client::Federation;
use crate::index::Entry;
use crate::lock::{LockEntry, Lockfile};
use crate::{Error, Result};
use aneural_core::config::{Config, SporesConfig};
use aneural_core::spore::{Capability, SporeManifest, Tier};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the user agreed to at the consent sheet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grants {
    pub capabilities: Vec<Capability>,
}

/// Everything needed to decide whether to install, and to then do it. Holding
/// the verified bytes means `commit` cannot fetch anything new.
#[derive(Debug)]
pub struct Plan {
    pub id: String,
    pub entry: Entry,
    pub manifest: SporeManifest,
    pub registry: String,
    pub registry_url: String,
    pub files: BTreeMap<String, Vec<u8>>,
    pub tier: Tier,
    /// The version being replaced, when this is an update.
    pub previous: Option<String>,
    /// Capabilities this version asks for that the installed one did not.
    pub new_capabilities: Vec<Capability>,
}

impl Plan {
    /// Whether the consent sheet must be shown before committing. A first
    /// install always consents; an update only when it asks for more.
    pub fn requires_consent(&self) -> bool {
        self.previous.is_none() || !self.new_capabilities.is_empty()
    }

    /// The plain-English lines the consent sheet shows.
    pub fn consent_lines(&self) -> Vec<String> {
        if self.manifest.capabilities.is_empty() {
            return vec![
                "Nothing. It only reads files you already index, using patterns declared in its manifest."
                    .to_string(),
            ];
        }
        self.manifest
            .capabilities
            .iter()
            .map(Capability::consent_line)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    pub tier: Tier,
    pub enabled: bool,
    pub previous: Option<String>,
}

fn spores_dir(root: &Path) -> PathBuf {
    aneural_core::Workspace::at(root).spores_dir()
}

/// Resolve and verify, writing nothing.
pub fn plan(root: &Path, fed: &Federation, id: &str) -> Result<Plan> {
    let (client, entry) = fed.resolve(id)?;

    if let Some(rev) = fed.revocation(id, &entry.version) {
        return Err(Error::Revoked {
            id: id.to_string(),
            reason: if rev.reason.is_empty() {
                "no reason given".into()
            } else {
                rev.reason.clone()
            },
        });
    }

    let problems = entry.validate();
    if !problems.is_empty() {
        return Err(Error::InvalidManifest {
            id: id.to_string(),
            problems: problems.join("; "),
        });
    }

    // Fetch every published file, each verified against its pinned hash.
    let mut files = BTreeMap::new();
    for name in entry.files.keys() {
        files.insert(name.clone(), client.fetch_file(&entry, name)?);
    }

    let manifest_bytes = files
        .get(crate::MANIFEST_FILE)
        .expect("entry.validate() requires a manifest");
    let manifest: SporeManifest = serde_json::from_slice(manifest_bytes)?;

    if manifest.id() != entry.id {
        return Err(Error::InvalidManifest {
            id: id.to_string(),
            problems: format!(
                "manifest says `{}` but the index lists it as `{}`",
                manifest.id(),
                entry.id
            ),
        });
    }

    let problems = manifest.validate();
    if !problems.is_empty() {
        return Err(Error::InvalidManifest {
            id: id.to_string(),
            problems: problems.join("; "),
        });
    }

    // An index that understates what a spore will do is an attack, not a typo.
    let undeclared: Vec<&Capability> = manifest
        .capabilities
        .iter()
        .filter(|c| !entry.capabilities.contains(c))
        .collect();
    if !undeclared.is_empty() {
        return Err(Error::UndeclaredCapability {
            id: id.to_string(),
            extra: undeclared
                .iter()
                .map(|c| c.consent_line())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }

    let tier = manifest.tier();
    if !tier.is_supported() {
        return Err(Error::UnsupportedTier {
            id: id.to_string(),
            tier: tier.label(),
        });
    }

    check_kind_collisions(root, &manifest)?;

    let lock = Lockfile::load(root)?;
    let existing = lock.get(id);
    let previous = existing.map(|e| e.version.clone());
    let new_capabilities = match existing {
        Some(e) => manifest
            .capabilities
            .iter()
            .filter(|c| !e.granted.contains(c))
            .cloned()
            .collect(),
        None => manifest.capabilities.clone(),
    };

    Ok(Plan {
        id: id.to_string(),
        registry: client.name().to_string(),
        registry_url: client.url().to_string(),
        entry,
        manifest,
        files,
        tier,
        previous,
        new_capabilities,
    })
}

/// A spore may not claim a node kind another installed spore already provides.
/// Rejecting here beats warning later: the user never ends up with a graph whose
/// styles silently changed under them.
fn check_kind_collisions(root: &Path, manifest: &SporeManifest) -> Result<()> {
    let dir = spores_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let path = e.path().join(crate::MANIFEST_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(other) = serde_json::from_str::<SporeManifest>(&text) else {
            continue;
        };
        if other.id() == manifest.id() {
            continue; // updating ourselves
        }
        for kind in manifest.node_types.iter().map(|n| &n.kind) {
            if other.node_types.iter().any(|n| &n.kind == kind) {
                return Err(Error::Invalid(format!(
                    "`{}` declares node kind `{kind}`, which `{}` already provides",
                    manifest.id(),
                    other.id()
                )));
            }
        }
    }
    Ok(())
}

/// Write the verified package, update the lockfile, and enable the spore.
pub fn commit(root: &Path, plan: &Plan, grants: &Grants, enable: bool) -> Result<Installed> {
    let dir = spores_dir(root).join(&plan.id);
    std::fs::create_dir_all(&dir)?;

    let mut hashes = BTreeMap::new();
    for (name, bytes) in &plan.files {
        // The entry allowlist already rejected anything else, but a package file
        // name is attacker-controlled, so never let one become a path.
        if !crate::ALLOWED_FILES.contains(&name.as_str()) {
            return Err(Error::Invalid(format!(
                "`{name}` is not an allowed package file"
            )));
        }
        std::fs::write(dir.join(name), bytes)?;
        hashes.insert(name.clone(), crate::sha256_hex(bytes));
    }

    let mut lock = Lockfile::load(root)?;
    lock.spores.insert(
        plan.id.clone(),
        LockEntry {
            version: plan.entry.version.clone(),
            registry: plan.registry.clone(),
            registry_url: plan.registry_url.clone(),
            repo: plan.entry.repo.clone(),
            path: plan.entry.path.clone(),
            tier: plan.tier.label().to_string(),
            granted: grants.capabilities.clone(),
            integrity: LockEntry::compute_integrity(&hashes),
            files: hashes,
            installed_at: aneural_core::now_rfc3339(),
        },
    );
    lock.save(root)?;

    if enable {
        set_enabled(root, &plan.id, true)?;
    }

    Ok(Installed {
        id: plan.id.clone(),
        version: plan.entry.version.clone(),
        dir,
        tier: plan.tier,
        enabled: enable,
        previous: plan.previous.clone(),
    })
}

pub fn uninstall(root: &Path, id: &str) -> Result<()> {
    let dir = spores_dir(root).join(id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)?;
    }
    let mut lock = Lockfile::load(root)?;
    lock.spores.remove(id);
    lock.save(root)?;
    set_enabled(root, id, false)?;
    Ok(())
}

/// Record a value for one of a spore's declared settings.
///
/// Settings are not credentials. A `{secret.*}` never comes from here — it comes
/// from the environment — so this file staying in the repo is fine, and is in
/// fact the point: a team can commit which repository or JIRA project a spore
/// watches without committing anyone's token.
pub fn set_setting(root: &Path, id: &str, key: &str, value: Option<&str>) -> Result<Config> {
    let ws = aneural_core::Workspace::at(root);
    let mut config = ws.load_config()?;
    let entry = config.spores.settings.entry(id.to_string()).or_default();
    match value {
        Some(v) => {
            entry.insert(key.to_string(), v.to_string());
        }
        None => {
            entry.remove(key);
        }
    }
    if entry.is_empty() {
        config.spores.settings.remove(id);
    }
    ws.save_config(&config)?;
    Ok(config)
}

/// Toggle a spore in `config.spores.enabled`, migrating the block on the way
/// through — a mutating action is exactly when persisting a migration is fair.
pub fn set_enabled(root: &Path, id: &str, on: bool) -> Result<Config> {
    let ws = aneural_core::Workspace::at(root);
    let mut config = ws.load_config()?;
    let name = id.split_once('.').map(|(_, n)| n).unwrap_or(id);

    config.spores.migrate(&crate::FIRST_PARTY_NAMES);
    config
        .spores
        .enabled
        .retain(|e| !SporesConfig::entry_matches(e, id, name));
    if on {
        config.spores.enabled.push(id.to_string());
    }
    ws.save_config(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StaticIndex;
    use crate::index::Index;
    use crate::transport::DirTransport;

    struct Fixture {
        tmp: tempfile::TempDir,
        registry: PathBuf,
    }

    fn manifest_json(id: &str, kind: &str, caps: &str) -> String {
        let (publisher, name) = id.split_once('.').unwrap();
        format!(
            r##"{{
              "publisher": "{publisher}", "name": "{name}", "version": "1.0.0",
              "displayName": "Test", "description": "d",
              "capabilities": [{caps}],
              "nodeTypes": [{{ "kind": "{kind}", "icon": "LuCircleDot", "color": "#ffffff" }}],
              "harvesters": [{{
                "id": "h", "kind": "markdown", "include": ["**/*.md"],
                "emit": {{ "node": {{ "kind": "{kind}", "id": "{id}.item:{{file}}", "label": "{{title}}" }}, "edges": [] }}
              }}]
            }}"##
        )
    }

    fn fixture(manifest: &str, caps_in_index: Vec<Capability>) -> (Fixture, Index) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        aneural_core::Workspace::at(&root)
            .init(Some("test"), false)
            .unwrap();

        let registry = root.join("registry");
        std::fs::create_dir_all(registry.join("pkg")).unwrap();
        std::fs::write(registry.join("pkg").join(crate::MANIFEST_FILE), manifest).unwrap();

        let parsed: SporeManifest = serde_json::from_str(manifest).unwrap();
        let index = Index {
            version: 1,
            spores: vec![Entry {
                id: parsed.id(),
                version: parsed.version.clone(),
                repo: "pkg".into(),
                files: BTreeMap::from([(
                    crate::MANIFEST_FILE.into(),
                    crate::sha256_hex(manifest.as_bytes()),
                )]),
                capabilities: caps_in_index,
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            registry.join("index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        (Fixture { tmp, registry }, index)
    }

    fn federation(f: &Fixture) -> Federation {
        Federation::new(vec![Box::new(StaticIndex::new(
            "official",
            "index.json",
            DirTransport::new(&f.registry),
        ))])
    }

    #[test]
    fn installs_verifies_and_locks() {
        let m = manifest_json("acme.adr", "Decision", "");
        let (f, _) = fixture(&m, vec![]);
        let root = f.tmp.path();
        let fed = federation(&f);

        let p = plan(root, &fed, "acme.adr").unwrap();
        assert_eq!(p.tier, Tier::Declarative);
        assert!(p.requires_consent(), "a first install always consents");
        assert!(p.consent_lines()[0].starts_with("Nothing."));

        let out = commit(root, &p, &Grants::default(), true).unwrap();
        assert_eq!(out.version, "1.0.0");
        assert!(out.dir.join(crate::MANIFEST_FILE).is_file());

        let lock = Lockfile::load(root).unwrap();
        let entry = lock.get("acme.adr").unwrap();
        assert_eq!(entry.registry, "official");
        assert_eq!(entry.tier, "declarative");
        assert!(lock.verify(root).unwrap().is_empty());

        let config = aneural_core::Workspace::at(root).load_config().unwrap();
        assert!(config.spores.enabled.contains(&"acme.adr".to_string()));
    }

    #[test]
    fn refuses_a_tier_with_no_runner_and_writes_nothing() {
        let caps = r#"{ "kind": "tcp", "endpoints": ["redis.internal:6379"] }"#;
        let m = manifest_json("acme.redis", "Key", caps);
        let (f, _) = fixture(
            &m,
            vec![Capability::Tcp {
                endpoints: vec!["redis.internal:6379".into()],
            }],
        );
        let root = f.tmp.path();

        let err = plan(root, &federation(&f), "acme.redis").unwrap_err();
        assert!(matches!(err, Error::UnsupportedTier { .. }), "{err:?}");
        assert!(!spores_dir(root).join("acme.redis").exists());
    }

    #[test]
    fn a_native_listing_is_refused_before_anything_is_fetched() {
        // Not "no runner yet" — never. The sandbox is the ceiling for anything
        // a registry can hand out, however honestly the listing advertises it.
        let caps = r#"{ "kind": "subprocess", "commands": ["make"] }"#;
        let m = manifest_json("acme.ci", "Job", caps);
        let (f, _) = fixture(
            &m,
            vec![Capability::Subprocess {
                commands: vec!["make".into()],
            }],
        );
        let root = f.tmp.path();

        let err = plan(root, &federation(&f), "acme.ci").unwrap_err();
        match err {
            Error::InvalidManifest { problems, .. } => {
                assert!(problems.contains("never listed"), "{problems}")
            }
            other => panic!("{other:?}"),
        }
        assert!(!spores_dir(root).join("acme.ci").exists());
    }

    #[test]
    fn refuses_a_manifest_asking_for_more_than_its_listing() {
        let caps = r#"{ "kind": "http", "hosts": ["evil.example"] }"#;
        let m = manifest_json("acme.sneaky", "Thing", caps);
        // The index advertises nothing at all.
        let (f, _) = fixture(&m, vec![]);
        let err = plan(f.tmp.path(), &federation(&f), "acme.sneaky").unwrap_err();
        assert!(matches!(err, Error::UndeclaredCapability { .. }), "{err:?}");
    }

    #[test]
    fn refuses_a_kind_another_spore_already_provides() {
        let first = manifest_json("acme.adr", "Decision", "");
        let (f, _) = fixture(&first, vec![]);
        let root = f.tmp.path();
        let p = plan(root, &federation(&f), "acme.adr").unwrap();
        commit(root, &p, &Grants::default(), true).unwrap();

        // A second spore claiming the same kind must not silently repaint it.
        let second = manifest_json("bob.decisions", "Decision", "");
        std::fs::create_dir_all(f.registry.join("pkg2")).unwrap();
        std::fs::write(f.registry.join("pkg2").join(crate::MANIFEST_FILE), &second).unwrap();
        let index = Index {
            version: 1,
            spores: vec![Entry {
                id: "bob.decisions".into(),
                version: "1.0.0".into(),
                repo: "pkg2".into(),
                files: BTreeMap::from([(
                    crate::MANIFEST_FILE.into(),
                    crate::sha256_hex(second.as_bytes()),
                )]),
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            f.registry.join("index2.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let fed2 = Federation::new(vec![Box::new(StaticIndex::new(
            "official",
            "index2.json",
            DirTransport::new(&f.registry),
        ))]);
        let err = plan(root, &fed2, "bob.decisions").unwrap_err();
        assert!(err.to_string().contains("already provides"), "{err}");
    }

    #[test]
    fn uninstall_removes_files_lock_and_config_entry() {
        let m = manifest_json("acme.adr", "Decision", "");
        let (f, _) = fixture(&m, vec![]);
        let root = f.tmp.path();
        let p = plan(root, &federation(&f), "acme.adr").unwrap();
        commit(root, &p, &Grants::default(), true).unwrap();

        uninstall(root, "acme.adr").unwrap();
        assert!(!spores_dir(root).join("acme.adr").exists());
        assert!(Lockfile::load(root).unwrap().get("acme.adr").is_none());
        let config = aneural_core::Workspace::at(root).load_config().unwrap();
        assert!(!config.spores.enabled.contains(&"acme.adr".to_string()));
    }
}
