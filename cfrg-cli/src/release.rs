//! `cfrg release`: publish a build artifact for an exact commit on the primary
//! forge, and read back what is published. Consumers fetch the printed URL by
//! hash (Nix `fetchurl`), so no host needs a manual install.
use cfrg::{
    land::commit_id,
    model::Forge,
    native::http::{Http, Pacing},
    release::{self, Config},
    Result,
};
use clap::{Args, Subcommand};
use serde_json::json;
use std::path::PathBuf;

#[derive(Args)]
pub struct Options {
    /// Declared release configuration (JSON) of the repository.
    #[arg(long, env = "CFRG_RELEASE_CONFIG")]
    config: Option<PathBuf>,
    /// Request state; a fresh temporary directory when omitted (CI jobs).
    #[arg(long, env = "CFRG_RELEASE_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Publish a file under the exact commit it was built from.
    Publish {
        /// Publish this very executable, versioned by the commit it was built from.
        #[arg(long = "self", conflicts_with_all = ["file", "version"], required_unless_present = "file")]
        this_binary: bool,
        #[arg(long, requires = "version")]
        file: Option<PathBuf>,
        /// Full commit id the file was built from.
        #[arg(long = "commit")]
        version: Option<String>,
        /// Published file name; defaults to `<package>-<target>` or the file's name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Print URL, hash and size of a published file.
    Show {
        /// Full commit id the file was built from.
        #[arg(long = "commit")]
        version: String,
        #[arg(long)]
        name: String,
    },
}

pub fn run(options: Options) -> Result<()> {
    let config = Config::load(
        options
            .config
            .as_deref()
            .ok_or("Set --config or CFRG_RELEASE_CONFIG")?,
    )?;
    if config.forge != Forge::Forgejo {
        let capability = match config.forge {
            Forge::Gitlab => cglb::RELEASE,
            Forge::Bitbucket => cbkt::RELEASE,
            _ => cghb::RELEASE,
        };
        return Err(format!(
            "Release is {:?} for this forge: {}",
            capability.support, capability.note
        )
        .into());
    }
    let scratch;
    let state_dir = match options.state_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            dir
        }
        None => {
            scratch = tempfile::tempdir()?;
            scratch.path().to_path_buf()
        }
    };
    // Only reads go through the paced transport; the upload is one curl call.
    let mut http = Http::open(&state_dir.join("http.json"), false)?.paced(Pacing {
        read: 1,
        write: 3,
        creation: 3,
    });
    let record = match options.action {
        Action::Publish {
            this_binary,
            file,
            version,
            name,
        } => {
            let (path, version, default_name) = if this_binary {
                commit_id(cfrg::SOURCE_REVISION)
                    .map_err(|_| "This binary was built without a commit id; publish a CI build")?;
                (
                    std::env::current_exe()?,
                    cfrg::SOURCE_REVISION.to_owned(),
                    format!("{}-{}", config.package, cfrg::TARGET),
                )
            } else {
                let path = file.ok_or("Give --file or --self")?;
                let default = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("Unusable file name")?
                    .to_owned();
                (path, version.ok_or("Give --commit")?, default)
            };
            release::publish(
                &cfgj::land::PACKAGES,
                &mut http,
                &mut release::Curl,
                &config,
                &version,
                &name.unwrap_or(default_name),
                &path,
            )?
        }
        Action::Show { version, name } => {
            release::show(&cfgj::land::PACKAGES, &mut http, &config, &version, &name)?
        }
    };
    println!("{}", json!(record));
    Ok(())
}
