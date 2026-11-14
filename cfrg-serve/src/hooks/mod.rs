//! Event hooks that run an external command per event (slot-a consolidation, R56: cfrg-serve is the ONE receiver).
//!
//! Each hook is its own module: it knows its path and turns a verified Forgejo payload into a [`Job`] (a command key and
//! the arguments to append). The command itself is configuration (`hook_commands`, argv by key), so the service stays
//! free of provider code. Jobs run one at a time in one worker; a job already queued for the same key is dropped as a
//! duplicate. One attempt per event (R30): nothing is retried; the next event, or the daily drift run, repairs a failure.
use crate::server::{log, Bridge};
use http::HeaderMap;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub mod copy;
pub mod downsync;
pub mod enroll;
pub mod gate;
pub mod handoff;
pub mod lockrefresh;

/// One unit of work: run `hook_commands[command]` with `args` appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub command: &'static str,
    /// Identity for de-duplication of queued jobs.
    pub key: String,
    pub args: Vec<String>,
}

type Planner = fn(&HeaderMap, &Value) -> Option<Job>;

/// path -> planner. THE registration point: one row per hook module.
const HOOKS: &[(&str, Planner)] = &[
    (copy::PATH, copy::plan),
    (downsync::PATH, downsync::plan),
    (enroll::PATH, enroll::plan),
    (gate::PATH, gate::plan),
    (handoff::PATH, handoff::plan),
    (lockrefresh::PATH, lockrefresh::plan),
];

pub fn lookup(path: &str) -> Option<&'static str> {
    HOOKS.iter().find(|(p, _)| *p == path).map(|(p, _)| *p)
}

pub fn plan(path: &str, headers: &HeaderMap, payload: &Value) -> Option<Job> {
    HOOKS
        .iter()
        .find(|(p, _)| *p == path)
        .and_then(|(_, f)| f(headers, payload))
}

pub(crate) fn event(headers: &HeaderMap) -> &str {
    ["x-forgejo-event", "x-gitea-event"]
        .iter()
        .find_map(|n| headers.get(*n).and_then(|v| v.to_str().ok()))
        .unwrap_or("")
}

/// `OWNER/REPO` as the forge spells it.
pub(crate) fn valid_repo(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.len() <= 100
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    matches!(s.split_once('/'), Some((o, r)) if part(o) && part(r) && !r.contains('/'))
}

/// The queue: a bounded channel plus the set of keys waiting in it.
pub struct Queue {
    tx: mpsc::Sender<Job>,
    pending: Mutex<HashSet<String>>,
}

impl Queue {
    pub fn new() -> (Self, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::channel(256);
        (
            Self {
                tx,
                pending: Mutex::new(HashSet::new()),
            },
            rx,
        )
    }

    /// "queued", "duplicate" or "busy".
    pub fn enqueue(&self, job: Job) -> &'static str {
        let id = format!("{}/{}", job.command, job.key);
        if !self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone())
        {
            return "duplicate";
        }
        if self.tx.try_send(job).is_err() {
            self.forget(&id);
            return "busy";
        }
        "queued"
    }

    pub fn forget(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
}

/// A verified event for `path`: plan it, check the command is configured, queue it. -> (status, text)
pub fn accept(
    bridge: &Arc<Bridge>,
    path: &str,
    headers: &HeaderMap,
    payload: &Value,
) -> (u16, &'static str) {
    let Some(job) = plan(path, headers, payload) else {
        return (200, "ignored"); // another event or action: the sender must not retry
    };
    if !bridge.ctx().cfg.hook_commands.contains_key(job.command) {
        log(
            "refused",
            json!({"path": path, "reason": "no command configured"}),
        );
        return (503, "hook is not configured");
    }
    let (command, key) = (job.command, job.key.clone());
    let state = bridge.hooks.enqueue(job);
    log(
        "accepted",
        json!({"kind": "hook", "command": command, "key": key, "state": state}),
    );
    match state {
        "busy" => (503, "busy"),
        s => (202, s),
    }
}

/// The one worker: jobs in order, each a child process, one log line with the exit status.
pub fn run(bridge: Arc<Bridge>, mut rx: mpsc::Receiver<Job>) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            bridge.hooks.forget(&format!("{}/{}", job.command, job.key));
            let exit = match bridge.ctx().cfg.hook_commands.get(job.command) {
                Some(argv) if !argv.is_empty() => tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .args(&job.args)
                    .status()
                    .await
                    .ok()
                    .and_then(|s| s.code())
                    .unwrap_or(1),
                _ => 1,
            };
            log(
                "done",
                json!({"kind": "hook", "command": job.command, "key": job.key, "exit": exit}),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_drops_a_duplicate_until_taken() {
        let (q, mut rx) = Queue::new();
        let job = || Job {
            command: "copy",
            key: "a/b".into(),
            args: vec![],
        };
        assert_eq!(q.enqueue(job()), "queued");
        assert_eq!(q.enqueue(job()), "duplicate");
        let taken = rx.try_recv().unwrap();
        q.forget(&format!("{}/{}", taken.command, taken.key));
        assert_eq!(q.enqueue(job()), "queued");
    }

    #[test]
    fn repo_shape() {
        assert!(valid_repo("corbet-cid/cfrg"));
        assert!(!valid_repo("a/b/c") && !valid_repo("../x") && !valid_repo("a/"));
    }
}
