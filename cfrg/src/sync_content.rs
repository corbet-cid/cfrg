//! Ref promotion never claims to have copied external source payloads.
use super::git::{valid_oid, Git};
use crate::{failure, Result};
use serde::Serialize;
use std::io::Write;

#[derive(Debug, Default, Serialize)]
pub struct Content {
    pub inspected: bool,
    pub lfs_required: bool,
    pub submodules_required: bool,
}

impl Content {
    pub fn verified(&self) -> bool {
        self.inspected && !self.lfs_required && !self.submodules_required
    }
}

pub(super) fn inspect(git: &Git, oid_bytes: usize) -> Result<Content> {
    // This object database contains only the complete fetched source closure.
    // Smudge decodes the first 1024 bytes, even for larger noncanonical blobs.
    // Trees carry gitlinks even when .gitmodules is absent or was later removed.
    let inventory = git.run(&[
        "cat-file",
        "--batch-all-objects",
        "--batch-check=%(objectname) %(objecttype) %(objectsize)",
    ])?;
    let mut objects = Vec::new();
    let mut content = Content::default();
    for line in inventory.lines() {
        let fields: Vec<_> = line.split(' ').collect();
        let [oid, kind, size] = fields.as_slice() else {
            return Err(failure("Malformed object inventory"));
        };
        let size = size.parse::<usize>()?;
        if !valid_oid(oid) {
            return Err(failure("Malformed object identity"));
        }
        if *kind == "blob" && size > 8 * 1024 * 1024 {
            let prefix = git.runner.run_prefix(
                &git.argv(&["cat-file".into(), "blob".into(), (*oid).into()]),
                1024,
            )?;
            content.lfs_required |= lfs_pointer(&prefix);
        } else if *kind == "tree" || *kind == "blob" {
            if size > 8 * 1024 * 1024 {
                return Err(failure("Source tree exceeds bounded inspection size"));
            }
            objects.push(((*oid).to_owned(), (*kind).to_owned(), size));
        }
    }
    let mut offset = 0;
    while offset < objects.len() {
        let start = offset;
        let mut bytes = 0;
        while offset < objects.len() && bytes + objects[offset].2 + 100 <= 12 * 1024 * 1024 {
            bytes += objects[offset].2 + 100;
            offset += 1;
        }
        let mut input = tempfile::NamedTempFile::new_in(&git.runner.root)?;
        for (oid, _, _) in &objects[start..offset] {
            writeln!(input, "{oid}")?;
        }
        input.flush()?;
        let output = git.runner.run_bytes_with_input_file(
            &git.argv(&["cat-file".into(), "--batch".into()]),
            input.path(),
        )?;
        let mut tail = output.as_slice();
        for (oid, kind, size) in &objects[start..offset] {
            let end = tail
                .iter()
                .position(|c| *c == b'\n')
                .ok_or_else(|| failure("Truncated object header"))?;
            if &tail[..end] != format!("{oid} {kind} {size}").as_bytes() {
                return Err(failure("Object header mismatch"));
            }
            tail = &tail[end + 1..];
            if tail.len() <= *size || tail[*size] != b'\n' {
                return Err(failure("Truncated object payload"));
            }
            let body = &tail[..*size];
            if kind == "blob" {
                // Accept historical version URLs too; an unverified pointer
                // must never be mistaken for the payload it names.
                // git-lfs accepts noncanonical pointers after Unicode whitespace
                // trimming. Missing those would claim payloads were replicated.
                content.lfs_required |= lfs_pointer(&body[..body.len().min(1024)]);
            } else {
                content.submodules_required |= gitlinks(body, oid_bytes)?;
            }
            tail = &tail[*size + 1..];
        }
        if !tail.is_empty() {
            return Err(failure("Unexpected batch object output"));
        }
    }
    content.inspected = true;
    Ok(content)
}

fn lfs_pointer(body: &[u8]) -> bool {
    String::from_utf8_lossy(body)
        .trim_start()
        .starts_with("version ")
        && body
            .windows(b"oid sha256:".len())
            .any(|part| part == b"oid sha256:")
}

fn gitlinks(mut tree: &[u8], oid_bytes: usize) -> Result<bool> {
    let mut found = false;
    while !tree.is_empty() {
        let space = tree
            .iter()
            .position(|c| *c == b' ')
            .ok_or_else(|| failure("Invalid Git tree mode"))?;
        let nul = tree
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| failure("Invalid Git tree path"))?;
        if nul <= space || tree.len() < nul + 1 + oid_bytes {
            return Err(failure("Truncated Git tree"));
        }
        found |= &tree[..space] == b"160000";
        tree = &tree[nul + 1 + oid_bytes..];
    }
    Ok(found)
}
