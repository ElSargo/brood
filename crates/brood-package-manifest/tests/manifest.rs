use brood_package_manifest::{ArtifactAbi, ArtifactKind, LoadError, Manifest, SCHEMA_VERSION};
use std::fs;
use std::path::Path;
use std::process::Command;

const VALID: &str = r#"
schema-version = 1

[package]
id = "org.brood.default-client"
version = "0.1.0"
root = "session-switcher"

[[artifacts]]
id = "session-switcher"
kind = "wasm-component"
abi = "brood-component-v1"
source = "plugins/session-switcher/target/session-switcher.component.wasm"
placement = "actors/session-switcher.component.wasm"
capabilities = ["actor:mailbox", "presentation:layout"]
dependencies = ["picker"]

[[artifacts]]
id = "picker"
kind = "wasm-component"
abi = "brood-component-v1"
source = "plugins/picker/target/picker.component.wasm"
placement = "actors/picker.component.wasm"
capabilities = ["actor:mailbox"]
"#;

#[test]
fn parses_a_typed_package_and_orders_dependencies_first() {
    let manifest = Manifest::from_toml(VALID).expect("valid package manifest");

    assert_eq!(manifest.schema_version, SCHEMA_VERSION);
    assert_eq!(
        manifest.root_artifact().expect("root").id.as_str(),
        "session-switcher"
    );
    assert_eq!(
        manifest
            .installation_order()
            .expect("install order")
            .into_iter()
            .map(|artifact| artifact.id.as_str())
            .collect::<Vec<_>>(),
        ["picker", "session-switcher"]
    );
    assert_eq!(manifest.artifacts[0].kind, ArtifactKind::WasmComponent);
    assert_eq!(manifest.artifacts[0].abi, ArtifactAbi::BroodComponentV1);
}

#[test]
fn validates_the_default_client_migration_fixture() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        Manifest::from_path(crate_dir.join("../../packages/default-client.brood-package.toml"))
            .expect("default client package manifest");

    assert_eq!(manifest.package.id.as_str(), "org.brood.default-client");
    assert_eq!(
        manifest.root_artifact().expect("root").id.as_str(),
        "session-switcher"
    );
    assert_eq!(
        manifest.installation_order().expect("install order").len(),
        8
    );
}

#[test]
fn rejects_unknown_fields_during_deserialization() {
    let source = VALID.replace(
        "version = \"0.1.0\"",
        "version = \"0.1.0\"\nambient-authority = true",
    );

    let error = Manifest::from_toml(&source).expect_err("unknown field must fail closed");
    assert!(matches!(error, LoadError::Parse(_)));
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn reports_graph_identity_path_and_abi_failures_together() {
    let invalid = r#"
schema-version = 99

[package]
id = "Bad Package"
version = ""
root = "missing"

[[artifacts]]
id = "root"
kind = "wasm-component"
abi = "brood-preview1-v1"
source = "../escape.wasm"
placement = "/absolute/root.wasm"
capabilities = ["actor:mailbox", "actor:mailbox"]
dependencies = ["root", "unknown", "unknown"]

[[artifacts]]
id = "root"
kind = "wasm-core"
abi = "brood-legacy-v1"
source = "plugins/root.wasm"
placement = "/absolute/root.wasm"
"#;

    let error = Manifest::from_toml(invalid).expect_err("invalid manifest must fail");
    let LoadError::Validation(errors) = error else {
        panic!("expected validation errors");
    };
    let rendered = errors.to_string();
    for expected in [
        "unsupported schema version",
        "package.id",
        "package.version",
        "unknown artifact `missing`",
        "duplicates artifacts[0].id",
        "must be a non-empty portable relative path",
        "duplicates artifacts[0].placement",
        "incompatible with artifact kind",
        "duplicates an earlier capability",
        "must not refer to the artifact itself",
        "unknown artifact `unknown`",
        "duplicates an earlier dependency",
    ] {
        assert!(
            rendered.contains(expected),
            "missing `{expected}` in:\n{rendered}"
        );
    }
}

#[test]
fn rejects_dependency_cycles() {
    let source = VALID.replace(
        "capabilities = [\"actor:mailbox\"]\n",
        "capabilities = [\"actor:mailbox\"]\ndependencies = [\"session-switcher\"]\n",
    );

    let error = Manifest::from_toml(&source).expect_err("cycles must fail");
    assert!(error.to_string().contains("dependency cycle includes"));
}

#[test]
fn rejects_duplicate_install_placements() {
    let source = VALID.replace(
        "actors/picker.component.wasm",
        "actors/session-switcher.component.wasm",
    );

    let error = Manifest::from_toml(&source).expect_err("placements must be unique");
    assert!(
        error
            .to_string()
            .contains("duplicates artifacts[0].placement")
    );
}

#[test]
fn assembler_installs_a_dependency_ordered_package_tree() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let source_root = temporary.path().join("source");
    let output_root = temporary.path().join("package");
    fs::create_dir_all(source_root.join("plugins")).expect("source tree");
    fs::write(source_root.join("plugins/root.wasm"), b"root").expect("root artifact");
    fs::write(source_root.join("plugins/member.wasm"), b"member").expect("member artifact");
    let manifest_path = temporary.path().join("package.toml");
    let manifest = r#"
schema-version = 1

[package]
id = "org.brood.assembler-test"
version = "1.0.0"
root = "root"

[[artifacts]]
id = "root"
kind = "wasm-component"
abi = "brood-component-v1"
source = "plugins/root.wasm"
placement = "root.wasm"
dependencies = ["member"]

[[artifacts]]
id = "member"
kind = "wasm-component"
abi = "brood-component-v1"
source = "plugins/member.wasm"
placement = "lib/member.wasm"
"#;
    fs::write(&manifest_path, manifest).expect("manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_brood-package-manifest"))
        .arg("assemble")
        .arg(&manifest_path)
        .arg(&source_root)
        .arg(&output_root)
        .output()
        .expect("run assembler");

    assert!(
        output.status.success(),
        "assembler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = String::from_utf8(output.stdout).expect("UTF-8 log");
    assert!(
        log.find("installed=member").expect("member log")
            < log.find("installed=root").expect("root log")
    );
    assert_eq!(
        fs::read(output_root.join("root.wasm")).expect("root"),
        b"root"
    );
    assert_eq!(
        fs::read(output_root.join("lib/member.wasm")).expect("member"),
        b"member"
    );
    assert_eq!(
        fs::read_to_string(output_root.join("brood-package-root")).expect("root metadata"),
        "root.wasm\n"
    );
    Manifest::from_path(output_root.join("brood-package.toml"))
        .expect("installed manifest validates");
}
