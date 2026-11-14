//! `cfrg daily`: the one daily run of the bridge as a scheduler runs it (a Kubernetes CronJob, a systemd timer, cron).
//!
//! Six steps, in this order, each ALONE: a failing step is logged and the next one still runs; the command exits non-zero when any step
//! failed, so the job fails. (Renovate is not one of them: run it as its own job.) A step whose code or credential is not ready is a NO-OP that says so (and succeeds):
//!   native       `cfrg native` drift reconcile (`--operation plan` when dry), when `forgejo-token` is there (it reads the per-credential files of the credentials directory like the relay)
//!   settings     `cfrg native --operation settings --apply` (copy settings, bounded by `--max-writes`), behind the GitHub gate's kill switch and pause folder
//!   wiki         `cfrg native --operation wiki --apply` (copy wikis, bounded pushes), same gate files and the wiki deploy key
//!   sweep        `cfrg relay sweep` (dry: only objects pending for ten years, a connection test of the database login)
//!   releases     `cfrg-serve release-resume`: finishes the release uploads a restart cut off (the journal `release_intents`), when the serve config is there
//!   releases     `cfrg-serve release-resume`: finishes the open release intents
//!   copy         the git copy of every repository (the script of `/hooks/copy` without a repository): the catch-up for a missed push or new-repository event
//!   gitlab-key   the GitLab key renewal (`--gitlab-key-script`), when the credentials gitlab_token and forgejo_push are there; dry: nothing is due (a connection test)
//!
//! The database connection string is built here from the files `host port dbname username password` of the credentials directory
//! (a mounted Secret or a systemd credential directory) and handed to the child as `CFRG_RELAY_DATABASE`; it never appears in an argument or in the output.
use cfrg::Result;
use clap::Args;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Args)]
pub struct Options {
    /// The directory of the credential files (default: `$CREDENTIALS_DIRECTORY`).
    #[arg(long, env = "CFRG_CREDENTIALS_DIRECTORY")]
    credentials_dir: PathBuf,
    #[arg(long, env = "CFRG_RELAY_CONFIG")]
    config: PathBuf,
    #[arg(long, env = "CFRG_PLACEMENT")]
    placement: PathBuf,
    /// Persistent state of `cfrg native`.
    #[arg(long, default_value = "/work/state")]
    state: PathBuf,
    /// CA file the database certificate is verified against (NOT `SSL_CERT_FILE`: that stays the public roots, which the providers and the key renewal need).
    #[arg(long, env = "CFRG_DAILY_CA", default_value = "/ca/ca.crt")]
    ca: PathBuf,
    /// The GitLab key renewal script: a script you provide (run with bash).
    #[arg(long, default_value = "/scripts/gitlab-key-renew.sh")]
    gitlab_key_script: PathBuf,
    /// The GitHub write gate's directory (its volume): `<dir>/kill` is the kill switch (the Frankfurt gate's `/state/kill`), `<dir>/paused` the pause folder. Without the directory the `settings` and `wiki` steps refuse.
    #[arg(long, default_value = "/gate")]
    gate_dir: PathBuf,
    /// The GitHub gate's catalogue (its `gate-config.json`, the ConfigMap `github-gate-files`): the `settings` step writes a GitHub copy
    /// only when the catalogue lists it (`github_copy_owned`). Without the file no GitHub copy is owned and none is written.
    #[arg(long, default_value = "/gate-files/gate-config.json")]
    gate_catalog: PathBuf,
    /// The serve configuration (for the `releases` step: forge addresses, organisations, the relay database login).
    #[arg(long, default_value = "/app/serve-config.json")]
    serve_config: PathBuf,
    /// The git copy's script, the very one `cfrg-serve`'s `/hooks/copy` runs per push (`hook_commands.copy`): the `copy` step runs it
    /// WITHOUT a repository, so it covers every repository the forge lists (the catch-up for a missed event).
    #[arg(long, default_value = "/hooks/copy.sh")]
    copy_script: PathBuf,
    /// Writes of one `settings` run and pushes of one `wiki` run (bounded per run).
    #[arg(long, default_value_t = 20)]
    max_writes: usize,
    /// The id of the wiki deploy key on GitLab (not a secret; optional).
    #[arg(long, env = "DAILY_WIKI_KEY_ID")]
    wiki_key_id: Option<u64>,
    /// Look only: every step plans or tests, nothing is written.
    #[arg(long, env = "DAILY_DRY_RUN")]
    dry: bool,
}

