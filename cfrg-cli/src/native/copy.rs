//! The copy: every repository the source lists, and its wiki, on one plain git host.
//!
//! Forgejo's own SSH push mirrors do the transfer (sync on commit, plus the
//! interval as a safety net). cfrg only provisions: before a mirror exists it
//! makes the bare repository (and the wiki twin the mirror's wiki probe must
//! find), after Forgejo created the mirror it admits the public key Forgejo
//! generated. Both go through the receiver's admin channel, one command per
//! line (`ensure`, `admit`), so cfrg never needs a shell, a file system or a
//! provider API on the receiving side. Every step is idempotent, so a pass that
//! stopped halfway is finished by the next one.
use cfgj::native::{duration_seconds, list_repositories, status, wiki_exists, Mirrors};
use cfrg::{
    native::{http::Transport, Content, Copy, Endpoint, Repository},
    process::Runner,
    Result,
};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf, time::Duration};

/// The receiver's admin channel: command lines in, one answer line per command out.
pub trait Receiver {
    fn send(&mut self, lines: &[String]) -> Result<Vec<String>>;
}

/// For read-only passes: nothing is ever sent.
pub struct NoReceiver;

impl Receiver for NoReceiver {
    fn send(&mut self, lines: &[String]) -> Result<Vec<String>> {
        if lines.is_empty() {
            Ok(Vec::new())
        } else {
            Err("The receiver is not reached by a read-only pass".into())
        }
    }
}

/// The admin channel over SSH: the receiver's forced command reads stdin.
pub struct Ssh<'a> {
    pub copy: &'a Copy,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
}

impl Ssh<'_> {
    /// Both references are environment variables naming files.
    pub fn open(copy: &Copy) -> Result<Ssh<'_>> {
        let file = |name: &str| -> Result<PathBuf> {
            let path = PathBuf::from(std::env::var(name).map_err(|_| format!("Set {name}"))?);
            if !path.is_absolute() || !path.is_file() {
                return Err(format!("{name} must name an existing file by absolute path").into());
            }
            Ok(path)
        };
        Ok(Ssh {
            key: file(&copy.admin_key_env)?,
            known_hosts: file(&copy.known_hosts_env)?,
            copy,
        })
    }
}

impl Receiver for Ssh<'_> {
    fn send(&mut self, lines: &[String]) -> Result<Vec<String>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let (host, port) = self.copy.host_and_port();
        let mut argv: Vec<String> = [
            "ssh",
            "-i",
            &self.key.to_string_lossy(),
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            &format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=20",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=4",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if let Some(port) = port {
            argv.extend(["-p".into(), port.into()]);
        }
        argv.push(format!("{}@{host}", self.copy.user));
        let mut input = tempfile::NamedTempFile::new()?;
        for line in lines {
            writeln!(input, "{line}")?;
        }
        input.flush()?;
        // The child sees PATH and nothing else: no agent, no ambient ssh configuration.
        let environment: cfrg::Environment = ["PATH", "TMPDIR"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value)))
            .collect();
        let runner = Runner::new(
            std::env::current_dir()?,
            environment,
            Duration::from_secs(600),
        )?
        .with_stderr_events();
        let output = runner.run_bytes_with_input_file(&argv, input.path())?;
        let answers: Vec<String> = String::from_utf8(output)?
            .lines()
            .map(String::from)
            .collect();
        if answers.len() != lines.len() || answers.iter().any(|a| !a.starts_with("ok")) {
            // The first refusal says why; command lines carry no secret (a public key at most).
            let refusal = answers
                .iter()
                .find(|a| !a.starts_with("ok"))
                .map_or("no answer", String::as_str);
            return Err(format!("Receiver refused the admin commands: {refusal}").into());
        }
        Ok(answers)
    }
}

/// Whether a repository name is something the receiver accepts as a branch name.
fn plain_branch(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
}

