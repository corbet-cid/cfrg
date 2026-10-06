//! Versioned primary placement shared with the pointer Worker.
use crate::{failure, Error};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub version: u32,
    pub default: String,
    pub primaries: BTreeMap<String, String>,
}

impl Placement {
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != 1 || self.primaries.len() > 100_000 {
            return Err(failure("Unsupported placement schema or size"));
        }
        valid_base(&self.default)?;
        for (path, base) in &self.primaries {
            if path.split('/').count() != 2
                || path.split('/').any(|part| {
                    part.is_empty()
                        || part == "."
                        || part == ".."
                        || !part.bytes().all(|b| {
                            b.is_ascii_lowercase()
                                || b.is_ascii_digit()
                                || matches!(b, b'_' | b'-' | b'.')
                        })
                })
                || base == &self.default
            {
                return Err(failure("Invalid placement exception"));
            }
            valid_base(base)?;
        }
        Ok(())
    }

    pub fn primary(&self, path: &str) -> &str {
        self.primaries.get(path).unwrap_or(&self.default)
    }
}

fn valid_base(base: &str) -> Result<(), Error> {
    if !base.starts_with("https://")
        || crate::normalize_origin(base).is_none()
        || base.contains(['?', '#', '@', '%', '\\'])
        || base.chars().any(char::is_whitespace)
    {
        return Err(failure("Placement requires a bare HTTPS origin"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_foreign_primary() {
        let p: Placement = serde_json::from_str(
            r#"{"version":1,"default":"https://home.example","primaries":{"acme/foreign":"https://github.com"}}"#,
        )
        .unwrap();
        p.validate().unwrap();
        assert_eq!(p.primary("acme/local"), "https://home.example");
        assert_eq!(p.primary("acme/foreign"), "https://github.com");
    }

    #[test]
    fn malformed_placement_is_rejected() {
        for document in [
            r#"{"version":2,"default":"https://home.example","primaries":{}}"#,
            r#"{"version":1,"default":"https://user@home.example","primaries":{}}"#,
            r#"{"version":1,"default":"https://home.example/path","primaries":{}}"#,
            r#"{"version":1,"default":"https://home.example","primaries":{"Acme/repo":"https://github.com"}}"#,
            r#"{"version":1,"default":"https://home.example","primaries":{"acme/repo":"https://home.example"}}"#,
            r#"{"version":1,"default":"https://home.example","primaries":{},"unknown":true}"#,
        ] {
            assert!(serde_json::from_str::<Placement>(document)
                .map(|p| p.validate().is_err())
                .unwrap_or(true));
        }
    }
}