/// What one step does.
#[derive(Debug, Clone)]
pub enum Action {
    /// Succeeds, saying why nothing was done.
    Noop(String),
    Command {
        program: PathBuf,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: &'static str,
    pub action: Action,
}

/// Runs every step on its own. Returns the names of the steps that failed.
pub fn run_steps(steps: &[Step]) -> Vec<&'static str> {
    let mut failed = Vec::new();
    for step in steps {
        let outcome = match &step.action {
            Action::Noop(why) => {
                println!(
                    "{}",
                    json!({"step": step.name, "outcome": "noop", "why": why})
                );
                true
            }
            Action::Command { program, args, env } => {
                println!("{}", json!({"step": step.name, "outcome": "start"}));
                match Command::new(program).args(args).envs(env).status() {
                    Ok(status) if status.success() => true,
                    Ok(status) => {
                        eprintln!("daily: step {} exited with {status}", step.name);
                        false
                    }
                    Err(error) => {
                        eprintln!("daily: step {} could not start: {error}", step.name);
                        false
                    }
                }
            }
        };
        if matches!(step.action, Action::Command { .. }) {
            println!(
                "{}",
                json!({"step": step.name, "outcome": if outcome { "ok" } else { "failed" }})
            );
        }
        if !outcome {
            failed.push(step.name);
        }
    }
    failed
}

fn credential(dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim_end_matches('\n').to_owned();
    (!text.is_empty()).then_some(text)
}

/// libpq connection string from the Secret's files; values quoted (`\` and `'` escaped).
fn database(dir: &Path, ca: &Path) -> Option<String> {
    let quote = |v: String| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"));
    let mut parts = Vec::new();
    for (key, file) in [
        ("host", "host"),
        ("port", "port"),
        ("dbname", "dbname"),
        ("user", "username"),
        ("password", "password"),
    ] {
        parts.push(format!("{key}={}", quote(credential(dir, file)?)));
    }
    parts.push("sslmode=verify-full".into());
    parts.push(format!("sslrootcert={}", quote(ca.display().to_string())));
    Some(parts.join(" "))
}

