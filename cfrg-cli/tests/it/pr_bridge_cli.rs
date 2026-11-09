use std::process::Command;

#[test]
fn bridge_is_inert_without_feature_flag_or_ci_confirmation() {
    let binary = env!("CARGO_BIN_EXE_cfrg");
    for flags in [
        vec![],
        vec!["--enable-gitlab-import", "--apply", "--mr", "7"],
    ] {
        let result = Command::new(binary)
            .args(["bridge"])
            .args(flags)
            .args([
                "--config",
                "/nonexistent/bridge.toml",
                "--state-dir",
                "/nonexistent/bridge-state",
            ])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("disabled") || stderr.contains("--confirm-ci-disabled"));
        assert!(!stderr.contains("No such file"));
    }
}
