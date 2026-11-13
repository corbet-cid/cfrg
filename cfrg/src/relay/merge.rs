//! RD3: three-way convergence, and the patch a sibling needs to reach the merged state. All pure.
//!
//! The router keeps, per sibling, the state it last synced (the snapshot). A sibling's local change is
//! its current state minus that snapshot. Changes merge into the canon:
//! - label sets by additions and removals (labels that exist on one forge only are never merged);
//! - open/closed by last processed wins (the sibling named by the hint is processed last);
//! - title and body come from the origin only: R50 relays edits "by their author", and the origin is
//!   the author's object (the router never writes an origin's text, model R41);
//! - assignees as identity-table person keys; a handle the table does not know is never relayed.

use super::model::{Canon, IssueState, RouterConfig, Snap};
use crate::model::Forge;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One sibling as the merge sees it: what it shows now and what the router last synced.
pub struct Input<'a> {
    pub forge: Forge,
    pub origin: bool,
    pub cur: &'a Snap,
    pub snap: Option<&'a Snap>,
}

fn person_keys(cfg: &RouterConfig, forge: Forge, handles: &BTreeSet<String>) -> BTreeSet<String> {
    handles
        .iter()
        .filter_map(|h| cfg.person_by_handle(forge, h).map(|p| p.key.clone()))
        .collect()
}

/// Merges the changes of `inputs` (in processing order, the hinted sibling last) into `canon`.
pub fn merge(cfg: &RouterConfig, canon: &Canon, inputs: &[Input<'_>]) -> Canon {
    let mut out = canon.clone();
    let empty = Snap::default();
    for input in inputs {
        if input.origin {
            out.title.clone_from(&input.cur.title);
            out.body.clone_from(&input.cur.body);
        }
        let snap = input.snap.unwrap_or(&empty);
        // A sibling never seen before contributes its state only as the origin (a new object).
        let state_changed = match input.snap {
            Some(known) => input.cur.state != known.state,
            None => input.origin,
        };
        if state_changed && input.cur.state.is_some() {
            out.state = input.cur.state;
        }
        let shared = |set: &BTreeSet<String>| -> BTreeSet<String> {
            set.iter()
                .filter(|l| !cfg.is_local_label(l))
                .cloned()
                .collect()
        };
        let (cur_l, snap_l) = (shared(&input.cur.labels), shared(&snap.labels));
        for added in cur_l.difference(&snap_l) {
            out.labels.insert(added.clone());
        }
        for removed in snap_l.difference(&cur_l) {
            out.labels.remove(removed);
        }
        let cur_a = person_keys(cfg, input.forge, &input.cur.assignees);
        let snap_a = person_keys(cfg, input.forge, &snap.assignees);
        for added in cur_a.difference(&snap_a) {
            out.assignees.insert(added.clone());
        }
        for removed in snap_a.difference(&cur_a) {
            out.assignees.remove(removed);
        }
    }
    out
}

/// The state a copy is converged to. `body` is the rendered text (credit, sibling list, markers);
/// `None` leaves title and body alone (an origin is never rewritten). Labels that exist on this forge
/// only are kept as they are; the held label is set exactly when the object is held on Forgejo.
pub fn desired(
    cfg: &RouterConfig,
    canon: &Canon,
    forge: Forge,
    cur: &Snap,
    body: Option<&str>,
    held: bool,
) -> Snap {
    let mut labels = canon.labels.clone();
    for kept in cur.labels.iter().filter(|l| cfg.is_local_label(l)) {
        if *kept != cfg.held_label {
            labels.insert(kept.clone());
        }
    }
    if held && forge == Forge::Forgejo {
        labels.insert(cfg.held_label.clone());
    }
    let mut assignees: BTreeSet<String> = canon
        .assignees
        .iter()
        .filter_map(|key| cfg.person(key).and_then(|p| p.handles.get(&forge)).cloned())
        .collect();
    // Handles on this forge that the identity table does not know are not ours to remove.
    for handle in &cur.assignees {
        if cfg.person_by_handle(forge, handle).is_none() {
            assignees.insert(handle.clone());
        }
    }
    Snap {
        title: if body.is_some() {
            canon.title.clone()
        } else {
            cur.title.clone()
        },
        body: body.map_or_else(|| cur.body.clone(), str::to_owned),
        state: canon.state.or(cur.state),
        labels,
        assignees,
    }
}

/// What to change on a sibling. Empty = nothing to write (RD4(b)).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<IssueState>,
    pub labels_add: BTreeSet<String>,
    pub labels_remove: BTreeSet<String>,
    pub assignees_add: BTreeSet<String>,
    pub assignees_remove: BTreeSet<String>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// What the post-write state of `snap` is.
    pub fn apply(&self, snap: &mut Snap) {
        if let Some(title) = &self.title {
            snap.title.clone_from(title);
        }
        if let Some(body) = &self.body {
            snap.body.clone_from(body);
        }
        if self.state.is_some() {
            snap.state = self.state;
        }
        for l in &self.labels_remove {
            snap.labels.remove(l);
        }
        snap.labels.extend(self.labels_add.iter().cloned());
        for a in &self.assignees_remove {
            snap.assignees.remove(a);
        }
        snap.assignees.extend(self.assignees_add.iter().cloned());
    }
}