/// `KEY=VALUE` lines of a credential file, for the child's environment only.
pub fn plan(options: &Options, exe: &Path) -> Vec<Step> {
    let dir = &options.credentials_dir;
    let dry = options.dry;
    let common = |env: &mut BTreeMap<String, String>| {
        env.insert(
            "CFRG_RELAY_CONFIG".into(),
            options.config.display().to_string(),
        );
        env.insert(
            "CFRG_PLACEMENT".into(),
            options.placement.display().to_string(),
        );
        env.insert(
            "CFRG_CREDENTIALS_DIRECTORY".into(),
            dir.display().to_string(),
        );
    };
    // `cfrg native` reads the same per-credential files as the relay and serve (forgejo-token, gitlab-token, the GitHub App's github-app-id and
    // github-app-key; a Bitbucket credential is skipped by native itself with one log line); the step needs the Forgejo token at least.
    let missing_native = credential(dir, "forgejo-token").is_none();
    let native = if missing_native {
        Action::Noop("refused: forgejo-token is not in the credentials directory".into())
    } else {
        let mut env = BTreeMap::new();
        common(&mut env);
        let mut args: Vec<String> = ["native", "--placement"].map(String::from).to_vec();
        args.push(options.placement.display().to_string());
        args.extend(["--state".into(), options.state.display().to_string()]);
        if dry {
            args.extend(["--operation".into(), "plan".into()]);
        } else {
            args.extend(["--operation".into(), "reconcile".into(), "--apply".into()]);
        }
        Action::Command {
            program: exe.to_owned(),
            args,
            env,
        }
    };
    // settings and wiki: the copy settings and the copy wikis, only behind the gate's files (kill switch, pause folder) and, for the wiki, the deploy key.
    let gate_missing = if options.gate_dir.join("paused").is_dir() {
        None
    } else {
        Some(format!(
            "the GitHub gate's pause folder {} is missing",
            options.gate_dir.join("paused").display()
        ))
    };
    let gated = |operation: &str, extra: Vec<String>| -> Action {
        if missing_native {
            return Action::Noop(
                "refused: forgejo-token is not in the credentials directory".into(),
            );
        }
        let mut env = BTreeMap::new();
        if let Some(why) = &gate_missing {
            return Action::Noop(format!("refused: {why}"));
        }
        common(&mut env);
        let mut args: Vec<String> = ["native", "--placement"].map(String::from).to_vec();
        args.push(options.placement.display().to_string());
        args.extend([
            "--state".into(),
            options.state.display().to_string(),
            "--operation".into(),
            operation.into(),
        ]);
        args.extend([
            "--github-kill-switch".into(),
            options.gate_dir.join("kill").display().to_string(),
        ]);
        args.extend([
            "--github-paused-dir".into(),
            options.gate_dir.join("paused").display().to_string(),
        ]);
        args.extend(extra);
        if !dry {
            args.push("--apply".into());
        }
        Action::Command {
            program: exe.to_owned(),
            args,
            env,
        }
    };
    let mut settings_args = vec!["--max-writes".into(), options.max_writes.to_string()];
    if options.gate_catalog.is_file() {
        settings_args.extend([
            "--github-gate-catalog".into(),
            options.gate_catalog.display().to_string(),
        ]);
    }
    let settings = gated("settings", settings_args);
    // The id of the deploy key on GitLab is no secret: an optional setting of the job (DAILY_WIKI_KEY_ID); without it a copy that needs the
    // key enabled by id reports that row, every other copy works (the key is found or created by its own fingerprint).
    let wiki = match (credential(dir, "wiki_deploy_key"), credential(dir, "wiki_known_hosts")) {
        (Some(_), Some(_)) => {
            let mut extra = vec![
                "--wiki-key".into(),
                dir.join("wiki_deploy_key").display().to_string(),
                "--wiki-known-hosts".into(),
                dir.join("wiki_known_hosts").display().to_string(),
                "--wiki-max-pushes".into(),
                options.max_writes.to_string(),
            ];
            if let Some(id) = options.wiki_key_id {
                extra.extend(["--wiki-key-id".into(), id.to_string()]);
            }
            gated("wiki", extra)
        }
        _ => Action::Noop("refused: the wiki credentials (wiki_deploy_key, wiki_known_hosts) are not both in the credentials directory".into()),
    };
    let sweep = match database(dir, &options.ca) {
        None => Action::Noop("the database credentials (host port dbname username password) are not in the credentials directory".into()),
        Some(url) => {
            let mut env = BTreeMap::new();
            common(&mut env);
            env.insert("CFRG_RELAY_DATABASE".into(), url);
            // the relay reads its provider credentials from $CREDENTIALS_DIRECTORY (docs/relay.md); without it every forge was refused
            env.insert("CREDENTIALS_DIRECTORY".into(), dir.display().to_string());
            let mut args = vec!["relay".to_owned(), "sweep".into()];
            if dry {
                args.extend(["--older-than".into(), "315360000".into()]);
            }
            Action::Command { program: exe.to_owned(), args, env }
        }
    };
    let releases = if options.serve_config.is_file() {
        let mut env = BTreeMap::new();
        env.insert(
            "FORGE_BRIDGE_CONFIG".into(),
            options.serve_config.display().to_string(),
        );
        env.insert("FORGE_BRIDGE_SECRETS".into(), dir.display().to_string());
        Action::Command {
            // next to cfrg when it is there; else by name on PATH (the image links cfrg from its own store path, where cfrg-serve is not a sibling)
            program: {
                let sibling = exe.with_file_name("cfrg-serve");
                if sibling.is_file() {
                    sibling
                } else {
                    PathBuf::from("cfrg-serve")
                }
            },
            args: vec!["release-resume".into()],
            env,
        }
    } else {
        Action::Noop(format!(
            "the serve configuration {} is not mounted",
            options.serve_config.display()
        ))
    };
    let gitlab_key =
        if credential(dir, "gitlab_token").is_none() || credential(dir, "forgejo_push").is_none() {
            Action::Noop(
            "the credentials gitlab_token and forgejo_push are not in the credentials directory"
                .into(),
        )
        } else {
            let mut env = BTreeMap::new();
            for (var, file) in [
                ("PGHOST", "host"),
                ("PGPORT", "port"),
                ("PGDATABASE", "dbname"),
                ("PGUSER", "username"),
            ] {
                if let Some(value) = credential(dir, file) {
                    env.insert(var.to_owned(), value);
                }
            }
            env.insert(
                "PGPASSWORD_FILE".into(),
                dir.join("password").display().to_string(),
            );
            env.insert("PGSSLMODE".into(), "verify-full".into());
            env.insert("PGSSLROOTCERT".into(), options.ca.display().to_string());
            env.insert("CREDENTIALS_DIRECTORY".into(), dir.display().to_string());
            env.insert("WORK".into(), "/work/gitlab-key".into());
            env.insert("HOME".into(), "/work".into());
            if dry {
                // Nothing is due, so nothing is rotated: the run reads both keys and the lease only.
                env.insert("THRESHOLD_DAYS".into(), "0".into());
            }
            Action::Command {
                program: "bash".into(),
                args: vec![options.gitlab_key_script.display().to_string()],
                env,
            }
        };
    // copy: the hook's own script, no repository argument = all of them. Dry: the script is told to plan only (`COPY_DRY=1`).
    let copy = if options.copy_script.is_file() {
        let mut env = BTreeMap::new();
        if dry {
            env.insert("COPY_DRY".into(), "1".into());
        }
        Action::Command {
            program: "/bin/sh".into(),
            args: vec![options.copy_script.display().to_string()],
            env,
        }
    } else {
        Action::Noop(format!(
            "the git copy script {} is not mounted",
            options.copy_script.display()
        ))
    };
    vec![
        Step {
            name: "native",
            action: native,
        },
        Step {
            name: "settings",
            action: settings,
        },
        Step {
            name: "wiki",
            action: wiki,
        },
        Step {
            name: "sweep",
            action: sweep,
        },
        Step {
            name: "releases",
            action: releases,
        },
        Step {
            name: "gitlab-key",
            action: gitlab_key,
        },
        Step {
            name: "copy",
            action: copy,
        },
    ]
}

