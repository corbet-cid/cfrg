use super::*;
use crate::process::Runner;

pub(super) trait Kubernetes {
    fn get(&mut self, namespace: &str, resource: &str, name: Option<&str>) -> Result<Value>;
    fn create(&mut self, job: &Value) -> Result<Value>;
}

pub(super) struct Client {
    runner: Runner,
    command: String,
}
impl Client {
    pub fn new(command: PathBuf) -> Result<Self> {
        if !command.is_absolute() || !command.is_file() {
            return Err(failure(
                "Kubernetes wrapper must be an existing absolute executable",
            ));
        }
        let mut environment = http::environment();
        if let Some(value) = std::env::var_os("KUBECONFIG") {
            environment.insert("KUBECONFIG".into(), value);
        }
        Ok(Self {
            runner: Runner::new(
                std::env::current_dir()?,
                environment,
                Duration::from_secs(1020),
            )?
            .with_stderr_events()
            .without_child_stderr(),
            command: command.display().to_string(),
        })
    }
}
impl Kubernetes for Client {
    fn get(&mut self, namespace: &str, resource: &str, name: Option<&str>) -> Result<Value> {
        let mut args = vec![self.command.clone(), "get".into(), resource.into()];
        if let Some(name) = name {
            args.push(name.into());
        }
        if !namespace.is_empty() {
            args.extend(["--namespace".into(), namespace.into()]);
        }
        args.extend(["--ignore-not-found".into(), "--output=json".into()]);
        args.push("--request-timeout=30s".into());
        let data = self.runner.run(&args, true)?;
        if data.trim().is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_str(&data)?)
        }
    }
    fn create(&mut self, job: &Value) -> Result<Value> {
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(&mut file, job)?;
        file.flush()?;
        let output = self.runner.run(
            &[
                self.command.clone(),
                "create".into(),
                "--filename".into(),
                file.path().display().to_string(),
                "--output=json".into(),
            ],
            true,
        )?;
        Ok(serde_json::from_str(&output)?)
    }
}

fn empty_inventory(value: &Value) -> bool {
    value["items"].as_array().is_some_and(Vec::is_empty)
}

pub(super) fn audit(kube: &mut impl Kubernetes, plan: &Value) -> Result<()> {
    let ns = plan["metadata"]["namespace"]
        .as_str()
        .ok_or_else(|| failure("Missing isolated namespace"))?;
    let namespace = kube.get("", "namespace", Some(ns))?;
    if namespace["metadata"]["name"] != ns
        || namespace["metadata"]["labels"]["pod-security.kubernetes.io/enforce"] != "restricted"
    {
        return Err(failure("Isolated namespace lacks restricted admission"));
    }
    let policies = kube.get(ns, "networkpolicies", None)?;
    let items = policies["items"]
        .as_array()
        .ok_or_else(|| failure("Missing network policy inventory"))?;
    let empty_rules =
        |value: &Value| value.is_null() || value.as_array().is_some_and(Vec::is_empty);
    if items.len() != 1
        || items[0]["spec"]["podSelector"] != json!({})
        || items[0]["spec"]["policyTypes"] != json!(["Ingress", "Egress"])
        || !empty_rules(&items[0]["spec"]["ingress"])
        || !empty_rules(&items[0]["spec"]["egress"])
    {
        return Err(failure(
            "Isolated namespace must deny all ingress and egress without additional allow policies",
        ));
    }
    for resource in ["secrets", "rolebindings", "persistentvolumeclaims"] {
        if !empty_inventory(&kube.get(ns, resource, None)?) {
            return Err(failure(
                "Isolated namespace contains credentials, grants or persistent storage",
            ));
        }
    }
    let account = kube.get(ns, "serviceaccount", Some("ccid-untrusted"))?;
    if account["automountServiceAccountToken"] != false
        || account
            .get("secrets")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        || account
            .get("imagePullSecrets")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err(failure(
            "Untrusted service account has credentials or token automount",
        ));
    }
    let quota = kube.get(ns, "resourcequota", Some("ccid-untrusted"))?;
    for (key, value) in [
        ("pods", "1"),
        ("secrets", "0"),
        ("persistentvolumeclaims", "0"),
    ] {
        if quota["spec"]["hard"][key] != value {
            return Err(failure("Untrusted resource quota is missing or weakened"));
        }
    }
    let name = plan["spec"]["template"]["spec"]["volumes"][0]["configMap"]["name"]
        .as_str()
        .ok_or_else(|| failure("Missing source snapshot"))?;
    let source = kube.get(ns, "configmap", Some(name))?;
    if source["immutable"] != true || !source["binaryData"]["source.tar"].is_string() {
        return Err(failure(
            "Source ConfigMap must be immutable and carry the staged archive",
        ));
    }
    Ok(())
}

pub(super) fn dispatch(
    kube: &mut impl Kubernetes,
    journal: &Journal,
    plan: &Value,
) -> Result<Value> {
    let ns = plan["metadata"]["namespace"].as_str().unwrap();
    let name = plan["metadata"]["name"].as_str().unwrap();
    let key = format!("dispatch-{:x}", Sha256::digest(serde_json::to_vec(plan)?));
    let existing = kube.get(ns, "job", Some(name))?;
    if !existing.is_null() {
        if existing["metadata"]["annotations"] != plan["metadata"]["annotations"] {
            return Err(failure("Existing Job does not match approved contribution"));
        }
        return Ok(
            json!({"status":"attached","namespace":ns,"job":name,"uid":existing["metadata"]["uid"]}),
        );
    }
    if journal.root.join(format!("{key}.json")).exists() {
        return Err(failure(
            "Unresolved CI creation intent; no duplicate Job submission",
        ));
    }
    journal.save(&key, plan)?;
    let created = kube.create(plan)?;
    if created["metadata"]["name"] != name
        || created["metadata"]["namespace"] != ns
        || !created["metadata"]["uid"].is_string()
    {
        return Err(failure("Created CI Job identity not confirmed"));
    }
    Ok(json!({"status":"submitted","namespace":ns,"job":name,"uid":created["metadata"]["uid"]}))
}

