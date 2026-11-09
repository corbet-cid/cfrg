//! Release: publish a build artifact for one exact commit on the primary forge
//! so consumers fetch it by URL and hash (Nix `fetchurl`) instead of a manual
//! install on a host.
//!
//! The forge-neutral procedure is here; an adapter supplies the native
//! registry's paths ([`ReleaseTarget`]). Published bytes are immutable per
//! (package, version, file): publishing the same bytes again is a no-op that
//! reports the same URL, publishing different bytes under an existing version
//! is an error, never an overwrite.
use crate::{
    failure,
    land::commit_id,
    model::Forge,
    native::{
        http::{expect, Auth, Request, Transport},
        Endpoint,
    },
    process::Runner,
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

/// Declared per repository next to its CI manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub forge: Forge,
    pub endpoint: Endpoint,
    /// User or organisation that owns the registry namespace.
    pub owner: String,
    pub package: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Unsupported release config schema"));
        }
        self.endpoint.validate()?;
        crate::native::component(&self.owner)?;
        name(&self.package)
    }
}

/// One immutable published file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub owner: String,
    pub package: String,
    /// The exact commit the bytes were built from.
    pub version: String,
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

pub enum Upload {
    Created,
    /// The file already exists; immutable, so it must be compared, not replaced.
    Exists,
}

/// The forge side of release. Implemented once per adapter.
pub trait ReleaseTarget {
    /// Path (below the origin) the bytes are PUT to; also the stable download path.
    fn file_path(&self, artifact: &Artifact) -> String;
    /// Path of the JSON listing that carries the stored hash of each file.
    fn listing_path(&self, artifact: &Artifact) -> String;
    fn auth(&self) -> Auth;
    /// SHA-256 the forge computed for `artifact.file`, if the listing has it.
    fn stored_sha256(&self, listing: &Value, artifact: &Artifact) -> Option<String>;
}

/// Sends the bytes. A separate seam so the procedure is testable offline.
pub trait Uploader {
    fn put(&mut self, endpoint: &Endpoint, auth: Auth, path: &str, file: &Path) -> Result<Upload>;
}

pub fn name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with(['.', '-'])
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        return Err(failure("Invalid package, version or file name"));
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((digest, size))
}

