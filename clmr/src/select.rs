//! Pure store selection. No I/O: presence answers arrive through [`Prober`].
use crate::{
    failure,
    validate::{canonical_url, store_repo_url, RefKind, Request, Store},
    Error,
};
use serde::{Deserialize, Serialize};

/// Presence answer for one (store, repo, need). Only `Hit` routes; every
/// other outcome falls back to the canonical pointer. `Unsupported` marks a
/// well-formed store whose provider kind has no probe (skipped, never
/// guessed); it is reported in the decision note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    Miss,
    Denied,
    Error,
    Unsupported,
}

/// Minimal probe primitive: does this store hold this exact need?
/// Implementations must bound every call by the request timeout and must
/// never forward one store's credential to another origin.
pub trait Prober {
    fn probe(&self, store: usize, path: &str, need: &RefKind) -> Outcome;

    /// Can the canonical pointer, and the primary forge behind it, answer for
    /// this repository right now? Asked ONLY for a moving ref that no
    /// primary-identity store served, to decide the emergency fallback below.
    /// `None` means "not asked / unknown" and keeps the pure canonical
    /// fallback, so probers that do not implement it never change a routing.
    fn canonical_reachable(&self, _path: &str) -> Option<bool> {
        None
    }
}

/// One repo decision: routed store fetch or canonical pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDecision {
    pub id: String,
    pub path: String,
    pub r#ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    pub outcome: Routing,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_url: Option<String>,
    pub note: String,
    pub instead_of: Vec<(String, String)>,
}

/// Routing outcome. Wire format matches the former plain strings, so the
/// published contract is unchanged (`"routed"`, `"canonical-pointer"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Routing {
    Routed,
    CanonicalPointer,
    /// The canonical pointer or the primary forge could not answer, so a
    /// moving ref is served from a declared store that is NOT its primary.
    /// The copy may lag the primary; consumers must surface the decision note
    /// as a warning. Pinned commits never need this: they are verified.
    EmergencyFallback,
}

/// Full response: decisions plus credential scopes and rendered git config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub schema: u32,
    pub canonical_base: String,
    pub decisions: Vec<RepoDecision>,
    pub credentials: Vec<crate::CredentialScope>,
    pub git_config: String,
}

fn ref_label(need: &RefKind) -> String {
    match need {
        RefKind::Pinned(sha) => format!("pinned:{sha}"),
        RefKind::Moving(gitref) => format!("moving:{gitref}"),
    }
}

/// Pure selection over validated requests and probe answers.
pub fn select(request: &Request, prober: &dyn Prober) -> Result<Response, Error> {
    request.validate()?;
    let mut decisions = Vec::new();
    // insteadOf cannot distinguish refs: the same repo requested twice must
    // never resolve to two destinations.
    let mut routed: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    for repo in &request.repositories {
        let decision = decide(request, prober, repo)?;
        let destination = decision.via.clone();
        if let Some(previous) = routed.insert(repo.path.clone(), destination.clone()) {
            if previous != destination {
                return Err(failure(format!(
                    "Repository {} resolves pinned and moving refs to different destinations; refusing to route",
                    repo.path
                )));
            }
        }
        decisions.push(decision);
    }
    // Identical declared source URLs must never map to different
    // destinations (git would apply last-wins); fail closed instead.
    {
        let mut routes: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for decision in &decisions {
            for (from, to) in &decision.instead_of {
                if let Some(previous) = routes.insert(from.as_str(), to.as_str()) {
                    if previous != to.as_str() {
                        return Err(failure(format!(
                            "Declared source URL {from} maps to conflicting destinations; refusing to route"
                        )));
                    }
                }
            }
        }
    }
    let credentials = crate::render_credentials(request, &decisions);
    let git_config = crate::render_git_config(request, &decisions);
    Ok(Response {
        schema: 1,
        canonical_base: request.canonical_base.clone(),
        decisions,
        credentials,
        git_config,
    })
}

