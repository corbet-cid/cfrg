//! Git transport with isolated objects, exact refspecs and one bounded deadline.
use crate::{failure, process::Runner, Environment, Result};
use std::{collections::BTreeMap, path::Path, time::Duration};

pub(super) struct Git {
    pub runner: Runner,
}

impl Git {
    pub fn new(root: &Path, timeout: Duration) -> Result<Self> {
        let mut environment: Environment = std::env::vars_os().collect();
        environment.retain(|key, _| !key.to_string_lossy().starts_with("GIT_TRACE"));
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_SHALLOW_FILE",
            "GIT_REPLACE_REF_BASE",
            "GIT_CURL_VERBOSE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG",
            "GIT_EXEC_PATH",
            "GIT_SSL_NO_VERIFY",
        ] {
            environment.remove(std::ffi::OsStr::new(key));
        }
        for (key, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            // Only explicit caller-supplied transport settings (GIT_CONFIG_COUNT,
            // askpass and SSH) are trusted, never ambient user/system Git config.
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_PROTOCOL_FROM_USER", "0"),
            ("LC_ALL", "C"),
        ] {
            environment.insert(key.into(), value.into());
        }
        Ok(Self {
            runner: Runner::new(root.into(), environment, timeout)?
                .with_stderr_events()
                .without_child_stderr(),
        })
    }

    pub fn argv(&self, args: &[String]) -> Vec<String> {
        let mut command: Vec<String> = [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.sslVerify=true",
            "-c",
            "credential.helper=",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "gc.auto=0",
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        command.extend_from_slice(args);
        command
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.run_owned(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>())
    }

    pub fn run_owned(&self, args: &[String]) -> Result<String> {
        self.runner.run(&self.argv(args), true)
    }

    pub fn inventory(
        &self,
        url: &str,
        refs: &[String],
        all: bool,
    ) -> Result<BTreeMap<String, String>> {
        let mut args = vec!["ls-remote".into(), "--refs".into(), "--".into(), url.into()];
        if all {
            args.extend(["refs/heads/*".into(), "refs/tags/*".into()]);
        } else {
            args.extend_from_slice(refs);
        }
        let text = self.run_owned(&args)?;
        let mut result = BTreeMap::new();
        for line in text.lines() {
            if result.len() >= 4096 || line.len() > 1100 {
                return Err(failure("Git ref inventory exceeds reconciliation limits"));
            }
            let (oid, name) = line
                .split_once('\t')
                .ok_or_else(|| failure("Malformed Git ref inventory"))?;
            if !valid_oid(oid) || !supported_ref(name) || (!all && !refs.iter().any(|r| r == name))
            {
                return Err(failure("Unexpected Git ref inventory entry"));
            }
            if result.insert(name.into(), oid.into()).is_some() {
                return Err(failure("Duplicate Git ref inventory entry"));
            }
        }
        Ok(result)
    }

    pub fn fetch(
        &self,
        url: &str,
        objects: &BTreeMap<String, String>,
        namespace: &str,
    ) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--no-write-fetch-head".into(),
            "--".into(),
            url.into(),
        ];
        for (index, oid) in objects.values().enumerate() {
            args.push(format!("{oid}:refs/ccid/{namespace}/{index}"));
        }
        self.run_owned(&args)?;
        for (index, oid) in objects.values().enumerate() {
            if self.run(&[
                "rev-parse",
                "--verify",
                &format!("refs/ccid/{namespace}/{index}"),
            ])? != *oid
            {
                return Err(failure(
                    "Fetched Git object does not match the observed source",
                ));
            }
        }
        if self.run(&["rev-parse", "--is-shallow-repository"])? != "false" {
            return Err(failure("Reconciliation requires complete Git ancestry"));
        }
        Ok(())
    }
}

pub(super) fn supported_ref(name: &str) -> bool {
    name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
}

pub(super) fn valid_oid(oid: &str) -> bool {
    [40, 64].contains(&oid.len())
        && oid
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
