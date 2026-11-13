//! `cfrg daily`: the one daily run of the bridge (operating model R49), as the CronJob in the `forge-bridge` namespace runs it.
//!
//! Four steps, in this order, each ALONE: a failing step is logged and the next one still runs; the command exits non-zero when any step
//! failed, so the Job fails. A step whose code or credential is not ready is a NO-OP that says so (and succeeds):
//!   renovate     runs in its own image (an initContainer of the Job: Renovate, cargo, git, nix are a large closure); it leaves its exit status in `--renovate-status`, and this step reports it (no file: a logged no-op)
//!   native       `cfrg native` drift reconcile (`--operation plan` when dry), when the credential file `native-env` (KEY=VALUE lines) is there
//!   settings     `cfrg native --operation settings --apply` (R42 copy settings, bounded by `--max-writes`), behind the GitHub gate's kill switch and pause folder
//!   wiki         `cfrg native --operation wiki --apply` (copy wikis, bounded pushes), same gate files and the wiki deploy key
//!   sweep        `cfrg relay sweep` (dry: only objects pending for ten years, a connection test of the database login)
//!   gitlab-key   the GitLab key renewal (`--gitlab-key-script`, B11), when the credentials gitlab_token and forgejo_push are there; dry: nothing is due (a connection test)
//!
//! The database connection string is built here from the files `host port dbname username password` of the credentials directory
//! (the Secret forge-bridge-db) and handed to the child as `CFRG_RELAY_DATABASE`; it never appears in an argument or in the output.
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
    /// The file the Renovate initContainer leaves its exit status in (a decimal number).
    #[arg(long, default_value = "/work/renovate.status")]
    renovate_status: PathBuf,
    /// The GitLab key renewal script (run with bash; workloads brh-fra-k8s/forge-bridge/gitlab-key-renew.sh).
    #[arg(long, default_value = "/scripts/gitlab-key-renew.sh")]
    gitlab_key_script: PathBuf,
    /// The GitHub write gate's directory (its volume): `<dir>/kill-switch` is the kill switch, `<dir>/paused` the pause folder. Without the directory the `settings` and `wiki` steps refuse.
    #[arg(long, default_value = "/gate")]
    gate_dir: PathBuf,
    /// Writes of one `settings` run and pushes of one `wiki` run (bounded per run).
    #[arg(long, default_value_t = 20)]
    max_writes: usize,
    /// Look only: every step plans or tests, nothing is written.
    #[arg(long, env = "DAILY_DRY_RUN")]
    dry: bool,
}

