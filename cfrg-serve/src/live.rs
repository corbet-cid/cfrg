//! The live relay: `cfrg_relay::apply` behind the [`Relay`] seam.
//!
//! `apply` is synchronous and blocks (database, HTTP through its own current-thread runtime), so it runs on a
//! blocking thread. The database connection string is built from the files of the credentials directory (a mounted Secret: `host`, `port`, `dbname`, `username`, `password`), verified against the cluster CA.
use crate::{
    config::{RelayConfig, Result},
    relay::{Relay, RelayFuture},
};
use cfrg::relay::{Hint, Outcome};
use cfrg_relay::{Applied, Settings};
use std::{fs, path::Path, sync::Arc};

pub struct LiveRelay {
    settings: Arc<Settings>,
}

fn libpq_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The libpq connection string of the mapping database from the credential files.
pub fn database_string(dir: &Path, ca: Option<&str>) -> Result<String> {
    let read = |name: &str| -> Result<String> {
        Ok(fs::read_to_string(dir.join(name))
            .map_err(|e| format!("database credential {name}: {e}"))?
            .trim()
            .to_owned())
    };
    let mut parts = vec![
        format!("host={}", libpq_quote(&read("host")?)),
        format!("port={}", libpq_quote(&read("port")?)),
        format!("dbname={}", libpq_quote(&read("dbname")?)),
        format!("user={}", libpq_quote(&read("username")?)),
        format!("password={}", libpq_quote(&read("password")?)),
        "sslmode=verify-full".to_owned(),
    ];
    if let Some(ca) = ca {
        parts.push(format!("sslrootcert={}", libpq_quote(ca)));
    }
    Ok(parts.join(" "))
}

impl LiveRelay {
    /// `Ok(None)` when the database credential is not mounted yet: the relay routes then refuse (503).
    pub fn from_config(config: &RelayConfig) -> Result<Option<Self>> {
        let dir = Path::new(&config.credentials_dir);
        if !dir.join("password").exists() {
            return Ok(None);
        }
        let settings = Settings {
            config: config.config.clone().into(),
            placement: config.placement.clone().into(),
            database: database_string(dir, config.database_ca.as_deref())?,
            credentials_dir: Some(dir.to_path_buf()),
        };
        Ok(Some(Self {
            settings: Arc::new(settings),
        }))
    }
}

impl LiveRelay {
    /// The release intent journal in the same database as the relay.
    pub fn journal(&self) -> crate::journal::PgJournal {
        crate::journal::PgJournal::new(Arc::clone(&self.settings))
    }
}

impl Relay for LiveRelay {
    fn apply(&self, hint: Hint) -> RelayFuture<'_> {
        let settings = Arc::clone(&self.settings);
        Box::pin(async move {
            let applied = tokio::task::spawn_blocking(move || cfrg_relay::apply(&settings, &hint))
                .await
                .map_err(|e| format!("relay task: {e}"))?
                .map_err(|e| e.to_string())?;
            match applied {
                Applied::Refused(why) => Err(format!("relay refused: {why}")),
                Applied::Done(report) => match report.outcome {
                    // Nothing could be done now (busy lease, outage, kill switch): the sender's redelivery retries.
                    Outcome::Deferred(why) => Err(format!("relay deferred: {why:?}")),
                    _ => Ok(()),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connection_string_comes_from_the_secret_files_and_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("host", "db.example"),
            ("port", "5432"),
            ("dbname", "forge_bridge"),
            ("username", "relay"),
            ("password", "p'w d\n"),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let text = database_string(dir.path(), Some("/ca/ca.crt")).unwrap();
        assert_eq!(
            text,
            "host='db.example' port='5432' dbname='forge_bridge' user='relay' password='p\\'w d' sslmode=verify-full sslrootcert='/ca/ca.crt'"
        );
        assert!(database_string(&dir.path().join("missing"), None).is_err());
    }

    #[test]
    fn without_the_database_credential_there_is_no_relay() {
        let dir = tempfile::tempdir().unwrap();
        let config = RelayConfig {
            config: "/x".into(),
            placement: "/y".into(),
            credentials_dir: dir.path().to_str().unwrap().into(),
            database_ca: None,
        };
        assert!(LiveRelay::from_config(&config).unwrap().is_none());
    }
}
