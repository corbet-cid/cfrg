//! Maintainer approval is a live review bound to exact inputs, never a source review.
use super::*;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    head: String,
    base: String,
    review: u64,
    maintainer: String,
    maintainer_id: u64,
    sandbox: Sandbox,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sandbox {
    namespace: String,
    image: String,
    archive_sha256: String,
    command: Vec<String>,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl Request {
    fn validate(&self) -> Result<String> {
        let sandbox = &self.sandbox;
        if !oid(&self.head)
            || !oid(&self.base)
            || self.review == 0
            || self.maintainer_id == 0
            || !segment(&self.maintainer)
            || !sandbox.namespace.starts_with("ccid-untrusted-")
            || !segment(&sandbox.namespace)
            || sandbox.namespace.len() > 63
            || !sandbox
                .namespace
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || !digest(&sandbox.archive_sha256)
            || !sandbox
                .image
                .split_once("@sha256:")
                .is_some_and(|(name, hash)| {
                    !name.is_empty() && !name.chars().any(char::is_whitespace) && digest(hash)
                })
        {
            return Err(failure("CI approval requires exact head/base, maintainer, isolated namespace, archive and image digests"));
        }
        crate::validate_command(&sandbox.command)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(sandbox)?));
        Ok(format!(
            "ccid-ci-approve head={} base={} sandbox={hash}",
            self.head, self.base
        ))
    }
}

pub(super) fn message(bytes: &[u8]) -> Result<String> {
    serde_json::from_slice::<Request>(bytes)?.validate()
}

pub(super) fn plan(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    config: &Config,
    iid: u64,
    journal: &Journal,
    bytes: &[u8],
) -> Result<Value> {
    let request: Request = serde_json::from_slice(bytes)?;
    let approval = request.validate()?;
    let (_, entry, pull) =
        super::lifecycle::recorded_pull(gitlab, forgejo, transport, config, iid, journal)?;
    let index = pull.number;
    if pull.state != "open"
        || pull.merged != Some(false)
        || pull.head_sha != request.head
        || pull.base_sha != request.base
        || entry.sha.as_deref() != Some(request.head.as_str())
        || super::mr(gitlab, transport, config, iid)?.sha != request.head
    {
        return Err(failure(
            "CI approval is stale or does not identify the journaled contribution",
        ));
    }
    let review = forgejo.pull_review(
        transport,
        &config.forgejo,
        &config.primary_repository,
        index,
        request.review,
    )?;
    if review.id != request.review
        || review.user_id != request.maintainer_id
        || review.login != request.maintainer
        || review.state != "APPROVED"
        || review.dismissed
        || review.stale
        || review.commit_id != request.head
        || review.body != approval
    {
        return Err(failure(
            "Primary review does not approve these exact CI inputs",
        ));
    }
    let permission = forgejo.collaborator_permission(
        transport,
        &config.forgejo,
        &config.primary_repository,
        &request.maintainer,
    )?;
    if permission.user_id != request.maintainer_id
        || !matches!(permission.permission.as_str(), "admin" | "write" | "owner")
    {
        return Err(failure("Approver is not a current primary maintainer"));
    }
    let current = forgejo.pull(
        transport,
        &config.forgejo,
        &config.primary_repository,
        index,
    )?;
    if current != pull || super::mr(gitlab, transport, config, iid)?.sha != request.head {
        return Err(failure("Contribution changed during approval validation"));
    }
    let sandbox = &request.sandbox;
    let id = format!(
        "{:x}",
        Sha256::digest(format!("{}:{approval}", config.key(iid)))
    );
    let shell="set -eu\nprintf '%s  /input/source.tar\\n' \"$SOURCE_SHA256\" | sha256sum -c -\nmkdir /work/source\ntar -xf /input/source.tar -C /work/source\ncd /work/source\nexec \"$@\"";
    Ok(json!({"apiVersion":"batch/v1","kind":"Job",
    "metadata":{"name":format!("ccid-{}",&id[..32]),"namespace":sandbox.namespace,"annotations":{"ccid/head":request.head,"ccid/base":request.base,"ccid/review":request.review.to_string(),"ccid/approval":approval}},
    "spec":{"backoffLimit":0,"activeDeadlineSeconds":900,"template":{"metadata":{"labels":{"app":"ccid-untrusted"}},"spec":{
        "serviceAccountName":"ccid-untrusted","automountServiceAccountToken":false,"enableServiceLinks":false,"restartPolicy":"Never",
        "securityContext":{"runAsNonRoot":true,"runAsUser":65532,"runAsGroup":65532,"fsGroup":65532,"seccompProfile":{"type":"RuntimeDefault"}},
        "containers":[{"name":"check","image":sandbox.image,"command":["/bin/sh","-c",shell,"ccid-sandbox"],"args":sandbox.command,
            "env":[{"name":"SOURCE_SHA256","value":sandbox.archive_sha256},{"name":"HOME","value":"/work"},{"name":"TMPDIR","value":"/work"}],
            "resources":{"requests":{"cpu":"100m","memory":"128Mi"},"limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"4Gi"}},
            "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},
            "volumeMounts":[{"name":"input","mountPath":"/input","readOnly":true},{"name":"work","mountPath":"/work"}]}],
        "volumes":[{"name":"input","configMap":{"name":format!("ccid-source-{}",&sandbox.archive_sha256[..32])}},{"name":"work","emptyDir":{"sizeLimit":"4Gi"}}]
    }}}}))
}
