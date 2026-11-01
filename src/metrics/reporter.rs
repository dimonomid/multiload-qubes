//! Reporter keys select streams; qube identities are checked separately.
//!
//! Xen can reuse a domain ID, and the configured network qube can change.
//! The key stays the same in those cases, so it cannot identify the running qube.
//! Filenames and JSON manifest keys use the same conversion rules below.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
// JSON map keys must be strings. These Serde hooks use Display/FromStr through
// the From/TryFrom adapters, keeping filenames and manifest keys in the same form.
#[serde(try_from = "String", into = "String")]
pub enum ReporterKey {
    /// General metrics for a Xen domain. The filename remains memory-{id}.
    QubeMetrics(u32),
    /// The single network stream; its qube's identity is stored separately.
    Network,
}

impl fmt::Display for ReporterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QubeMetrics(id) => write!(f, "memory-{id}"),
            Self::Network => f.write_str("network"),
        }
    }
}
impl FromStr for ReporterKey {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if name == "network" {
            return Ok(Self::Network);
        }
        let id = name
            .strip_prefix("memory-")
            .ok_or("invalid reporter filename")?;
        let number = id
            .parse::<u32>()
            .map_err(|_| "invalid reporter domain ID")?;
        // Each key must have one filename. Accepting memory-01 would turn it
        // into memory-1 when a later read builds the path again.
        if id != number.to_string() {
            return Err("noncanonical reporter filename");
        }
        Ok(Self::QubeMetrics(number))
    }
}
impl From<ReporterKey> for String {
    fn from(key: ReporterKey) -> Self {
        key.to_string()
    }
}
impl TryFrom<String> for ReporterKey {
    type Error = &'static str;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        name.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn filenames_and_json_keys_keep_the_existing_layout() {
        let expected = BTreeMap::from([
            (ReporterKey::QubeMetrics(23), "23/uuid"),
            (ReporterKey::Network, "42/network-uuid"),
        ]);
        assert_eq!(ReporterKey::QubeMetrics(23).to_string(), "memory-23");
        assert_eq!(ReporterKey::Network.to_string(), "network");
        let json = serde_json::to_string(&expected).unwrap();
        assert_eq!(
            json,
            r#"{"memory-23":"23/uuid","network":"42/network-uuid"}"#
        );
        assert_eq!(
            serde_json::from_str::<BTreeMap<ReporterKey, &str>>(&json).unwrap(),
            expected
        );
        for key in [
            ReporterKey::QubeMetrics(0),
            ReporterKey::QubeMetrics(u32::MAX),
            ReporterKey::Network,
        ] {
            assert_eq!(key.to_string().parse::<ReporterKey>().unwrap(), key);
        }
    }

    #[test]
    fn rejects_unknown_unsafe_and_noncanonical_names() {
        for name in [
            "",
            "memory-",
            "memory--1",
            "memory-+1",
            "memory-01",
            "memory-4294967296",
            "memory-1/extra",
            "../network",
            "network/extra",
            ".metrics-tmp",
            "other",
        ] {
            assert!(name.parse::<ReporterKey>().is_err(), "{name}");
            let json = format!(r#"{{"{name}":"identity"}}"#);
            assert!(
                serde_json::from_str::<BTreeMap<ReporterKey, String>>(&json).is_err(),
                "{name}"
            );
        }
    }
}
