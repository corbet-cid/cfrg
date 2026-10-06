#![forbid(unsafe_code)]

fn main() {
    println!("cargo:rerun-if-env-changed=CFRG_SOURCE_REVISION");
    println!("cargo:rerun-if-env-changed=CI_COMMIT_SHA");
    // Crow jobs always set the verified CI_COMMIT_SHA; developers building
    // locally get "unversioned" unless they export CFRG_SOURCE_REVISION.
    let revision = std::env::var("CFRG_SOURCE_REVISION")
        .or_else(|_| std::env::var("CI_COMMIT_SHA"))
        .unwrap_or_else(|_| "unversioned".into());
    assert!(
        revision == "unversioned"
            || revision.len() == 40
                && revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "CFRG_SOURCE_REVISION must be a full lowercase Git commit"
    );
    println!("cargo:rustc-env=CFRG_SOURCE_REVISION={revision}");
}
