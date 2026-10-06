//! Profile projection: leading-dot repositories are projected, not mirrored.
//!
//! Organization profile repositories (`.github`, `.profile`, …) carry
//! per-forge conventions that do not survive a byte-for-byte mirror: profile
//! paths, org-wide default directories and name rules differ per forge.
//! This module projects canonical content across those conventions instead.
//! Pure policy: no network, clock or filesystem access here.

use crate::{failure, model::Forge, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Planner bounds; oversized projections fail closed.
const MAX_PROJECTED_FILES: usize = 1024;
const MAX_PROJECTED_BYTES: usize = 1024 * 1024;

/// One forge's profile conventions. Implemented once per adapter (`cfgj`,
/// `cglb`, `cbkt`, `cghb`).
pub trait ProfileConvention {
    /// Profile repository name, or `None` where the forge keeps the profile
    /// in a workspace/account description instead of a repository.
    fn profile_repo(&self) -> Option<&'static str>;
    /// Profile README path inside the profile repository, if any.
    fn readme_path(&self) -> Option<&'static str>;
    /// Per-repository directory receiving org-wide default files, if any.
    fn default_dir(&self) -> Option<&'static str>;
    /// Whether the profile is read-only while the forge is frozen: nothing
    /// may be projected into it then.
    fn frozen_readonly(&self) -> bool;
    /// Whether a repository name is usable on this forge.
    fn valid_repo_name(&self, name: &str) -> bool {
        plain_name(name)
    }
}

/// Shared name hygiene shared by every forge's repository names.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether a repository is a profile repository: its final path segment
/// starts with a dot (`.github`, `.profile`, …). Profile repositories are
/// never mirrored 1:1; project their content instead.
pub fn is_profile_repo(name: &str) -> bool {
    name.rsplit('/').next().is_some_and(|s| s.starts_with('.'))
}

/// One projected file: destination repository, repo-relative path and
/// exact bytes. Coordinates are explicit so no future applier can double a
/// repo prefix: `repo` is the repository path as in policy locations
/// (`group/widget`), `path` never contains it. Profile READMEs resolve
/// their repo-relative path through the target convention (`readme_path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedFile {
    pub repo: String,
    pub path: String,
    pub content: Vec<u8>,
}

/// Planned paths stay inside the target repository on every OS: reject
/// traversal, absolute paths, Windows separators (`\\`) and drive prefixes
/// (`:`), and control characters (including NUL and newlines), so planned
/// files cannot escape when an applier executes them. Trailing dots and
/// spaces are rejected too: Windows strips them, which would redirect the
/// write.
fn relative_file(path: &str) -> Result<&str> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', ':', '\0']) {
        return Err(failure(format!("Projected file path escapes: {path}")));
    }
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.chars().any(|c| c.is_control())
            || part.ends_with(['.', ' '])
        {
            return Err(failure(format!("Projected file path escapes: {path}")));
        }
    }
    Ok(path)
}

/// Project org-wide default files into one repository: every source file
/// lands under the target forge's default directory, but only where the
/// target repository has no file of its own. Source files are keyed relative
/// to the source default directory (for example `workflows/ci.yml`).
pub fn project_profile(
    source_repo: &str,
    source_files: &BTreeMap<String, Vec<u8>>,
    target_repo: &str,
    target_has: &BTreeSet<String>,
    source: &dyn ProfileConvention,
    target: &dyn ProfileConvention,
    target_frozen: bool,
) -> Result<Vec<ProjectedFile>> {
    if target_frozen && target.frozen_readonly() {
        return Err(failure(
            "Target profile is frozen read-only; refusing writes",
        ));
    }
    let Some(source_profile) = source.profile_repo() else {
        return Err(failure("Source forge keeps no profile repository"));
    };
    if source_repo != source_profile {
        return Err(failure("Projection sources must be the profile repository"));
    }
    if !source.valid_repo_name(source_repo) || !target.valid_repo_name(target_repo) {
        return Err(failure("Invalid repository name for the projection"));
    }
    if is_profile_repo(target_repo) {
        return Err(failure(
            "Defaults project into regular repositories, never into a profile repository",
        ));
    }
    let Some(dir) = target.default_dir() else {
        return Err(failure(
            "Target forge has no default-file directory convention",
        ));
    };
    if source_files.len() > MAX_PROJECTED_FILES {
        return Err(failure("Projection exceeds reconciliation limits"));
    }
    let mut projected = Vec::new();
    for (name, content) in source_files {
        relative_file(name)?;
        if content.len() > MAX_PROJECTED_BYTES {
            return Err(failure("Projected file exceeds reconciliation limits"));
        }
        let path = format!("{dir}/{name}");
        if !target_has.contains(&path) {
            projected.push(ProjectedFile {
                repo: target_repo.into(),
                path,
                content: content.clone(),
            });
        }
    }
    Ok(projected)
}

