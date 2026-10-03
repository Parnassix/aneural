//! The Open Spores Marketplace: registry indexes, verified fetch, install, and
//! the workspace lockfile.
//!
//! This is the only crate in the workspace that reaches the network. Everything
//! it downloads is third-party content, so the rules are deliberately strict:
//!
//! * every file is pinned by sha256 in the index and verified after download;
//! * a manifest whose declared capabilities exceed what the index advertised is
//!   rejected, because an index that understates a spore is an attack;
//! * a spore whose tier this build has no runner for will not install;
//! * nothing is ever overwritten without the lockfile agreeing.

pub mod client;
pub mod fetcher;
pub mod index;
pub mod install;
pub mod lock;
pub mod search;
pub mod transport;

pub use client::{Federation, RegistryClient, RegistryStatus, StaticIndex, cache_dir, federation};
pub use fetcher::UreqFetcher;
pub use index::{Entry, Index, Revocation};
pub use install::{Grants, Installed, Plan, commit, plan, set_enabled, set_setting, uninstall};
pub use lock::{Drift, LockEntry, Lockfile};
pub use search::{Hit, search};
pub use transport::{DirTransport, Fetched, HttpTransport, MixedTransport, Transport};

/// The registry the official first-party spores are published to.
pub const OFFICIAL_REGISTRY: &str = aneural_core::config::OFFICIAL_REGISTRY_URL;

/// Shown before every install, in the GUI consent sheet, on the CLI, and in the
/// docs. Defined once so the three cannot drift apart.
pub const DISCLAIMER: &str = "Spores are published by third parties. Aneural does not review, \
    endorse, or guarantee them, and they are provided without warranty. Installing a spore \
    means you accept its publisher's terms and whatever risk it carries.";

/// `spore.json` is mandatory; the rest of a package is optional.
pub const MANIFEST_FILE: &str = "spore.json";
pub const README_FILE: &str = "README.md";
pub const ICON_FILE: &str = "icon.svg";

/// Files a package is allowed to contain. A registry entry naming anything else
/// is rejected before a single byte is downloaded.
pub const ALLOWED_FILES: &[&str] = &[MANIFEST_FILE, README_FILE, ICON_FILE];

/// The first-party spores, for migrating bare `enabled` entries.
pub const FIRST_PARTY_NAMES: [&str; 4] = ["comments", "plans", "icebox", "wiki-links"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("core: {0}")]
    Core(#[from] aneural_core::Error),
    #[error("{url} responded {status}")]
    Status { url: String, status: u16 },
    #[error("could not reach {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("{url} is larger than the {limit} byte limit for a spore package")]
    TooLarge { url: String, limit: usize },
    #[error("sha256 mismatch for {file} of `{id}`: expected {expected}, got {actual}")]
    Hash {
        id: String,
        file: String,
        expected: String,
        actual: String,
    },
    #[error("`{0}` is not in any configured registry")]
    NotFound(String),
    #[error("`{id}` has been revoked: {reason}")]
    Revoked { id: String, reason: String },
    #[error("`{id}` needs the `{tier}` runtime, which this version of Aneural does not ship")]
    UnsupportedTier { id: String, tier: &'static str },
    #[error("`{id}` is invalid: {problems}")]
    InvalidManifest { id: String, problems: String },
    #[error(
        "`{id}` asks for more than its listing declared ({extra}); refusing to install without fresh consent"
    )]
    UndeclaredCapability { id: String, extra: String },
    #[error("`{id}` is already installed from a different source ({have}); remove it first")]
    SourceConflict { id: String, have: String },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Lowercase hex sha256 of a byte slice. The registry format specifies sha256,
/// so this is the one place the workspace does not use blake3.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_reference_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