/// Whether a Forgejo mirror is one of the copy (its address is on the copy host).
pub fn is_copy_mirror(copy: Option<&Copy>, mirror: &Value) -> bool {
    copy.is_some_and(|c| {
        mirror["remote_address"]
            .as_str()
            .is_some_and(|a| a.starts_with(&format!("ssh://{}/", c.host)))
    })
}

/// The mirror a repository already has on the receiver (any path on the host
/// counts as ours: a repository that was renamed still points at its old path).
fn ours<'a>(copy: &Copy, mirrors: &'a [Value]) -> Result<Option<&'a Value>> {
    let prefix = format!("ssh://{}/", copy.host);
    let mut found = mirrors.iter().filter(|m| {
        m["remote_address"]
            .as_str()
            .is_some_and(|a| a.starts_with(&prefix))
    });
    let first = found.next();
    if found.next().is_some() {
        return Err(
            "Several mirrors to the copy host on one repository; explicit repair required".into(),
        );
    }
    Ok(first)
}

/// The mirror is the one the copy wants: right address, SSH key, sync on commit.
fn current(copy: &Copy, path: &str, mirror: &Value) -> bool {
    mirror["remote_address"] == copy.address(path).as_str()
        && mirror["public_key"].as_str().is_some_and(|k| !k.is_empty())
        && mirror["sync_on_commit"] == true
        && mirror["branch_filter"].as_str().unwrap_or("").is_empty()
        && mirror["interval"].as_str().and_then(duration_seconds) == Some(copy.interval_seconds)
}