fn completion(job: &Value) -> Result<bool> {
    let conditions = job["status"]["conditions"].as_array();
    if conditions.is_some_and(|items| {
        items
            .iter()
            .any(|c| c["type"] == "Failed" && c["status"] == "True")
    }) {
        return Err(failure("Isolated contribution check failed"));
    }
    Ok(conditions.is_some_and(|items| {
        items
            .iter()
            .any(|c| c["type"] == "Complete" && c["status"] == "True")
    }))
}

pub(super) fn wait(kube: &mut impl Kubernetes, plan: &Value, mut report: Value) -> Result<Value> {
    let namespace = plan["metadata"]["namespace"].as_str().unwrap();
    let name = plan["metadata"]["name"].as_str().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(920);
    loop {
        let job = kube.get(namespace, "job", Some(name))?;
        if job["metadata"]["uid"] != report["uid"] || job.is_null() {
            return Err(failure(
                "Isolated Job disappeared or changed identity while running",
            ));
        }
        if completion(&job)? {
            report["status"] = json!("success");
            return Ok(report);
        }
        if std::time::Instant::now() >= deadline {
            return Err(failure(
                "Isolated Job completion remains unknown; no passing result",
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn submission_is_not_a_passing_check() {
        assert!(!completion(&json!({"status":{"active":1}})).unwrap());
        assert!(
            completion(&json!({"status":{"conditions":[{"type":"Failed","status":"True"}]}}))
                .is_err()
        );
        assert!(completion(
            &json!({"status":{"conditions":[{"type":"Complete","status":"True"}]}})
        )
        .unwrap());
    }
    struct Fake {
        objects: std::collections::BTreeMap<String, Value>,
        creates: usize,
        lose: bool,
    }
    impl Kubernetes for Fake {
        fn get(&mut self, _: &str, resource: &str, _: Option<&str>) -> Result<Value> {
            Ok(self.objects.get(resource).cloned().unwrap_or(Value::Null))
        }
        fn create(&mut self, job: &Value) -> Result<Value> {
            self.creates += 1;
            let mut created = job.clone();
            created["metadata"]["uid"] = json!("fixture-uid");
            self.objects.insert("job".into(), created.clone());
            if self.lose {
                Err(failure("Lost create response"))
            } else {
                Ok(created)
            }
        }
    }
    fn fake() -> Fake {
        Fake { creates:0,lose:false,objects: [
            ("namespace",json!({"metadata":{"name":"ccid-untrusted-test","labels":{"pod-security.kubernetes.io/enforce":"restricted"}}})),
            ("networkpolicies",json!({"items":[{"spec":{"podSelector":{},"policyTypes":["Ingress","Egress"]}}]})),
            ("secrets",json!({"items":[]})),("rolebindings",json!({"items":[]})),("persistentvolumeclaims",json!({"items":[]})),
            ("serviceaccount",json!({"automountServiceAccountToken":false})),
            ("resourcequota",json!({"spec":{"hard":{"pods":"1","secrets":"0","persistentvolumeclaims":"0"}}})),
            ("configmap",json!({"immutable":true,"binaryData":{"source.tar":"fixture"}})),
        ].into_iter().map(|(k,v)|(k.into(),v)).collect() }
    }
    fn plan() -> Value {
        json!({"metadata":{"name":"fixture","namespace":"ccid-untrusted-test","annotations":{"ccid/head":"approved"}},"spec":{"template":{"spec":{"volumes":[{"configMap":{"name":"source"}}]}}}})
    }
    #[test]
    fn audit_rejects_network_exceptions_credentials_and_missing_admission() {
        audit(&mut fake(), &plan()).unwrap();
        for (resource, value) in [
            (
                "networkpolicies",
                json!({"items":[{"spec":{"podSelector":{},"policyTypes":["Ingress","Egress"],"egress":[{}]}}]}),
            ),
            ("secrets", json!({"items":[{"metadata":{"name":"token"}}]})),
            (
                "namespace",
                json!({"metadata":{"name":"ccid-untrusted-test"}}),
            ),
            (
                "serviceaccount",
                json!({"automountServiceAccountToken":true}),
            ),
            (
                "configmap",
                json!({"immutable":false,"binaryData":{"source.tar":"fixture"}}),
            ),
        ] {
            let mut kube = fake();
            kube.objects.insert(resource.into(), value);
            assert!(audit(&mut kube, &plan()).is_err(), "{resource}");
        }
    }
    #[test]
    fn lost_submission_attaches_existing_and_never_recreates_missing_job() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        let mut kube = fake();
        kube.lose = true;
        assert!(dispatch(&mut kube, &journal, &plan()).is_err());
        assert_eq!(
            dispatch(&mut kube, &journal, &plan()).unwrap()["status"],
            "attached"
        );
        kube.objects.remove("job");
        assert!(dispatch(&mut kube, &journal, &plan()).is_err());
        assert_eq!(kube.creates, 1);
    }
}
