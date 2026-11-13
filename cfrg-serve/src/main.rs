//! `cfrg-serve serve` is the service; `cfrg-serve sync OWNER/REPO TAG [--plan]` is one release sync by hand (the
//! same code, `--plan` = read-only). Exit 0, 75 (incomplete, not a fault) or 1.
use cfrg_serve::{
    config::{Config, Credentials},
    release,
    server::{self, Bridge},
};
use std::{env, path::PathBuf, process::ExitCode};
use tokio::{net::TcpListener, sync::watch};

const USAGE: &str = "usage: cfrg-serve serve | cfrg-serve release-upload <github|gitlab|all> OWNER/REPO TAG [--plan] | cfrg-serve sync OWNER/REPO TAG [--plan]\n\
config from FORGE_BRIDGE_CONFIG (default /config/config.json), credentials from FORGE_BRIDGE_SECRETS (default /secrets)";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cfrg-serve: {error}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run(&args)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cfrg-serve: {error}");
            ExitCode::from(1)
        }
    }
}

async fn run(args: &[String]) -> cfrg_serve::config::Result<u8> {
    let config_path =
        env::var("FORGE_BRIDGE_CONFIG").unwrap_or_else(|_| "/config/config.json".into());
    let secrets = env::var("FORGE_BRIDGE_SECRETS").unwrap_or_else(|_| "/secrets".into());
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    match strs.as_slice() {
        ["serve"] => serve(&config_path, &secrets).await,
        ["release-upload", forge, repo, tag] | ["release-upload", forge, repo, tag, "--plan"]
            if ["github", "gitlab", "all"].contains(forge) =>
        {
            let dry = strs.len() == 5;
            let ctx = release::Ctx::new(
                Config::load(&PathBuf::from(&config_path))?,
                Credentials::load(&PathBuf::from(&secrets)),
            )?;
            let outcome =
                release::sync_for(&ctx, repo, tag, dry, (*forge != "all").then_some(*forge)).await;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(outcome.exit as u8)
        }
        ["sync", repo, tag] | ["sync", repo, tag, "--plan"] => {
            let dry = strs.len() == 4;
            let ctx = release::Ctx::new(
                Config::load(&PathBuf::from(&config_path))?,
                Credentials::load(&PathBuf::from(&secrets)),
            )?;
            let outcome = release::sync(&ctx, repo, tag, dry).await;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(outcome.exit as u8)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

async fn serve(config_path: &str, secrets: &str) -> cfrg_serve::config::Result<u8> {
    let config = Config::load(&PathBuf::from(config_path))?;
    let creds = Credentials::load(&PathBuf::from(secrets));
    let creds = match &config.relay {
        Some(r) => creds.merge(&PathBuf::from(&r.credentials_dir)),
        None => creds,
    };
    let listeners = config.listeners.clone();
    let keys: Vec<String> = creds
        .names()
        .iter()
        .filter(|k| k.starts_with("hmac_"))
        .map(|k| (*k).to_owned())
        .collect();
    let (github, gitlab) = (
        creds.get("github_token").is_some(),
        creds.get("gitlab_token").is_some(),
    );
    let relay: Option<std::sync::Arc<dyn cfrg_serve::relay::Relay>> = match &config.relay {
        Some(r) => {
            cfrg_serve::live::LiveRelay::from_config(r)?.map(|l| std::sync::Arc::new(l) as _)
        }
        None => None,
    };
    let relay_on = relay.is_some();
    let (bridge, workers) = Bridge::new(config, creds, relay)?;
    bridge.run_workers(workers);
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut ports = serde_json::Map::new();
    let mut tasks = Vec::new();
    for (name, listener) in listeners.iter().filter(|(_, l)| l.enabled) {
        let socket = TcpListener::bind((listener.listen.as_str(), listener.port)).await?;
        ports.insert(name.clone(), socket.local_addr()?.port().into());
        // In-cluster traffic is verified TLS 1.3 (R1): the certificate of the cluster service CA.
        let tls = match &listener.tls_dir {
            Some(dir) => Some(server::tls_acceptor(&PathBuf::from(dir))?),
            None => None,
        };
        tasks.push(tokio::spawn(server::serve(
            bridge.clone(),
            name.clone(),
            socket,
            tls,
            stop_rx.clone(),
        )));
    }
    server::log(
        "listening",
        serde_json::json!({"listeners": ports, "keys": keys, "github": github, "gitlab": gitlab, "relay": relay_on}),
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    let _ = stop_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    Ok(0)
}