/// What one step does.
#[derive(Debug, Clone)]
pub enum Action {
    /// Succeeds, saying why nothing was done.
    Noop(String),
    /// A step that already ran elsewhere in the pod: succeeds when the file holds `0`.
    Status(PathBuf),
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
            Action::Status(path) => {
                let code = fs::read_to_string(path).unwrap_or_default();
                let ok = code.trim() == "0";
                println!(
                    "{}",
                    json!({"step": step.name, "outcome": if ok { "ok" } else { "failed" }, "exit": code.trim()})
                );
                ok
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
fn env_file(dir: &Path, name: &str) -> Option<BTreeMap<String, String>> {
    let text = credential(dir, name)?;
    Some(
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect(),
    )
}

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
    let renovate = if options.renovate_status.is_file() {
        Action::Status(options.renovate_status.clone())
    } else {
        Action::Noop(
            "renovate did not run in this pod (no status file from its initContainer)".into(),
        )
    };
    let native = match env_file(dir, "native-env") {
        None => Action::Noop("the provider credentials of cfrg native (native-env) are not in the credentials directory yet".into()),
        Some(mut env) => {
            common(&mut env);
            let mut args: Vec<String> = ["native", "--placement"].map(String::from).to_vec();
            args.push(options.placement.display().to_string());
            args.extend(["--state".into(), options.state.display().to_string()]);
            if dry {
                args.extend(["--operation".into(), "plan".into()]);
            } else {
                args.extend(["--operation".into(), "reconcile".into(), "--apply".into()]);
            }
            Action::Command { program: exe.to_owned(), args, env }
        }
    };
    // settings and wiki: the copies of the R42 settings and the copy wikis, only behind the gate's files (kill switch, pause folder) and, for the wiki, the deploy key.
    let gate_missing = if options.gate_dir.join("paused").is_dir() {
        None
    } else {
        Some(format!(
            "the GitHub gate's pause folder {} is missing",
            options.gate_dir.join("paused").display()
        ))
    };
    let gated = |operation: &str, extra: Vec<String>| -> Action {
        let Some(mut env) = env_file(dir, "native-env") else {
            return Action::Noop("the provider credentials of cfrg native (native-env) are not in the credentials directory yet".into());
        };
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
            options.gate_dir.join("kill-switch").display().to_string(),
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
    let settings = gated(
        "settings",
        vec!["--max-writes".into(), options.max_writes.to_string()],
    );
    let wiki = match (credential(dir, "wiki_deploy_key"), credential(dir, "wiki_known_hosts"), credential(dir, "wiki_key_id").and_then(|id| id.parse::<u64>().ok())) {
        (Some(_), Some(_), Some(id)) => gated(
            "wiki",
            vec![
                "--wiki-key".into(),
                dir.join("wiki_deploy_key").display().to_string(),
                "--wiki-known-hosts".into(),
                dir.join("wiki_known_hosts").display().to_string(),
                "--wiki-key-id".into(),
                id.to_string(),
                "--wiki-max-pushes".into(),
                options.max_writes.to_string(),
            ],
        ),
        _ => Action::Noop("refused: the wiki credentials (wiki_deploy_key, wiki_known_hosts, wiki_key_id) are not all in the credentials directory".into()),
    };
    let sweep = match database(dir, &options.ca) {
        None => Action::Noop("the database credentials (host port dbname username password) are not in the credentials directory".into()),
        Some(url) => {
            let mut env = BTreeMap::new();
            common(&mut env);
            env.insert("CFRG_RELAY_DATABASE".into(), url);
            let mut args = vec!["relay".to_owned(), "sweep".into()];
            if dry {
                args.extend(["--older-than".into(), "315360000".into()]);
            }
            Action::Command { program: exe.to_owned(), args, env }
        }
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
    vec![
        Step {
            name: "renovate",
            action: renovate,
        },
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
            name: "gitlab-key",
            action: gitlab_key,
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
            renovate_status: dir.join("renovate.status"),
            gitlab_key_script: "/scripts/gitlab-key-renew.sh".into(),
            gate_dir: "/nonexistent-gate".into(),
            max_writes: 20,
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
                "renovate",
                "native",
                "settings",
                "wiki",
                "sweep",
                "gitlab-key"
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
            ("renovate-forgejo", "tok"),
            ("native-env", "A=1\nB=2\n"),
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
        for step in [&steps[1], &steps[4]] {
            let Action::Command { args, .. } = &step.action else {
                panic!("{} must run", step.name)
            };
            assert!(args
                .iter()
                .all(|a| !a.contains("p'w") && !a.contains("tok")));
        }
        let Action::Command { args, env, .. } = &steps[4].action else {
            unreachable!()
        };
        assert_eq!(args, &["relay", "sweep", "--older-than", "315360000"]);
        assert!(env["CFRG_RELAY_DATABASE"].contains("password='p\\'w\\\\x'"));
        assert!(env["CFRG_RELAY_DATABASE"].contains("sslmode=verify-full"));
        let Action::Command { args, env, .. } = &steps[1].action else {
            unreachable!()
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "plan"])
                && !args.contains(&"--apply".to_string())
        );
        assert_eq!(env["A"], "1");
    }

    fn gate_world(dir: &Path, gate: &Path, wiki: bool) {
        for (k, v) in [("native-env", "A=1"), ("host", "db")] {
            fs::write(dir.join(k), v).unwrap();
        }
        if wiki {
            for (k, v) in [
                ("wiki_deploy_key", "k"),
                ("wiki_known_hosts", "h"),
                ("wiki_key_id", "77"),
            ] {
                fs::write(dir.join(k), v).unwrap();
            }
        }
        fs::create_dir_all(gate.join("paused")).unwrap();
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
                "renovate",
                "native",
                "settings",
                "wiki",
                "sweep",
                "gitlab-key"
            ]
        );
        let Action::Command { args, .. } = &steps[2].action else {
            panic!("settings must run")
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "settings"])
                && args.contains(&"--apply".to_string())
        );
        assert!(args.windows(2).any(|w| w == ["--max-writes", "20"]));
        assert!(
            args.contains(&"--github-kill-switch".to_string())
                && args.contains(&"--github-paused-dir".to_string())
        );
        let Action::Command { args, .. } = &steps[3].action else {
            panic!("wiki must run")
        };
        assert!(
            args.windows(2).any(|w| w == ["--operation", "wiki"])
                && args.contains(&"--apply".to_string())
        );
        assert!(
            args.windows(2).any(|w| w == ["--wiki-max-pushes", "20"])
                && args.windows(2).any(|w| w == ["--wiki-key-id", "77"])
        );
        // dry: neither writes
        o.dry = true;
        let steps = plan(&o, Path::new("/bin/cfrg"));
        for i in [2, 3] {
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
        assert!(matches!(steps[2].action, Action::Command { .. }));
        let Action::Noop(why) = &steps[3].action else {
            panic!("wiki without credentials must refuse")
        };
        assert!(why.starts_with("refused"));
        o.gate_dir = "/nonexistent-gate".into();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        for i in [2, 3] {
            let Action::Noop(why) = &steps[i].action else {
                panic!("no gate: refuse")
            };
            assert!(why.starts_with("refused"), "{why}");
        }
    }

    #[test]
    fn renovate_is_the_status_its_init_container_left() {
        let dir = tempfile::tempdir().unwrap();
        let o = options(dir.path(), false);
        let steps = plan(&o, Path::new("/bin/cfrg"));
        assert!(matches!(&steps[0].action, Action::Noop(why) if why.contains("did not run")));
        fs::write(dir.path().join("renovate.status"), "0\n").unwrap();
        let steps = plan(&o, Path::new("/bin/cfrg"));
        assert!(matches!(steps[0].action, Action::Status(_)));
        assert!(run_steps(&steps[..1]).is_empty());
        fs::write(dir.path().join("renovate.status"), "3\n").unwrap();
        assert_eq!(
            run_steps(&plan(&o, Path::new("/bin/cfrg"))[..1]),
            vec!["renovate"]
        );
    }

    #[test]
    fn a_real_run_reconciles_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("native-env"), "A=1").unwrap();
        let steps = plan(&options(dir.path(), false), Path::new("/bin/cfrg"));
        let Action::Command { args, .. } = &steps[1].action else {
            panic!()
        };
        assert!(args.contains(&"--apply".to_string()) && args.contains(&"reconcile".to_string()));
    }
}
