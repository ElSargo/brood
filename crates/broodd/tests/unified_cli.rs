use anyhow::ensure;
use std::{os::unix::fs::symlink, process::Command};

#[test]
fn modes_and_legacy_names_reach_the_same_parsers() -> anyhow::Result<()> {
    let binary = env!("CARGO_BIN_EXE_brood");
    let directory = tempfile::tempdir()?;
    for (role, alias) in brood_launch::ROLES {
        if *role == "runner-exec" || *role == "runner-nix-develop" {
            continue;
        }
        let direct = Command::new(binary).args([role, "--help"]).output()?;
        let alias_path = directory.path().join(alias);
        symlink(binary, &alias_path)?;
        let legacy = Command::new(alias_path).arg("--help").output()?;
        ensure!(direct.status.success() == legacy.status.success(), "{role}");
        if *role == "client" && !cfg!(feature = "presentation") {
            ensure!(!direct.status.success());
            ensure!(
                String::from_utf8_lossy(&direct.stderr).contains("does not include presentation")
            );
        } else {
            ensure!(direct.status.success(), "{role}: {direct:?}");
            ensure!(!direct.stdout.is_empty(), "{role}");
        }
    }
    Ok(())
}

#[test]
fn capability_report_matches_the_selected_build() -> anyhow::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_brood"))
        .arg("capabilities")
        .output()?;
    ensure!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report
            .get("presentation")
            .and_then(serde_json::Value::as_bool)
            == Some(cfg!(feature = "presentation"))
    );
    ensure!(report.get("server").and_then(serde_json::Value::as_bool) == Some(true));
    Ok(())
}