pub fn diff(cur: &Snap, want: &Snap) -> Patch {
    Patch {
        title: (cur.title != want.title).then(|| want.title.clone()),
        body: (cur.body != want.body).then(|| want.body.clone()),
        state: (want.state.is_some() && cur.state != want.state)
            .then_some(want.state)
            .flatten(),
        labels_add: want.labels.difference(&cur.labels).cloned().collect(),
        labels_remove: cur.labels.difference(&want.labels).cloned().collect(),
        assignees_add: want.assignees.difference(&cur.assignees).cloned().collect(),
        assignees_remove: cur.assignees.difference(&want.assignees).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::model::Person;
    use std::collections::BTreeMap;

    fn cfg() -> RouterConfig {
        RouterConfig {
            people: vec![Person {
                key: "ada".into(),
                name: "Ada".into(),
                handles: BTreeMap::from([
                    (Forge::Github, "adal".into()),
                    (Forge::Gitlab, "ada-gl".into()),
                ]),
            }],
            ..RouterConfig::default()
        }
    }

    fn snap(state: IssueState, labels: &[&str]) -> Snap {
        Snap {
            title: "T".into(),
            body: "B".into(),
            state: Some(state),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            assignees: BTreeSet::new(),
        }
    }

    fn canon(state: IssueState, labels: &[&str]) -> Canon {
        let s = snap(state, labels);
        Canon {
            title: s.title,
            body: s.body,
            state: s.state,
            labels: s.labels,
            assignees: BTreeSet::new(),
        }
    }

    #[test]
    fn label_sets_merge_by_additions_and_removals() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &["bug", "p2"]);
        let a = snap(IssueState::Open, &["bug", "p2", "ui"]);
        let b = snap(IssueState::Open, &["bug"]);
        let merged = merge(
            &cfg,
            &canon(IssueState::Open, &["bug", "p2"]),
            &[
                Input {
                    forge: Forge::Github,
                    origin: true,
                    cur: &a,
                    snap: Some(&base),
                },
                Input {
                    forge: Forge::Gitlab,
                    origin: false,
                    cur: &b,
                    snap: Some(&base),
                },
            ],
        );
        assert_eq!(
            merged.labels,
            BTreeSet::from(["bug".to_owned(), "ui".to_owned()])
        );
    }

    #[test]
    fn local_labels_never_merge() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &[]);
        let cur = snap(IssueState::Open, &["held", "contribution", "bug"]);
        let merged = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[Input {
                forge: Forge::Forgejo,
                origin: false,
                cur: &cur,
                snap: Some(&base),
            }],
        );
        assert_eq!(merged.labels, BTreeSet::from(["bug".to_owned()]));
    }

    #[test]
    fn last_processed_wins_on_state() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &[]);
        let closed = snap(IssueState::Closed, &[]);
        let still_open = snap(IssueState::Open, &[]);
        let reopened_later = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[
                Input {
                    forge: Forge::Gitlab,
                    origin: false,
                    cur: &closed,
                    snap: Some(&base),
                },
                Input {
                    forge: Forge::Github,
                    origin: true,
                    cur: &still_open,
                    snap: Some(&closed),
                },
            ],
        );
        assert_eq!(reopened_later.state, Some(IssueState::Open));
        let closed_last = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[Input {
                forge: Forge::Gitlab,
                origin: false,
                cur: &closed,
                snap: Some(&base),
            }],
        );
        assert_eq!(closed_last.state, Some(IssueState::Closed));
    }

    #[test]
    fn title_and_body_come_from_the_origin_only() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &[]);
        let mut copy_edit = base.clone();
        copy_edit.title = "edited by a maintainer on a copy".into();
        let mut origin_edit = base.clone();
        origin_edit.title = "edited by the author".into();
        let merged = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[
                Input {
                    forge: Forge::Gitlab,
                    origin: false,
                    cur: &copy_edit,
                    snap: Some(&base),
                },
                Input {
                    forge: Forge::Github,
                    origin: true,
                    cur: &origin_edit,
                    snap: Some(&base),
                },
            ],
        );
        assert_eq!(merged.title, "edited by the author");
        let unchanged = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[Input {
                forge: Forge::Gitlab,
                origin: false,
                cur: &copy_edit,
                snap: Some(&base),
            }],
        );
        assert_eq!(unchanged.title, "T");
    }

    #[test]
    fn assignees_travel_only_where_the_table_maps_them() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &[]);
        let mut cur = base.clone();
        cur.assignees = BTreeSet::from(["adal".to_owned(), "unknown".to_owned()]);
        let merged = merge(
            &cfg,
            &canon(IssueState::Open, &[]),
            &[Input {
                forge: Forge::Github,
                origin: true,
                cur: &cur,
                snap: Some(&base),
            }],
        );
        assert_eq!(merged.assignees, BTreeSet::from(["ada".to_owned()]));
        let mut on_gitlab = snap(IssueState::Open, &[]);
        on_gitlab.assignees = BTreeSet::from(["foreign".to_owned()]);
        let want = desired(&cfg, &merged, Forge::Gitlab, &on_gitlab, None, false);
        assert_eq!(
            want.assignees,
            BTreeSet::from(["ada-gl".to_owned(), "foreign".to_owned()])
        );
    }

    #[test]
    fn merging_is_idempotent_and_a_converged_sibling_needs_no_patch() {
        let cfg = cfg();
        let base = snap(IssueState::Open, &["bug"]);
        let cur = snap(IssueState::Closed, &["bug", "ui"]);
        let inputs = [Input {
            forge: Forge::Github,
            origin: true,
            cur: &cur,
            snap: Some(&base),
        }];
        let once = merge(&cfg, &canon(IssueState::Open, &["bug"]), &inputs);
        let twice = merge(&cfg, &once, &inputs);
        assert_eq!(once, twice);
        let want = desired(&cfg, &once, Forge::Github, &cur, None, false);
        assert!(diff(&cur, &want).is_empty());
    }

    #[test]
    fn held_label_follows_the_hold_and_local_labels_stay() {
        let cfg = cfg();
        let c = canon(IssueState::Open, &["bug"]);
        let cur = snap(IssueState::Open, &["bug", "contribution", "held"]);
        let released = desired(&cfg, &c, Forge::Forgejo, &cur, None, false);
        assert_eq!(
            released.labels,
            BTreeSet::from(["bug".to_owned(), "contribution".to_owned()])
        );
        let held = desired(
            &cfg,
            &c,
            Forge::Forgejo,
            &snap(IssueState::Open, &["bug"]),
            None,
            true,
        );
        assert!(held.labels.contains("held"));
        let elsewhere = desired(
            &cfg,
            &c,
            Forge::Github,
            &snap(IssueState::Open, &["bug"]),
            None,
            true,
        );
        assert!(!elsewhere.labels.contains("held"));
    }

    #[test]
    fn patch_applies_to_what_it_was_diffed_from() {
        let cur = snap(IssueState::Open, &["a", "b"]);
        let mut want = snap(IssueState::Closed, &["b", "c"]);
        want.title = "N".into();
        let patch = diff(&cur, &want);
        assert!(!patch.is_empty());
        let mut after = cur.clone();
        patch.apply(&mut after);
        assert_eq!(after, want);
    }
}