/// Pure output for forges that keep the profile in a workspace or account
/// description rather than a repository (Bitbucket): the exact description
/// text an applier would PUT, with the workspace it belongs to. There is no
/// live write transport behind this type; planners produce data, and no
/// applier executes it yet. See the crate docs for the planner/executable
/// split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedDescription {
    pub workspace: String,
    pub text: String,
}

/// Project one profile README to a workspace description, unless the
/// workspace already documents itself. This is the description-style twin
/// of [`project_readme`]: it applies exactly where the target convention
/// keeps no profile file (`readme_path` is `None`) and refuses otherwise,
/// so callers cannot silently produce no work on the wrong forge kind.
pub fn project_description(
    readme: &[u8],
    workspace: &str,
    target_has_description: bool,
    target: &dyn ProfileConvention,
    target_frozen: bool,
) -> Result<Option<ProjectedDescription>> {
    if target_frozen && target.frozen_readonly() {
        return Err(failure(
            "Target profile is frozen read-only; refusing writes",
        ));
    }
    if target.readme_path().is_some() {
        return Err(failure(
            "Target forge keeps profile READMEs in a repository; use project_readme",
        ));
    }
    if !target.valid_repo_name(workspace) || workspace.contains('/') {
        return Err(failure("Invalid workspace for description projection"));
    }
    if readme.is_empty() || readme.len() > MAX_PROJECTED_BYTES {
        return Err(failure("Invalid profile README for projection"));
    }
    let text = std::str::from_utf8(readme)
        .map_err(|_| failure("Profile README is not valid UTF-8 for a description"))?;
    if target_has_description {
        return Ok(None);
    }
    Ok(Some(ProjectedDescription {
        workspace: workspace.into(),
        text: text.into(),
    }))
}

/// Project one profile README into the target profile repository, unless
/// it already documents itself. `target_repo` must be that forge's profile
/// repository and pass its name rules; the output path is repo-relative, so
/// an applier joins it with `repo` exactly once. For workspace-description
/// forges (no profile file) see [`project_description`] instead.
pub fn project_readme(
    readme: &[u8],
    target_repo: &str,
    target: &dyn ProfileConvention,
    target_has_readme: bool,
    target_frozen: bool,
) -> Result<Option<ProjectedFile>> {
    if target_frozen && target.frozen_readonly() {
        return Err(failure(
            "Target profile is frozen read-only; refusing writes",
        ));
    }
    if readme.is_empty() || readme.len() > MAX_PROJECTED_BYTES {
        return Err(failure("Invalid profile README for projection"));
    }
    let Some(profile) = target.profile_repo() else {
        return Ok(None);
    };
    if target_repo != profile || !target.valid_repo_name(target_repo) {
        return Err(failure(
            "README projection targets the forge profile repository only",
        ));
    }
    let Some(path) = target.readme_path() else {
        return Ok(None);
    };
    if target_has_readme {
        return Ok(None);
    }
    Ok(Some(ProjectedFile {
        repo: target_repo.into(),
        path: path.into(),
        content: readme.into(),
    }))
}