pub fn run(options: Options) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(&options.state).ok();
    fs::create_dir_all("/work/gitlab-key").ok();
    let steps = plan(&options, &exe);
    let failed = run_steps(&steps);
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("daily: failed steps: {}", failed.join(", ")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(program: &str, args: &[&str]) -> Action {
        Action::Command {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn a_failing_step_does_not_stop_the_next_and_fails_the_run() {
        let steps = [
            Step {
                name: "a",
                action: cmd("false", &[]),
            },
            Step {
                name: "b",
                action: Action::Noop("later".into()),
            },
            Step {
                name: "c",
                action: cmd("true", &[]),
            },
            Step {
                name: "d",
                action: cmd("/nonexistent/program", &[]),
            },
        ];
        assert_eq!(run_steps(&steps), vec!["a", "d"]);
    }

    #[test]
    fn every_step_ok_means_no_failures() {
        let steps = [
            Step {
                name: "a",
                action: cmd("true", &[]),
            },
            Step {
                name: "b",
                action: Action::Noop("x".into()),
            },
        ];
        assert!(run_steps(&steps).is_empty());
    }

    fn options(dir: &Path, dry: bool) -> Options {
        Options {
            credentials_dir: dir.to_owned(),
            config: "/app/config.json".into(),
            placement: "/app/placement.json".into(),
            state: "/work/state".into(),
            ca: "/ca/ca.crt".into(),
            gitlab_key_script: "/scripts/gitlab-key-renew.sh".into(),
            gate_dir: "/nonexistent-gate".into(),
            gate_catalog: "/nonexistent-gate-config.json".into(),
            serve_config: "/nonexistent-serve-config.json".into(),
            copy_script: "/nonexistent-copy.sh".into(),
            max_writes: 20,
            wiki_key_id: None,
            dry,
        }
    }

    #[test]
    fn missing_credentials_make_every_step_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let steps = plan(&options(dir.path(), false), Path::new("/bin/cfrg"));
        assert_eq!(
            steps.iter().map(|s| s.name).collect::<Vec<_>>(),
            [
                "native",
                "settings",
                "wiki",
                "sweep",
                "releases",
                "gitlab-key",
                "copy"
            ]
        );
        assert!(steps.iter().all(|s| matches!(s.action, Action::Noop(_))));
    }

    #[test]
    fn credentials_make_commands_and_the_password_stays_out_of_arguments() {
        let dir = tempfile::tempdir().unwrap();
        for (k, v) in [
            ("host", "db"),
            ("port", "5432"),
            ("dbname", "forge_bridge"),
            ("username", "u"),
            ("password", "p'w\\x"),
            ("forgejo-token", "t"),
            ("gitlab_token", "g"),
            ("forgejo_push", "f"),
            ("gitlab_token", "g"),
            ("forgejo_push", "f"),
        ] {
            fs::write(dir.path().join(k), v).unwrap();
        }
        let steps = plan(&options(dir.path(), true), Path::new("/bin/cfrg"));
        let Action::Command { args, env, .. } = &steps[5].action else {
            panic!("gitlab-key must run")
        };
        assert_eq!(args, &["/scripts/gitlab-key-renew.sh"]);
        assert_eq!(env["THRESHOLD_DAYS"], "0");
        assert!(!env.values().any(|v| v.contains("p'w")));
        for step in [&steps[0], &steps[3]] {
            let Action::Command { args, .. } = &step.action else {
                panic!("{} must run", step.name)
            };
            assert!(args
                .iter()
                .all(|a| !a.contains("p'w") && !a.contains("tok")));
        }
        let Action::Command { args, env, .. } = &steps[3].action else {
            unreachable!()
        };
        assert_eq!(args, &["relay", "sweep", "--older-than", "315360000"]);
        assert!(env["CFRG_RELAY_DATABASE"].contains("password='p\\'w\\\\x'"));
        assert!(env["CFRG_RELAY_DATABASE"].contains("sslmode=verify-full"));
        assert_eq!(
            env["CREDENTIALS_DIRECTORY"],
            dir.path().display().to_string(),
            "the relay finds its provider credentials"
        );
        let Action::Command { args, env, .. } = &steps[0].action else {
            unreachable!()
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "plan"])
                && !args.contains(&"--apply".to_string())
        );
        assert_eq!(
            env["CFRG_CREDENTIALS_DIRECTORY"],
            dir.path().display().to_string()
        );
        assert!(
            !env.keys().any(|k| k.ends_with("TOKEN")),
            "no token travels in the environment"
        );
    }

    fn gate_world(dir: &Path, gate: &Path, wiki: bool) {
        for (k, v) in [("forgejo-token", "t"), ("host", "db")] {
            fs::write(dir.join(k), v).unwrap();
        }
        if wiki {
            for (k, v) in [("wiki_deploy_key", "k"), ("wiki_known_hosts", "h")] {
                fs::write(dir.join(k), v).unwrap();
            }
        }
        fs::create_dir_all(gate.join("paused")).unwrap();
    }

    #[test]
    fn the_settings_step_passes_the_gate_catalogue_when_it_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let gate = tempfile::tempdir().unwrap();
        gate_world(dir.path(), gate.path(), false);
        let catalog = gate.path().join("gate-config.json");
        fs::write(&catalog, "{}").unwrap();
        let mut o = options(dir.path(), false);
        o.gate_dir = gate.path().to_owned();
        o.gate_catalog = catalog.clone();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        let Action::Command { args, .. } = &steps[1].action else {
            panic!("settings must run")
        };
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--github-gate-catalog" && w[1] == catalog.display().to_string()));
    }

    #[test]
    fn settings_and_wiki_run_behind_the_gate_files_with_bounded_limits() {
        let dir = tempfile::tempdir().unwrap();
        let gate = tempfile::tempdir().unwrap();
        gate_world(dir.path(), gate.path(), true);
        let mut o = options(dir.path(), false);
        o.gate_dir = gate.path().to_owned();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        assert_eq!(
            steps.iter().map(|s| s.name).collect::<Vec<_>>(),
            [
                "native",
                "settings",
                "wiki",
                "sweep",
                "releases",
                "gitlab-key",
                "copy"
            ]
        );
        let Action::Command { args, .. } = &steps[1].action else {
            panic!("settings must run")
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "settings"])
                && args.contains(&"--apply".to_string())
        );
        assert!(args.windows(2).any(|w| w == ["--max-writes", "20"]));
        // The catalogue is passed only when the file is there; the test world has none.
        assert!(!args.contains(&"--github-gate-catalog".to_string()));
        assert!(
            args.contains(&"--github-kill-switch".to_string())
                && args.contains(&"--github-paused-dir".to_string())
        );
        let Action::Command { args, .. } = &steps[2].action else {
            panic!("wiki must run")
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "wiki"])
                && args.contains(&"--apply".to_string())
        );
        assert!(
            args.windows(2).any(|w| w == ["--wiki-max-pushes", "20"])
                && !args.contains(&"--wiki-key-id".to_string())
        );
        // dry: neither writes
        o.dry = true;
        let steps = plan(&o, Path::new("/bin/cfrg"));
        for i in [1, 2] {
            let Action::Command { args, .. } = &steps[i].action else {
                panic!()
            };
            assert!(!args.contains(&"--apply".to_string()));
        }
    }

    #[test]
    fn without_the_gate_or_the_wiki_credentials_these_steps_refuse_with_a_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let gate = tempfile::tempdir().unwrap();
        gate_world(dir.path(), gate.path(), false);
        let mut o = options(dir.path(), false);
        o.gate_dir = gate.path().to_owned();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        assert!(matches!(steps[1].action, Action::Command { .. }));
        let Action::Noop(why) = &steps[2].action else {
            panic!("wiki without credentials must refuse")
        };
        assert!(why.starts_with("refused"));
        o.gate_dir = "/nonexistent-gate".into();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        for i in [1, 2] {
            let Action::Noop(why) = &steps[i].action else {
                panic!("no gate: refuse")
            };
            assert!(why.starts_with("refused"), "{why}");
        }
    }

    #[test]
    fn a_real_run_reconciles_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("forgejo-token"), "t").unwrap();
        let steps = plan(&options(dir.path(), false), Path::new("/bin/cfrg"));
        let Action::Command { args, .. } = &steps[0].action else {
            panic!()
        };
        assert!(args.contains(&"--apply".to_string()) && args.contains(&"reconcile".to_string()));
    }

    #[test]
    fn the_releases_step_finishes_open_release_intents_with_the_serve_binary_next_to_cfrg() {
        let dir = tempfile::tempdir().unwrap();
        // no serve configuration mounted: a no-op that says so
        let steps = plan(&options(dir.path(), false), Path::new("/bin/cfrg"));
        let Action::Noop(why) = &steps[4].action else {
            panic!("not a no-op");
        };
        assert!(why.contains("serve configuration"));
        // mounted: `cfrg-serve release-resume` with the config and the credentials directory in the environment
        let serve = dir.path().join("serve-config.json");
        fs::write(&serve, "{}").unwrap();
        let mut o = options(dir.path(), false);
        o.serve_config = serve.clone();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        assert_eq!(steps[4].name, "releases");
        let Action::Command { program, args, env } = &steps[4].action else {
            panic!("not a command");
        };
        assert_eq!(
            program,
            Path::new("cfrg-serve"),
            "no sibling in /bin here: found by name on PATH"
        );
        assert_eq!(args, &["release-resume"]);
        assert_eq!(env["FORGE_BRIDGE_CONFIG"], serve.display().to_string());
        assert_eq!(
            env["FORGE_BRIDGE_SECRETS"],
            dir.path().display().to_string()
        );
        assert!(env.values().all(|v| !v.contains("password")));
    }

    #[test]
    fn the_copy_step_runs_the_hook_script_without_a_repository_and_plans_only_when_dry() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("copy.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        for dry in [false, true] {
            let mut o = options(dir.path(), dry);
            o.copy_script = script.clone();
            let steps = plan(&o, Path::new("/bin/cfrg"));
            let copy = steps.last().unwrap();
            assert_eq!(copy.name, "copy");
            let Action::Command { program, args, env } = &copy.action else {
                panic!("copy must run")
            };
            assert_eq!(program, Path::new("/bin/sh"));
            assert_eq!(
                args,
                &[script.display().to_string()],
                "no repository argument: all of them"
            );
            assert_eq!(env.get("COPY_DRY").map(String::as_str), dry.then_some("1"));
        }
    }
}