fn decide(
    request: &Request,
    prober: &dyn Prober,
    repo: &crate::Repository,
) -> Result<RepoDecision, Error> {
    let indexed: Vec<(usize, &Store)> = request
        .stores
        .iter()
        .enumerate()
        .filter(|(_, store)| store.covers(&repo.path))
        .collect();
    match &repo.r#ref {
        RefKind::Pinned(_) => {
            let mut skipped = 0u32;
            for (index, store) in indexed {
                match prober.probe(index, &repo.path, &repo.r#ref) {
                    Outcome::Hit => {
                        let via = store_repo_url(store, &repo.path);
                        return Ok(routed(request, repo, index, &via, "pinned commit verified"));
                    }
                    Outcome::Unsupported => skipped += 1,
                    Outcome::Miss | Outcome::Denied | Outcome::Error => continue,
                }
            }
            Ok(pointer_with(request, repo, skipped))
        }
        RefKind::Moving(_) => {
            let Some(primary) = repo.primary.as_deref() else {
                // Unknown primary: the canonical pointer, unless it cannot
                // answer and a declared store holds the ref (emergency).
                if request.emergency_fallback
                    && prober.canonical_reachable(&repo.path) == Some(false)
                {
                    let mut skipped = 0u32;
                    for (index, store) in &indexed {
                        match prober.probe(*index, &repo.path, &repo.r#ref) {
                            Outcome::Hit => {
                                let via = store_repo_url(store, &repo.path);
                                return Ok(emergency(request, repo, *index, &via));
                            }
                            Outcome::Unsupported => skipped += 1,
                            Outcome::Miss | Outcome::Denied | Outcome::Error => continue,
                        }
                    }
                    return Ok(pointer_with(request, repo, skipped));
                }
                return Ok(pointer_with(request, repo, 0));
            };
            let mut skipped = 0u32;
            // Every same-identity store is probed in order; other identities
            // are never consulted for moving refs (and never probed) while the
            // canonical path can answer. First eligible hit wins; anything
            // else falls to the pointer. No retry or sync: each declared store
            // is probed at most once.
            for (index, store) in &indexed {
                let (index, store) = (*index, *store);
                if store.identity != primary {
                    continue;
                }
                match prober.probe(index, &repo.path, &repo.r#ref) {
                    Outcome::Hit => {
                        let via = store_repo_url(store, &repo.path);
                        return Ok(routed(
                            request,
                            repo,
                            index,
                            &via,
                            "moving ref verified on primary store",
                        ));
                    }
                    Outcome::Unsupported => skipped += 1,
                    Outcome::Miss | Outcome::Denied | Outcome::Error => continue,
                }
            }
            // Resilience (decided 2026-10-07): when the primary is another
            // forge, or unknown, the canonical pointer is the answer. Only if
            // the pointer or that primary cannot answer at all does a declared
            // store step in, for a moving ref, with a warning that it may lag.
            // A primary that HAS a declared store never takes this path: its
            // stores failing is the express lane failing, and the canonical
            // pointer is the next link of that chain.
            let primary_has_store = indexed.iter().any(|(_, store)| store.identity == primary);
            if request.emergency_fallback
                && !primary_has_store
                && prober.canonical_reachable(&repo.path) == Some(false)
            {
                for (index, store) in &indexed {
                    match prober.probe(*index, &repo.path, &repo.r#ref) {
                        Outcome::Hit => {
                            let via = store_repo_url(store, &repo.path);
                            return Ok(emergency(request, repo, *index, &via));
                        }
                        Outcome::Unsupported => skipped += 1,
                        Outcome::Miss | Outcome::Denied | Outcome::Error => continue,
                    }
                }
            }
            Ok(pointer_with(request, repo, skipped))
        }
    }
}

fn pointer_with(request: &Request, repo: &crate::Repository, skipped: u32) -> RepoDecision {
    // Pointer repos still publish their declared alias forms mapped to the
    // canonical pointer, so owned GitHub/explicit-forge URLs normalize
    // without any network probe. Canonical self-forms are skipped as no-ops.
    let pointer = canonical_url(&request.canonical_base, &repo.path);
    RepoDecision {
        id: repo.id.clone(),
        path: repo.path.clone(),
        r#ref: ref_label(&repo.r#ref),
        primary: repo.primary.clone(),
        outcome: Routing::CanonicalPointer,
        store: None,
        via: None,
        primary_url: repo.primary_url.clone(),
        note: pointer_note(request, repo, skipped),
        instead_of: crate::render::instead_of_pairs(repo, &pointer),
    }
}
fn routed(
    _request: &Request,
    repo: &crate::Repository,
    index: usize,
    via: &str,
    note: &str,
) -> RepoDecision {
    RepoDecision {
        id: repo.id.clone(),
        path: repo.path.clone(),
        r#ref: ref_label(&repo.r#ref),
        primary: repo.primary.clone(),
        outcome: Routing::Routed,
        store: Some(index),
        via: Some(via.into()),
        primary_url: repo.primary_url.clone(),
        note: format!("{note} on store {index}"),
        instead_of: crate::render::instead_of_pairs(repo, via),
    }
}

fn emergency(
    _request: &Request,
    repo: &crate::Repository,
    index: usize,
    via: &str,
) -> RepoDecision {
    RepoDecision {
        id: repo.id.clone(),
        path: repo.path.clone(),
        r#ref: ref_label(&repo.r#ref),
        primary: repo.primary.clone(),
        outcome: Routing::EmergencyFallback,
        store: Some(index),
        via: Some(via.into()),
        primary_url: repo.primary_url.clone(),
        note: format!(
            "EMERGENCY FALLBACK: the canonical pointer or the primary forge cannot answer; moving ref served from store {index}, which is not the primary and may lag it"
        ),
        instead_of: crate::render::instead_of_pairs(repo, via),
    }
}

fn pointer_note(request: &Request, repo: &crate::Repository, skipped: u32) -> String {
    let _ = request;
    let suffix = if skipped > 0 {
        format!(" ({skipped} store(s) skipped: unsupported provider, never guessed)")
    } else {
        String::new()
    };
    match &repo.r#ref {
        RefKind::Pinned(_) => {
            format!("no in-scope store verified the pinned commit{suffix}; canonical pointer")
        }
        RefKind::Moving(_) => {
            match repo.primary.as_deref() {
                None => format!("unknown primary{suffix}; canonical pointer"),
                Some(_) => {
                    format!("no primary-identity store verified the moving ref{suffix}; canonical pointer")
                }
            }
        }
    }
}