/// The program interpreter (`PT_INTERP`) of a Linux ELF64 executable, `None` for a static
/// binary and for every non-ELF file. Other ELF classes are not judged: the release targets
/// are x86_64/arm64 Linux (ELF64), macOS (Mach-O) and Windows (PE). A malformed program
/// header table is an error rather than a silent pass.
pub fn elf_interpreter(path: &Path) -> Result<Option<String>> {
    let mut file = File::open(path)?;
    let mut ident = [0u8; 16];
    if file.read_exact(&mut ident).is_err() || ident[..4] != b"\x7fELF"[..] {
        return Ok(None);
    }
    if ident[4] != 2 || ident[5] != 1 {
        return Err(failure(
            "release: only little-endian ELF64 binaries can be checked against the CI build environment",
        ));
    }
    let mut header = [0u8; 64];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    let phoff = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let phentsize = u16::from_le_bytes(header[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(header[56..58].try_into().unwrap()) as usize;
    if phnum == 0 {
        return Ok(None);
    }
    if phentsize < 56 {
        return Err(failure("release: malformed ELF program header table"));
    }
    let mut header_bytes = vec![0u8; phentsize];
    for index in 0..phnum {
        let at = (index as u64)
            .checked_mul(phentsize as u64)
            .and_then(|offset| phoff.checked_add(offset))
            .ok_or_else(|| failure("release: malformed ELF program header offset"))?;
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut header_bytes)?;
        if u32::from_le_bytes(header_bytes[0..4].try_into().unwrap()) != 3 {
            continue;
        }
        let offset = u64::from_le_bytes(header_bytes[8..16].try_into().unwrap());
        let size = u64::from_le_bytes(header_bytes[32..40].try_into().unwrap());
        if size == 0 || size > 4096 {
            return Err(failure("release: malformed ELF interpreter path"));
        }
        let mut interpreter = vec![0u8; size as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut interpreter)?;
        let end = interpreter
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(interpreter.len());
        return Ok(Some(
            String::from_utf8_lossy(&interpreter[..end]).into_owned(),
        ));
    }
    Ok(None)
}

/// The build-environment gate of a release: a Linux ELF must name a Nix store program
/// interpreter. The CI job image and every Nix check-shell build with a Nix loader; a binary
/// built on a plain host links that host's loader (`/lib64/...`) and may require a newer glibc
/// than the CI image runs (2026-11-08: such a release stopped every heavy-box job). Static
/// binaries carry no loader dependency, and non-ELF files are not judged here.
pub fn require_ci_build(path: &Path) -> Result<()> {
    if let Some(interpreter) = elf_interpreter(path)? {
        if !interpreter.starts_with("/nix/store/") {
            return Err(failure(format!(
                "{} was built outside the CI image or a Nix check-shell: it links {interpreter}. \
                 Build the release in the CI environment and publish that file",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Print the record `publish` would create, without uploading anything. The build-environment
/// gate runs here too, so a release dry-run proves the binary would be accepted.
pub fn plan(
    target: &dyn ReleaseTarget,
    config: &Config,
    version: &str,
    file_name: &str,
    file: &Path,
) -> Result<Value> {
    commit_id(version)?;
    name(file_name)?;
    require_ci_build(file)?;
    let (sha256, size) = sha256_file(file)?;
    let artifact = Artifact {
        owner: config.owner.clone(),
        package: config.package.clone(),
        version: version.into(),
        file: file_name.into(),
        sha256,
        size,
    };
    let mut record = record(target, config, &artifact, false);
    record["created"] = Value::Null;
    record["dry_run"] = json!(true);
    Ok(record)
}

/// Publish `file` as `config.package` at `version` (a full commit id). Returns
/// the consumer-facing record: URL, hash and size.
pub fn publish(
    target: &dyn ReleaseTarget,
    io: &mut dyn Transport,
    uploader: &mut dyn Uploader,
    config: &Config,
    version: &str,
    file_name: &str,
    file: &Path,
) -> Result<Value> {
    commit_id(version)?;
    name(file_name)?;
    require_ci_build(file)?;
    let (sha256, size) = sha256_file(file)?;
    let artifact = Artifact {
        owner: config.owner.clone(),
        package: config.package.clone(),
        version: version.into(),
        file: file_name.into(),
        sha256,
        size,
    };
    let created = matches!(
        uploader.put(
            &config.endpoint,
            target.auth(),
            &target.file_path(&artifact),
            file
        )?,
        Upload::Created
    );
    // Whether new or pre-existing, what the forge stores must be these bytes.
    let stored = stored_hash(target, io, config, &artifact)?;
    if stored.as_deref() != Some(artifact.sha256.as_str()) {
        return Err(failure(if created {
            "The forge stored different bytes than were uploaded"
        } else {
            "An immutable release with different bytes already exists for this version"
        }));
    }
    Ok(record(target, config, &artifact, created))
}

/// What the forge holds for an exact version, without uploading anything.
pub fn show(
    target: &dyn ReleaseTarget,
    io: &mut dyn Transport,
    config: &Config,
    version: &str,
    file_name: &str,
) -> Result<Value> {
    commit_id(version)?;
    name(file_name)?;
    let mut artifact = Artifact {
        owner: config.owner.clone(),
        package: config.package.clone(),
        version: version.into(),
        file: file_name.into(),
        sha256: String::new(),
        size: 0,
    };
    let stored = stored_hash(target, io, config, &artifact)?
        .ok_or_else(|| failure("No such release file on the forge"))?;
    artifact.sha256 = stored;
    Ok(record(target, config, &artifact, false))
}

fn stored_hash(
    target: &dyn ReleaseTarget,
    io: &mut dyn Transport,
    config: &Config,
    artifact: &Artifact,
) -> Result<Option<String>> {
    let response = io.send(Request {
        endpoint: config.endpoint.clone(),
        auth: target.auth(),
        method: "GET",
        path: target.listing_path(artifact),
        body: None,
        scope: format!("{}/api", config.endpoint.origin),
        creation: false,
    })?;
    if response.status == 404 {
        return Ok(None);
    }
    let listing = expect(response, &[200])?;
    Ok(target.stored_sha256(&listing, artifact))
}

fn record(
    target: &dyn ReleaseTarget,
    config: &Config,
    artifact: &Artifact,
    created: bool,
) -> Value {
    json!({
        "url": format!("{}{}", config.endpoint.origin, target.file_path(artifact)),
        "sha256": artifact.sha256,
        "size": artifact.size,
        "commit": artifact.version,
        "package": artifact.package,
        "file": artifact.file,
        "created": created,
    })
}

/// Uploads with curl: credential expanded inside curl, no redirects, no
/// ambient configuration, no response body kept.
pub struct Curl;

impl Uploader for Curl {
    fn put(&mut self, endpoint: &Endpoint, auth: Auth, path: &str, file: &Path) -> Result<Upload> {
        endpoint.validate()?;
        crate::bridge::http::reject_path_tricks(path)?;
        let mut env = crate::bridge::http::environment();
        env.insert(
            "CFRG_NATIVE_TOKEN".into(),
            crate::bridge::http::token(&endpoint.token_env)?.into(),
        );
        let header = match auth {
            Auth::Token => "Authorization: token {{CFRG_NATIVE_TOKEN}}",
            Auth::PrivateToken => "PRIVATE-TOKEN: {{CFRG_NATIVE_TOKEN}}",
            Auth::Bearer => "Authorization: Bearer {{CFRG_NATIVE_TOKEN}}",
        };
        let output = tempfile::NamedTempFile::new()?;
        let runner = Runner::new(std::env::current_dir()?, env, Duration::from_secs(900))?
            .with_stderr_events()
            .without_child_stderr();
        let mut args: Vec<String> = [
            "curl",
            "--disable",
            "--silent",
            "--globoff",
            "--connect-timeout",
            "10",
            "--max-time",
            "850",
            "--proto",
            "=https",
            "--variable",
            "%CFRG_NATIVE_TOKEN",
            "--expand-header",
            header,
            "--output",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.push(output.path().to_string_lossy().into_owned());
        args.extend([
            "--write-out".into(),
            "%{http_code}".into(),
            "--upload-file".into(),
            file.to_string_lossy().into_owned(),
            "--url".into(),
            format!("{}{}", endpoint.origin, path),
        ]);
        let status: u16 = runner
            .run(&args, true)?
            .trim()
            .parse()
            .map_err(|_| failure("Unknown HTTP outcome"))?;
        match status {
            200 | 201 => Ok(Upload::Created),
            409 => Ok(Upload::Exists),
            other => Err(failure(format!("Release upload failed with HTTP {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::http::Response;
    use std::io::Write;

    struct Target;
    impl ReleaseTarget for Target {
        fn file_path(&self, a: &Artifact) -> String {
            format!("/p/{}/{}/{}/{}", a.owner, a.package, a.version, a.file)
        }
        fn listing_path(&self, a: &Artifact) -> String {
            format!("/l/{}/{}/{}", a.owner, a.package, a.version)
        }
        fn auth(&self) -> Auth {
            Auth::Token
        }
        fn stored_sha256(&self, listing: &Value, a: &Artifact) -> Option<String> {
            listing
                .as_array()?
                .iter()
                .find(|f| f["name"] == a.file.as_str())
                .and_then(|f| f["sha256"].as_str().map(String::from))
        }
    }

    struct Io {
        stored: Option<String>,
    }
    impl Transport for Io {
        fn send(&mut self, request: Request) -> Result<Response> {
            assert_eq!(request.method, "GET");
            Ok(match &self.stored {
                Some(hash) => Response {
                    status: 200,
                    body: json!([{"name":"cfrg-x","sha256":hash}]),
                },
                None => Response {
                    status: 404,
                    body: Value::Null,
                },
            })
        }
    }

    struct Put(Upload);
    impl Uploader for Put {
        fn put(&mut self, _: &Endpoint, _: Auth, path: &str, _: &Path) -> Result<Upload> {
            assert!(path.starts_with("/p/o/pkg/"));
            Ok(match self.0 {
                Upload::Created => Upload::Created,
                Upload::Exists => Upload::Exists,
            })
        }
    }

    fn config() -> Config {
        serde_json::from_value(json!({
            "schema":1,"forge":"forgejo",
            "endpoint":{"origin":"https://forge.example","token_env":"TOKEN"},
            "owner":"o","package":"pkg"
        }))
        .unwrap()
    }

    fn file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfrg-x");
        fs::File::create(&path).unwrap().write_all(bytes).unwrap();
        let (hash, _) = sha256_file(&path).unwrap();
        (dir, path, hash)
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn new_upload_is_verified_against_the_stored_hash() {
        let (_dir, path, hash) = file(b"binary");
        let mut io = Io {
            stored: Some(hash.clone()),
        };
        let out = publish(
            &Target,
            &mut io,
            &mut Put(Upload::Created),
            &config(),
            COMMIT,
            "cfrg-x",
            &path,
        )
        .unwrap();
        assert_eq!(out["sha256"], hash.as_str());
        assert_eq!(out["created"], true);
        assert_eq!(
            out["url"],
            format!("https://forge.example/p/o/pkg/{COMMIT}/cfrg-x")
        );
        assert_eq!(out["size"], 6);
    }

    /// A minimal little-endian ELF64 header with an optional PT_INTERP.
    fn elf(interp: Option<&str>) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        match interp {
            Some(text) => {
                bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
                bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
                bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
                let mut header = vec![0u8; 56];
                header[0..4].copy_from_slice(&3u32.to_le_bytes());
                header[8..16].copy_from_slice(&(64u64 + 56).to_le_bytes());
                header[32..40].copy_from_slice(&((text.len() + 1) as u64).to_le_bytes());
                bytes.extend_from_slice(&header);
                bytes.extend_from_slice(text.as_bytes());
                bytes.push(0);
            }
            None => {
                bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn releases_must_come_from_the_ci_build_environment() {
        let dir = tempfile::tempdir().unwrap();
        let ci = dir.path().join("ci");
        let interpreter = "/nix/store/abc-glibc-2.42/lib/ld-linux-x86-64.so.2";
        fs::write(&ci, elf(Some(interpreter))).unwrap();
        assert_eq!(elf_interpreter(&ci).unwrap().as_deref(), Some(interpreter));
        require_ci_build(&ci).unwrap();

        let host = dir.path().join("host");
        fs::write(&host, elf(Some("/lib64/ld-linux-x86-64.so.2"))).unwrap();
        let err = require_ci_build(&host).unwrap_err().to_string();
        assert!(err.contains("built outside the CI image"), "{err}");

        let static_binary = dir.path().join("static");
        fs::write(&static_binary, elf(None)).unwrap();
        require_ci_build(&static_binary).unwrap();

        let script = dir.path().join("script");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        require_ci_build(&script).unwrap();

        let mut elf32_bytes = elf(Some("/lib/ld-linux.so.2"));
        elf32_bytes[4] = 1;
        let elf32 = dir.path().join("elf32");
        fs::write(&elf32, elf32_bytes).unwrap();
        assert!(require_ci_build(&elf32).is_err());
    }

    #[test]
    fn a_dry_run_reports_the_record_without_uploading() {
        let (_dir, path, hash) = file(b"binary");
        let out = plan(&Target, &config(), COMMIT, "pkg-x", &path).unwrap();
        assert_eq!(out["sha256"], hash.as_str());
        assert_eq!(out["dry_run"], true);
        assert!(out["created"].is_null());
        assert_eq!(out["size"], 6);
    }

    #[test]
    fn republishing_identical_bytes_is_a_noop_and_different_bytes_fail() {
        let (_dir, path, hash) = file(b"binary");
        let mut same = Io { stored: Some(hash) };
        let out = publish(
            &Target,
            &mut same,
            &mut Put(Upload::Exists),
            &config(),
            COMMIT,
            "cfrg-x",
            &path,
        )
        .unwrap();
        assert_eq!(out["created"], false);
        let mut other = Io {
            stored: Some("0".repeat(64)),
        };
        let err = publish(
            &Target,
            &mut other,
            &mut Put(Upload::Exists),
            &config(),
            COMMIT,
            "cfrg-x",
            &path,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("different bytes"));
        let mut missing = Io { stored: None };
        assert!(publish(
            &Target,
            &mut missing,
            &mut Put(Upload::Created),
            &config(),
            COMMIT,
            "cfrg-x",
            &path
        )
        .is_err());
    }

    #[test]
    fn show_reads_the_stored_record_and_names_are_validated() {
        let mut io = Io {
            stored: Some("ab".repeat(32)),
        };
        let out = show(&Target, &mut io, &config(), COMMIT, "cfrg-x").unwrap();
        assert_eq!(out["sha256"], "ab".repeat(32).as_str());
        assert!(show(
            &Target,
            &mut Io { stored: None },
            &config(),
            COMMIT,
            "cfrg-x"
        )
        .is_err());
        for bad in ["", ".x", "-x", "a/b", "a b", "a;b"] {
            assert!(name(bad).is_err(), "{bad}");
        }
        assert!(show(&Target, &mut io, &config(), "main", "cfrg-x").is_err());
    }
}