/// One pass over every repository of the source. Returns the report rows and
/// whether anything is still missing.
pub fn reconcile(
    copy: &Copy,
    source: &Endpoint,
    io: &mut dyn Transport,
    receiver: &mut dyn Receiver,
    write: bool,
    only: Option<&str>,
) -> Result<(Vec<Value>, bool)> {
    let mut report = Vec::new();
    let mut incomplete = false;
    let mut repositories = list_repositories(io, source)?;
    // `only`: the one repository of an event (cfrg serve's /hooks/copy). Every step below is per repository (an admission
    // replaces only the key line tagged for its path), so a pass over one repository leaves the others as they are.
    if let Some(only) = only {
        repositories.retain(|r| r.path.eq_ignore_ascii_case(only));
        if repositories.is_empty() {
            report.push(json!({"source": only, "state": "not-listed"}));
            return Ok((report, true));
        }
    }
    for repo in &mut repositories {
        repo.has_wiki = repo.has_wiki && wiki_exists(io, source, &repo.path)?;
    }
    // Provision the bare repositories first: the wiki probe of a new mirror needs its twin.
    let mut fresh_twin = vec![false; repositories.len()];
    if write {
        let mut lines = Vec::new();
        for repo in &repositories {
            if !plain_branch(&repo.default_branch) {
                return Err("Default branch the receiver cannot take".into());
            }
            lines.push(format!(
                "ensure {} {} {}",
                repo.path,
                repo.default_branch,
                u8::from(repo.has_wiki)
            ));
        }
        for (i, answer) in receiver.send(&lines)?.iter().enumerate() {
            fresh_twin[i] = answer == "ok wiki-created";
        }
    }
    let mut admissions = Vec::new();
    for (repo, fresh_twin) in repositories.iter().zip(fresh_twin) {
        let row = |state: &str, mirror: Option<&Value>| {
            let mut row = json!({"source":repo.path, "destination":copy.address(&repo.path), "state":state, "wiki":repo.has_wiki});
            if let Some(m) = mirror {
                row["mirror"] = status(m);
            }
            row
        };
        let declared = Repository {
            path: repo.path.clone(),
            source_id: repo.id,
            private: false,
            default_branch: repo.default_branch.clone(),
            content: Content::NativeGit,
            hold: None,
            destinations: Vec::new(),
        };
        let api = Mirrors {
            endpoint: source,
            repository: &declared,
        };
        let mirrors = api.list(io)?;
        let mut mirror = ours(copy, &mirrors)?.cloned();
        // An archived repository refuses a new push mirror. It cannot change, so
        // its copy is seeded once and verified; an existing mirror keeps working.
        if repo.archived && mirror.is_none() {
            report.push(row("archived", None));
            continue;
        }
        // A wiki twin made just now cannot have been probed by a mirror that
        // already exists: Forgejo configures the wiki remote only at creation.
        let replace = mirror
            .as_ref()
            .is_some_and(|m| !current(copy, &repo.path, m) || (fresh_twin && repo.has_wiki));
        let mut state = "current";
        if replace {
            if !write || repo.archived {
                report.push(row("replace-planned", mirror.as_ref()));
                incomplete = true;
                continue;
            }
            let old = mirror.take().ok_or("Missing mirror")?;
            api.delete(
                io,
                old["remote_name"].as_str().ok_or("Missing remote name")?,
            )?;
            state = "replaced";
        }
        if mirror.is_none() {
            if !write {
                report.push(row("create-planned", None));
                incomplete = true;
                continue;
            }
            mirror =
                Some(api.create_ssh(io, &copy.push_address(&repo.path), copy.interval_seconds)?);
            if state == "current" {
                state = "created";
            }
        }
        let mirror = mirror.ok_or("Missing mirror")?;
        if write {
            // The key type and the key, without any comment.
            let key = mirror["public_key"]
                .as_str()
                .map(|k| k.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
                .filter(|k| !k.is_empty())
                .ok_or("Mirror without a public key")?;
            admissions.push(format!("admit {} {key}", repo.path));
        }
        report.push(row(state, Some(&mirror)));
    }
    // Admit every key on every pass: it is idempotent and heals a keys file that was lost.
    receiver.send(&admissions)?;
    Ok((report, incomplete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::native::http::{Request, Response};
    use std::collections::{BTreeMap, BTreeSet};

    fn copy() -> Copy {
        serde_json::from_value(json!({"host":"192.0.2.6","user":"mirror","admin_key_env":"K","known_hosts_env":"H","interval_seconds":600})).unwrap()
    }
    fn source() -> Endpoint {
        Endpoint {
            origin: "https://forge.example".into(),
            token_env: "T".into(),
        }
    }

    /// A forge that answers the calls of the copy pass and remembers the writes.
    #[derive(Default)]
    struct Forge {
        repos: Vec<Value>,
        wikis: BTreeSet<String>,
        mirrors: BTreeMap<String, Vec<Value>>,
        writes: Vec<String>,
        serial: u32,
    }
    impl Forge {
        fn repo(mut self, id: u64, name: &str, wiki: bool, archived: bool) -> Self {
            self.repos.push(json!({"id":id,"full_name":name,"default_branch":"main","archived":archived,"has_wiki":true}));
            if wiki {
                self.wikis.insert(name.to_ascii_lowercase());
            }
            self
        }
        fn mirror(&mut self, path: &str, address: &str, interval: &str) {
            self.serial += 1;
            self.mirrors.entry(path.into()).or_default().push(json!({
                "remote_name": format!("remote_mirror_{}", self.serial), "remote_address": address,
                "interval": interval, "sync_on_commit": true, "branch_filter": "",
                "public_key": format!("ssh-ed25519 KEY{}", self.serial), "last_error": "", "last_update": "x",
            }));
        }
    }
    impl Transport for Forge {
        fn send(&mut self, request: Request) -> Result<Response> {
            let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
            let page1 = query.contains("page=1");
            let ok = |body: Value| Ok(Response { status: 200, body });
            if path == "/api/v1/repos/search" {
                return ok(
                    json!({"ok":true,"data": if page1 { self.repos.clone() } else { Vec::new() }}),
                );
            }
            let rest = path.strip_prefix("/api/v1/repos/").expect("repos route");
            if let Some(name) = rest.strip_suffix("/wiki/pages") {
                return if self.wikis.contains(name) {
                    ok(json!([{"title":"Home"}]))
                } else {
                    Ok(Response {
                        status: 404,
                        body: Value::Null,
                    })
                };
            }
            if let Some(name) = rest.strip_suffix("/push_mirrors") {
                return match request.method {
                    "GET" => ok(json!(if page1 {
                        self.mirrors.get(name).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    })),
                    "POST" => {
                        let body = request.body.expect("body");
                        assert_eq!(body["use_ssh"], true);
                        assert_eq!(body["sync_on_commit"], true);
                        assert!(body.get("remote_password").is_none());
                        let address = body["remote_address"]
                            .as_str()
                            .unwrap()
                            .replacen("mirror@", "", 1);
                        self.writes.push(format!("create {name}"));
                        self.mirror(name, &address, "10m0s");
                        ok(self.mirrors[name].last().unwrap().clone())
                    }
                    other => panic!("unexpected {other} {path}"),
                };
            }
            let (name, remote) = rest.split_once("/push_mirrors/").expect("mirror route");
            assert_eq!(request.method, "DELETE");
            self.writes.push(format!("delete {name}"));
            self.mirrors
                .get_mut(name)
                .unwrap()
                .retain(|m| m["remote_name"] != remote);
            Ok(Response {
                status: 204,
                body: Value::Null,
            })
        }
    }

    /// The receiver keeps the set of wiki twins it made, as the real one keeps directories.
    #[derive(Default)]
    struct Host {
        twins: BTreeSet<String>,
        lines: Vec<String>,
    }
    impl Receiver for Host {
        fn send(&mut self, lines: &[String]) -> Result<Vec<String>> {
            let mut answers = Vec::new();
            for line in lines {
                self.lines.push(line.clone());
                let words: Vec<&str> = line.split(' ').collect();
                answers.push(match words[0] {
                    "ensure" if words[3] == "1" && self.twins.insert(words[1].to_string()) => {
                        "ok wiki-created".to_string()
                    }
                    "ensure" | "admit" => "ok".to_string(),
                    other => panic!("unexpected {other}"),
                });
            }
            Ok(answers)
        }
    }

    #[test]
    fn every_listed_repository_gets_a_mirror_its_key_and_its_wiki_twin_before_it() {
        let mut forge = Forge::default()
            .repo(1, "Org/Wiki", true, false)
            .repo(2, "org/plain", false, false)
            .repo(3, "org/old", false, true);
        let mut host = Host::default();
        let (rows, incomplete) =
            reconcile(&copy(), &source(), &mut forge, &mut host, true, None).unwrap();
        assert!(!incomplete);
        let states: Vec<_> = rows
            .iter()
            .map(|r| (r["source"].as_str().unwrap(), r["state"].as_str().unwrap()))
            .collect();
        assert_eq!(
            states,
            [
                ("org/wiki", "created"),
                ("org/plain", "created"),
                ("org/old", "archived")
            ]
        );
        assert_eq!(forge.writes, ["create org/wiki", "create org/plain"]);
        // The twin is provisioned (in the first batch) before any mirror exists; every key follows in one batch.
        assert_eq!(
            &host.lines[..3],
            [
                "ensure org/wiki main 1",
                "ensure org/plain main 0",
                "ensure org/old main 0"
            ]
        );
        assert_eq!(
            &host.lines[3..],
            [
                "admit org/wiki ssh-ed25519 KEY1",
                "admit org/plain ssh-ed25519 KEY2"
            ]
        );

        // A second pass changes nothing and admits the same keys again.
        forge.writes.clear();
        host.lines.clear();
        let (rows, incomplete) =
            reconcile(&copy(), &source(), &mut forge, &mut host, true, None).unwrap();
        assert!(!incomplete && forge.writes.is_empty());
        assert_eq!(rows[0]["state"], "current");
        assert_eq!(host.lines.len(), 3 + 2);
    }

    #[test]
    fn one_repository_of_an_event_touches_only_that_repository() {
        let mut forge =
            Forge::default()
                .repo(1, "Org/Wiki", true, false)
                .repo(2, "org/plain", false, false);
        let mut host = Host::default();
        let (rows, incomplete) = reconcile(
            &copy(),
            &source(),
            &mut forge,
            &mut host,
            true,
            Some("org/PLAIN"),
        )
        .unwrap();
        assert!(!incomplete);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0]["source"].as_str(), rows[0]["state"].as_str()),
            (Some("org/plain"), Some("created"))
        );
        assert_eq!(forge.writes, ["create org/plain"]);
        assert_eq!(
            host.lines,
            [
                "ensure org/plain main 0",
                "admit org/plain ssh-ed25519 KEY1"
            ]
        );

        // A repository the forge does not list is reported, and nothing is sent.
        forge.writes.clear();
        host.lines.clear();
        let (rows, incomplete) = reconcile(
            &copy(),
            &source(),
            &mut forge,
            &mut host,
            true,
            Some("org/gone"),
        )
        .unwrap();
        assert!(incomplete && forge.writes.is_empty() && host.lines.is_empty());
        assert_eq!(rows[0]["state"], "not-listed");
    }

    #[test]
    fn a_mirror_older_than_its_wikis_twin_is_replaced_because_forgejo_probes_the_wiki_only_at_creation(
    ) {
        let mut forge = Forge::default().repo(1, "org/late", true, false);
        forge.mirror("org/late", "ssh://192.0.2.6/org/late.git", "10m0s");
        let mut host = Host::default();
        let (rows, _) = reconcile(&copy(), &source(), &mut forge, &mut host, true, None).unwrap();
        assert_eq!(rows[0]["state"], "replaced");
        assert_eq!(forge.writes, ["delete org/late", "create org/late"]);
        assert_eq!(forge.mirrors["org/late"].len(), 1);
        // Once the twin exists the same mirror is left alone.
        forge.writes.clear();
        reconcile(&copy(), &source(), &mut forge, &mut host, true, None).unwrap();
        assert!(forge.writes.is_empty());
    }

    #[test]
    fn a_renamed_repository_and_a_wrong_interval_replace_the_mirror_and_two_mirrors_stop_the_pass()
    {
        let mut forge = Forge::default()
            .repo(1, "org/new", false, false)
            .repo(2, "org/slow", false, false);
        forge.mirror("org/new", "ssh://192.0.2.6/org/old.git", "10m0s");
        forge.mirror("org/slow", "ssh://192.0.2.6/org/slow.git", "1h0m0s");
        let mut host = Host::default();
        let (rows, _) = reconcile(&copy(), &source(), &mut forge, &mut host, true, None).unwrap();
        assert!(rows.iter().all(|r| r["state"] == "replaced"));
        assert_eq!(
            forge.mirrors["org/new"][0]["remote_address"],
            "ssh://192.0.2.6/org/new.git"
        );

        forge.mirror("org/new", "ssh://192.0.2.6/org/new.git", "10m0s");
        assert!(reconcile(&copy(), &source(), &mut forge, &mut host, true, None).is_err());
    }

    #[test]
    fn a_read_only_pass_writes_nothing_and_never_reaches_the_receiver() {
        let mut forge = Forge::default()
            .repo(1, "org/a", false, false)
            .repo(2, "org/b", true, false);
        forge.mirror("org/a", "ssh://192.0.2.6/org/a.git", "10m0s");
        let (rows, incomplete) =
            reconcile(&copy(), &source(), &mut forge, &mut NoReceiver, false, None).unwrap();
        assert!(incomplete && forge.writes.is_empty());
        assert_eq!(
            (rows[0]["state"].as_str(), rows[1]["state"].as_str()),
            (Some("current"), Some("create-planned"))
        );
    }

    #[test]
    fn other_hosts_mirrors_are_not_the_copys() {
        let mirror = json!({"remote_address":"ssh://gitlab.example/org/a.git"});
        assert!(!is_copy_mirror(Some(&copy()), &mirror));
        assert!(is_copy_mirror(
            Some(&copy()),
            &json!({"remote_address":"ssh://192.0.2.6/org/a.git"})
        ));
        assert!(!is_copy_mirror(None, &mirror));
        assert!(ours(&copy(), &[mirror]).unwrap().is_none());
    }
}
