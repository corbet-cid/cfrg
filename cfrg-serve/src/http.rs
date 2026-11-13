//! The two outbound HTTP clients. Redirects are followed by hand (see `release::Api`), so both refuse to.
//!
//! * the providers (GitHub, GitLab) are reached with the platform's public roots;
//! * the in-cluster Forgejo is certified by the cluster service CA only (no `SSL_CERT_FILE` override for the
//!   providers: the first live run broke on exactly that).
use crate::config::Result;
use reqwest::{redirect::Policy, tls::Certificate, Client};
use std::{fs, path::Path, time::Duration};

fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(30))
        // The socket may be idle this long (a provider scanning an upload); a transfer of any size is allowed.
        .read_timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(2)
}

pub fn public_client() -> Result<Client> {
    Ok(builder().build()?)
}

pub fn forge_client(ca: Option<&Path>) -> Result<Client> {
    match ca {
        Some(path) => {
            let certificates = Certificate::from_pem_bundle(&fs::read(path)?)?;
            Ok(builder().tls_certs_only(certificates).build()?)
        }
        None => public_client(),
    }
}