/// Which forge a profile repository belongs to, if any. Used by planners
/// that resolve conventions per repository.
pub fn profile_forge(
    name: &str,
    conventions: &BTreeMap<Forge, &'static dyn ProfileConvention>,
) -> Option<Forge> {
    conventions
        .iter()
        .find(|(_, convention)| convention.profile_repo() == Some(name))
        .map(|(forge, _)| *forge)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        profile_repo: Option<&'static str>,
        readme_path: Option<&'static str>,
        default_dir: Option<&'static str>,
        frozen_readonly: bool,
    }
    impl ProfileConvention for Fixture {
        fn profile_repo(&self) -> Option<&'static str> {
            self.profile_repo
        }
        fn readme_path(&self) -> Option<&'static str> {
            self.readme_path
        }
        fn default_dir(&self) -> Option<&'static str> {
            self.default_dir
        }
        fn frozen_readonly(&self) -> bool {
            self.frozen_readonly
        }
        fn valid_repo_name(&self, name: &str) -> bool {
            // Permissive like the dot-profile forges; stricter rules
            // (GitLab leading dots, Bitbucket case) are tested per adapter.
            plain_name(name)
        }
    }

    fn source() -> Fixture {
        Fixture {
            profile_repo: Some(".github"),
            readme_path: Some(".github/profile/README.md"),
            default_dir: Some(".github"),
            frozen_readonly: true,
        }
    }
    fn target() -> Fixture {
        Fixture {
            profile_repo: Some(".profile"),
            readme_path: Some("README.md"),
            default_dir: Some(".forgejo"),
            frozen_readonly: false,
        }
    }
    fn files() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("workflows/ci.yml".into(), b"jobs: {}".to_vec()),
            ("labels.yml".into(), b"labels: []".to_vec()),
        ])
    }

    #[test]
    fn leading_dot_repositories_are_profile_sources_never_mirrors() {
        assert!(is_profile_repo(".github"));
        assert!(is_profile_repo("group/.profile"));
        assert!(!is_profile_repo("group/widget"));
        assert!(!is_profile_repo("widget"));
    }

    #[test]
    fn defaults_project_only_where_the_target_has_none() {
        let present = BTreeSet::from([".forgejo/labels.yml".into()]);
        let projected = project_profile(
            ".github",
            &files(),
            "group/widget",
            &present,
            &source(),
            &target(),
            false,
        )
        .unwrap();
        assert_eq!(
            projected,
            vec![ProjectedFile {
                repo: "group/widget".into(),
                path: ".forgejo/workflows/ci.yml".into(),
                content: b"jobs: {}".to_vec(),
            }]
        );
        // Repo and path never double the prefix: joining them is the
        // applier's whole job.
        assert!(!projected[0].path.contains("group/widget"));
        assert!(!projected[0].path.starts_with(".forgejo/.forgejo"));
    }

    #[test]
    fn projection_refuses_non_profile_sources_and_profile_targets() {
        let target = target();
        assert!(project_profile(
            "group/widget",
            &files(),
            "group/widget",
            &BTreeSet::new(),
            &source(),
            &target,
            false,
        )
        .is_err());
        assert!(project_profile(
            ".github",
            &files(),
            ".profile",
            &BTreeSet::new(),
            &source(),
            &target,
            false,
        )
        .is_err());
        assert!(project_profile(
            ".github",
            &BTreeMap::from([("../escape".into(), Vec::new())]),
            "group/widget",
            &BTreeSet::new(),
            &source(),
            &target,
            false,
        )
        .is_err());
        assert!(project_profile(
            ".github",
            &files(),
            "",
            &BTreeSet::new(),
            &source(),
            &target,
            false,
        )
        .is_err());
    }

    #[test]
    fn frozen_read_only_targets_refuse_writes() {
        let frozen = Fixture {
            profile_repo: Some(".github"),
            readme_path: Some("profile/README.md"),
            default_dir: Some(".github"),
            frozen_readonly: true,
        };
        assert!(project_profile(
            ".github",
            &files(),
            "group/widget",
            &BTreeSet::new(),
            &source(),
            &frozen,
            true,
        )
        .is_err());
        assert!(project_readme(b"# Profile", ".github", &frozen, false, true).is_err());
        assert!(project_readme(b"# Profile", "group/widget", &source(), false, false).is_err());
        let readme = project_readme(b"# Profile", ".profile", &target(), false, false)
            .unwrap()
            .unwrap();
        assert_eq!(readme.repo, ".profile");
        assert_eq!(readme.path, "README.md");
    }

    #[test]
    fn profile_repositories_resolve_to_their_forge() {
        let conventions: BTreeMap<Forge, &'static dyn ProfileConvention> = BTreeMap::new();
        assert_eq!(profile_forge(".github", &conventions), None);
    }

    #[test]
    fn projected_paths_reject_traversal_separators_and_controls() {
        let target = target();
        for evil in [
            "../escape",
            "a/../../b",
            "a\\b",
            "..\\..\\x",
            "C:/x",
            "C:x",
            "a\u{1}b",
            "a\nb",
            "/abs",
            "a//b",
            "a/./b",
            "trailing./x",
            "trailing /x",
            "",
        ] {
            let mut files = BTreeMap::new();
            files.insert(evil.into(), Vec::new());
            assert!(
                project_profile(
                    ".github",
                    &files,
                    "group/widget",
                    &BTreeSet::new(),
                    &source(),
                    &target,
                    false,
                )
                .is_err(),
                "{evil:?}"
            );
        }
        let mut files = BTreeMap::new();
        files.insert("nested/dir/ok.md".into(), b"fine".to_vec());
        let projected = project_profile(
            ".github",
            &files,
            "group/widget",
            &BTreeSet::new(),
            &source(),
            &target,
            false,
        )
        .unwrap();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].path, ".forgejo/nested/dir/ok.md");
    }

    #[test]
    fn workspace_descriptions_project_text_without_a_file() {
        let bare = Fixture {
            profile_repo: None,
            readme_path: None,
            default_dir: None,
            frozen_readonly: false,
        };
        let projected = project_description(
            b"# Org\n\nTools for authors.\n",
            "myworkspace",
            false,
            &bare,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(projected.workspace, "myworkspace");
        assert!(projected.text.contains("Tools for authors."));
        assert_eq!(
            project_description(b"# Org", "myworkspace", true, &bare, false).unwrap(),
            None
        );
        assert!(project_description(b"", "myworkspace", false, &bare, false).is_err());
        assert!(project_description(b"\xff\xfe", "myworkspace", false, &bare, false).is_err());
        assert!(project_description(b"# Org", "", false, &bare, false).is_err());
    }

    #[test]
    fn description_projection_refuses_file_conventions_and_frozen_writes() {
        // A file-convention target must use project_readme, never silently
        // produce no work through the description path.
        assert!(project_description(b"# Org", "myworkspace", false, &target(), false).is_err());
        let frozen_bare = Fixture {
            profile_repo: None,
            readme_path: None,
            default_dir: None,
            frozen_readonly: true,
        };
        assert!(project_description(b"# Org", "myworkspace", false, &frozen_bare, true).is_err());
        // ... but an unfrozen read-only-capable forge still projects.
        assert!(
            project_description(b"# Org", "myworkspace", false, &frozen_bare, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn readme_without_a_target_path_or_with_one_present_projects_nothing() {
        let bare = Fixture {
            profile_repo: None,
            readme_path: None,
            default_dir: None,
            frozen_readonly: false,
        };
        assert_eq!(
            project_readme(b"# Profile", ".profile", &bare, false, false).unwrap(),
            None
        );
        assert_eq!(
            project_readme(b"# Profile", ".profile", &target(), true, false).unwrap(),
            None
        );
    }
}
