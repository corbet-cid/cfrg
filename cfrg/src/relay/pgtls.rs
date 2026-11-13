//! TLS for the PostgreSQL connection (`rustls` through `tokio-postgres-rustls`).
//!
//! The connection string is a libpq-style URL or key/value string. This module reads two libpq options out of it
//! before the client sees it, because the client has no `verify-full`:
//!
//! * `sslmode`: `disable` (local hosts only), `require`, `verify-ca`, `verify-full`. A non-local host without
//!   `sslmode` gets `verify-full`; a local host (localhost, a loopback address, a unix socket) without it gets
//!   plain TCP. `require`, `verify-ca` and `verify-full` all verify the server chain and its name: stricter than
//!   libpq's `require`, never weaker, so a mode can only fail closed.
//! * `sslrootcert`: a PEM file of the fleet CA. Without it the system roots are used.

use crate::Result;
use postgres::config::Host;
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::{pem::PemObject, CertificateDer};
use std::{path::Path, sync::Arc};
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Disable,
    /// TLS, chain and name verified.
    Verify,
}

#[derive(Debug)]
pub struct Parsed {
    /// The connection string without the options this module consumed.
    pub config: String,
    pub mode: Mode,
    pub root_cert: Option<String>,
}

fn is_local(host: &Host) -> bool {
    match host {
        Host::Tcp(name) => {
            name == "localhost"
                || name
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        Host::Unix(_) => true,
    }
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .unwrap_or(value)
}

/// Splits `sslmode` and `sslrootcert` out of `config` and decides the mode.
pub fn parse(config: &str) -> Result<Parsed> {
    let (mut mode, mut root_cert) = (None, None);
    let mut take = |key: &str, value: &str| {
        match key {
            "sslmode" => mode = Some(value.to_owned()),
            "sslrootcert" => root_cert = Some(value.to_owned()),
            _ => return false,
        }
        true
    };
    let rest = if config.starts_with("postgres://") || config.starts_with("postgresql://") {
        let (base, query) = config.split_once('?').unwrap_or((config, ""));
        let kept: Vec<&str> = query
            .split('&')
            .filter(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                !take(k, v)
            })
            .collect();
        if kept.iter().all(|p| p.is_empty()) {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        }
    } else {
        config
            .split_whitespace()
            .filter(|token| {
                let (k, v) = token.split_once('=').unwrap_or((token, ""));
                !take(k, unquote(v))
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let parsed: postgres::Config = rest.parse()?;
    let local = !parsed.get_hosts().is_empty() && parsed.get_hosts().iter().all(is_local);
    let mode = match (mode.as_deref(), local) {
        (Some("disable"), true) | (None, true) => Mode::Disable,
        (Some("disable"), false) => {
            return Err("sslmode=disable is only allowed for a local PostgreSQL host".into())
        }
        (Some("require" | "verify-ca" | "verify-full") | None, _) => Mode::Verify,
        (Some(other), _) => return Err(format!("unsupported sslmode `{other}`").into()),
    };
    Ok(Parsed {
        config: rest,
        mode,
        root_cert,
    })
}

fn roots(file: Option<&str>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    if let Some(file) = file {
        let certs = CertificateDer::pem_file_iter(Path::new(file))
            .map_err(|e| format!("sslrootcert {file}: {e}"))?;
        for cert in certs {
            store.add(cert.map_err(|e| format!("sslrootcert {file}: {e}"))?)?;
        }
    } else {
        let loaded = rustls_native_certs::load_native_certs();
        for cert in loaded.certs {
            // A system root the verifier cannot parse is skipped; the others still work.
            let _ = store.add(cert);
        }
    }
    if store.is_empty() {
        return Err("no trusted CA certificates for the PostgreSQL connection".into());
    }
    Ok(store)
}

/// The connector for a verified connection.
pub fn connector(root_cert: Option<&str>) -> Result<MakeRustlsConnect> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots(root_cert)?)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts_default_to_plain_and_others_to_verify_full() {
        assert_eq!(parse("host=localhost user=a").unwrap().mode, Mode::Disable);
        assert_eq!(
            parse("postgres://u@127.0.0.1/db").unwrap().mode,
            Mode::Disable
        );
        assert_eq!(
            parse("host=/run/postgresql user=a").unwrap().mode,
            Mode::Disable
        );
        assert_eq!(parse("host=db.example user=a").unwrap().mode, Mode::Verify);
        assert_eq!(
            parse("postgres://u@db.example/d").unwrap().mode,
            Mode::Verify
        );
    }

    #[test]
    fn modes_are_checked_and_options_are_consumed() {
        assert!(parse("host=db.example sslmode=disable").is_err());
        assert!(parse("host=db.example sslmode=allow").is_err());
        let p = parse("host=db.example sslmode=require sslrootcert='/etc/ca.pem' user=a").unwrap();
        assert_eq!(
            (p.mode, p.root_cert.as_deref()),
            (Mode::Verify, Some("/etc/ca.pem"))
        );
        assert_eq!(p.config, "host=db.example user=a");
        let p = parse(
            "postgres://u@db.example/d?sslmode=verify-full&sslrootcert=/ca.pem&connect_timeout=3",
        )
        .unwrap();
        assert_eq!(p.config, "postgres://u@db.example/d?connect_timeout=3");
        let p = parse("postgres://u@localhost/d?sslmode=require").unwrap();
        assert_eq!(
            (p.mode, p.config.as_str()),
            (Mode::Verify, "postgres://u@localhost/d")
        );
    }
}
