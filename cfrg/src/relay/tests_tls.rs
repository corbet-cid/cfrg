//! TLS against a real PostgreSQL started by the test with a throwaway CA. Ignored unless
//! `CFRG_RELAY_TEST_PG_BIN` names a directory with `initdb` and `pg_ctl` (PostgreSQL 14+, run as a normal user).

use super::{pg::PgStore, pgtls, store::Store};
use crate::model::Forge;
use postgres::config::SslMode;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

struct Server {
    bin: PathBuf,
    data: PathBuf,
    port: u16,
    ca: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .args(["stop", "-m", "immediate", "-D"])
            .arg(&self.data)
            .output();
    }
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn start() -> Server {
    let bin = PathBuf::from(
        std::env::var("CFRG_RELAY_TEST_PG_BIN").expect("CFRG_RELAY_TEST_PG_BIN is set"),
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A test CA and a server certificate for localhost signed by it.
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let write = |name: &str, text: String| -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let ca_pem = write("ca.pem", ca.pem());
    let cert = write("server.crt", leaf.pem());
    let key = write("server.key", leaf_key.serialize_pem());
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let data = root.join("data");
    run(Command::new(bin.join("initdb"))
        .args(["--no-sync", "-A", "trust", "-U", "test", "-D"])
        .arg(&data));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let conf = format!(
        "\nlisten_addresses = '127.0.0.1'\nport = {port}\nunix_socket_directories = '{}'\nssl = on\n\
         ssl_cert_file = '{}'\nssl_key_file = '{}'\nfsync = off\n",
        root.display(),
        cert.display(),
        key.display()
    );
    let mut existing = std::fs::read_to_string(data.join("postgresql.conf")).unwrap();
    existing.push_str(&conf);
    std::fs::write(data.join("postgresql.conf"), existing).unwrap();
    run(Command::new(bin.join("pg_ctl"))
        .args(["start", "-w", "-s", "-D"])
        .arg(&data)
        .arg("-l")
        .arg(root.join("log")));
    Server {
        bin,
        data,
        port,
        ca: ca_pem,
        _dir: dir,
    }
}

fn config(s: &Server, mode: &str, root: Option<&Path>) -> String {
    let mut text = format!(
        "host=localhost port={} user=test dbname=postgres sslmode={mode}",
        s.port
    );
    if let Some(root) = root {
        text.push_str(&format!(" sslrootcert={}", root.display()));
    }
    text
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG_BIN"]
fn a_verified_tls_connection_to_a_server_with_a_test_ca() {
    let server = start();
    for mode in ["require", "verify-full"] {
        let text = config(&server, mode, Some(&server.ca));
        // The connection really is TLS: ask the server.
        let parsed = pgtls::parse(&text).unwrap();
        assert_eq!(parsed.mode, pgtls::Mode::Verify);
        let mut client = parsed
            .config
            .parse::<postgres::Config>()
            .unwrap()
            .ssl_mode(SslMode::Require)
            .connect(pgtls::connector(parsed.root_cert.as_deref()).unwrap())
            .unwrap();
        let ssl: bool = client
            .query_one(
                "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .unwrap()
            .get(0);
        assert!(ssl, "{mode}");
        // And the store works over it, pacing included.
        let mut store = PgStore::connect(&text).unwrap();
        assert_eq!(
            store.pace_claim(Forge::Github, mode, 100, 2, None).unwrap(),
            0
        );
    }
    // The system roots do not know the test CA: the same server is refused.
    assert!(PgStore::connect(&config(&server, "verify-full", None)).is_err());
    // Plain TCP is allowed for localhost only when asked for (or by default there).
    assert!(PgStore::connect(&config(&server, "disable", None)).is_ok());
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG_BIN"]
fn a_wrong_server_name_is_refused() {
    let server = start();
    // 127.0.0.1 is not in the certificate (only `localhost`).
    let text = format!(
        "host=127.0.0.1 port={} user=test dbname=postgres sslmode=verify-full sslrootcert={}",
        server.port,
        server.ca.display()
    );
    assert!(PgStore::connect(&text).is_err());
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG_BIN"]
fn concurrent_writers_in_the_database_are_spaced_by_the_gap() {
    let server = start();
    let text = config(&server, "verify-full", Some(&server.ca));
    // Eight jobs claim at the same second: the waits are 0, 2, 4, ... and every creation is 12 s apart.
    let mut waits: Vec<u64> = (0..8)
        .map(|_| {
            let text = text.clone();
            std::thread::spawn(move || {
                PgStore::connect(&text)
                    .unwrap()
                    .pace_claim(Forge::Forgejo, "org", 1_000, 2, None)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    waits.sort();
    assert_eq!(waits, [0, 2, 4, 6, 8, 10, 12, 14]);
    let mut store = PgStore::connect(&text).unwrap();
    let a = store
        .pace_claim(Forge::Gitlab, "g", 5_000, 2, Some(12))
        .unwrap();
    let b = store
        .pace_claim(Forge::Gitlab, "g", 5_000, 2, Some(12))
        .unwrap();
    let c = store
        .pace_claim(Forge::Gitlab, "g", 5_000, 2, None)
        .unwrap();
    assert_eq!((a, b, c), (0, 12, 14));
}
