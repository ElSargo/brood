use brood_package_manifest::installation::{apply, inspect, plan};
use std::{fs, path::PathBuf};

fn fixture(script: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("external");
    fs::create_dir(&source).unwrap();
    let executable = std::process::Command::new("sh")
        .args(["-c", "command -v bash"])
        .output()
        .unwrap();
    let executable = PathBuf::from(String::from_utf8(executable.stdout).unwrap().trim())
        .canonicalize()
        .unwrap();
    assert!(
        executable.starts_with("/nix/store"),
        "run this proof through nix develop"
    );
    fs::write(source.join("build.sh"), script).unwrap();
    fs::write(
        source.join("README.md"),
        "# External package\nAn offline extension example.",
    )
    .unwrap();
    fs::write(
        source.join("interface.json"),
        r#"{"type":"object","required":["version"]}"#,
    )
    .unwrap();
    let manifest = format!(
        r#"
schema-version = 1
[package]
id = "org.example.external"
version = "1.0.0"
root = "session"
[[artifacts]]
id = "session"
kind = "wasm-component"
abi = "brood-component-v1"
source = "session.wasm"
placement = "session.wasm"
[[artifacts]]
id = "presentation"
kind = "wasm-component"
abi = "brood-component-v1"
source = "presentation.wasm"
placement = "presentation.wasm"
[extension]
schema-version = 1
purpose = "paired external extension"
source-revision = "test-inputs-are-content-addressed"
state-compatibility = "stateless"
migration = "none"
[extension.build]
inputs = ["build.sh"]
executable = {executable:?}
arguments = ["build.sh"]
[[extension.documents]]
id = "example.overview"
path = "README.md"
category = "example"
[[extension.interfaces]]
id = "example.peer"
version = 1
schema = "interface.json"
multiplicity = "one"
disconnect = "pending"
[[extension.activation]]
artifact = "session"
realm = "surface-session"
scope = "per-realm"
provides = ["example.peer"]
restart = "never"
deactivation = "stop"
[[extension.activation]]
artifact = "presentation"
realm = "presentation-client"
scope = "per-realm"
requires = ["example.peer"]
restart = "never"
deactivation = "stop"
"#
    );
    let path = source.join("package.toml");
    fs::write(&path, manifest).unwrap();
    let store = root.path().join("store");
    (root, path, store)
}

const BUILD: &str = "printf '\\0asm\\015\\0\\001\\0' > session.wasm\nprintf '\\0asm\\015\\0\\001\\0' > presentation.wasm\n";

#[test]
fn resolved_inputs_survive_checkout_removal_and_repeat_converges() {
    let (_root, manifest, store) = fixture(BUILD);
    let resolved = plan(&manifest, &store).unwrap();
    fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
    let installed = apply(&resolved, &store).unwrap();
    assert_eq!(installed.artifacts.len(), 2);
    assert!(
        installed
            .activation
            .iter()
            .all(|outcome| outcome.status == "pending")
    );
    let repeated = apply(&resolved, &store).unwrap();
    assert_eq!(
        installed.artifact_tree_digest,
        repeated.artifact_tree_digest
    );
    assert_eq!(fs::read_dir(store.join("attempts")).unwrap().count(), 1);
    assert_eq!(
        inspect(&store, "org.example.external")
            .unwrap()
            .knowledge_digest,
        installed.knowledge_digest
    );
}

#[test]
fn failed_build_preserves_selection_and_phase_logs() {
    let (_root, manifest, store) = fixture(BUILD);
    let installed = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    fs::write(
        manifest.parent().unwrap().join("build.sh"),
        "echo deliberate-failure >&2\nexit 23\n",
    )
    .unwrap();
    let replacement = plan(&manifest, &store).unwrap();
    assert!(
        apply(&replacement, &store)
            .unwrap_err()
            .to_string()
            .contains("attempt preserved")
    );
    assert_eq!(
        inspect(&store, "org.example.external").unwrap().plan_digest,
        installed.plan_digest
    );
    let attempt = fs::read_dir(store.join("attempts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("failure.json").exists())
        .unwrap();
    assert!(
        fs::read_to_string(attempt.join("build.stderr"))
            .unwrap()
            .contains("deliberate-failure")
    );
    assert!(
        fs::read_to_string(attempt.join("failure.json"))
            .unwrap()
            .contains("build")
    );
}

#[test]
fn altered_plan_stale_selection_and_corrupted_artifacts_are_rejected() {
    let (_root, manifest, store) = fixture(BUILD);
    let resolved = plan(&manifest, &store).unwrap();
    let mut altered = resolved.clone();
    altered.inputs.insert("build.sh".into(), b"exit 0".to_vec());
    assert!(apply(&altered, &store).is_err());
    let installed = apply(&resolved, &store).unwrap();
    fs::write(
        manifest.parent().unwrap().join("build.sh"),
        format!("{BUILD}\n# another revision"),
    )
    .unwrap();
    let mut stale = plan(&manifest, &store).unwrap();
    stale.previous_selection = None;
    assert!(
        apply(&stale, &store)
            .unwrap_err()
            .to_string()
            .contains("conflict")
    );
    fs::write(
        store
            .join("objects")
            .join(installed.plan_digest)
            .join("session.wasm"),
        b"corrupted",
    )
    .unwrap();
    assert!(inspect(&store, "org.example.external").is_err());
    assert!(apply(&resolved, &store).is_err());
}

#[test]
fn invalid_output_never_becomes_selected() {
    let (_root, manifest, store) =
        fixture("echo invalid > session.wasm\necho invalid > presentation.wasm\n");
    assert!(apply(&plan(&manifest, &store).unwrap(), &store).is_err());
    assert!(!store.join("selected/org.example.external").exists());
}

#[test]
fn invalid_or_unavailable_schema_never_becomes_selected() {
    for schema in [
        r#"{"type":"not-a-type"}"#,
        r#"{"$ref":"https://example.invalid/remote-schema"}"#,
    ] {
        let (_root, manifest, store) = fixture(BUILD);
        fs::write(manifest.parent().unwrap().join("interface.json"), schema).unwrap();
        assert!(apply(&plan(&manifest, &store).unwrap(), &store).is_err());
        assert!(!store.join("selected/org.example.external").exists());
    }
}

#[test]
fn disable_enable_and_remove_keep_historical_knowledge() {
    use brood_package_manifest::installation::{
        SelectionAction, change_selection, knowledge_revision,
    };
    use brood_package_manifest::{catalogue, extension::Realm};
    let (_root, manifest, store) = fixture(BUILD);
    let installed = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    let enabled_revision = catalogue::selection_revision(&store).unwrap();
    change_selection(&store, "org.example.external", SelectionAction::Disable).unwrap();
    assert_ne!(
        enabled_revision,
        catalogue::selection_revision(&store).unwrap()
    );
    assert!(
        catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "1")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        change_selection(&store, "org.example.external", SelectionAction::Disable).unwrap()["changed"],
        false
    );
    change_selection(&store, "org.example.external", SelectionAction::Enable).unwrap();
    assert_eq!(
        catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "1")
            .unwrap()
            .len(),
        1
    );
    change_selection(&store, "org.example.external", SelectionAction::Remove).unwrap();
    assert!(catalogue::load(&store).unwrap().packages.is_empty());
    assert_eq!(
        change_selection(&store, "org.example.external", SelectionAction::Remove).unwrap()["changed"],
        false
    );
    let knowledge =
        knowledge_revision(&store, "org.example.external", &installed.plan_digest).unwrap();
    assert_eq!(knowledge["plan_digest"], installed.plan_digest);
    assert!(change_selection(&store, "../escape", SelectionAction::Remove).is_err());
}

#[test]
fn distributed_bundle_preserves_identity_and_destination_authority() {
    use brood_package_manifest::{
        catalogue,
        extension::Realm,
        installation::{DestinationAuthorization, export_bundle, import_bundle},
    };
    let (root, manifest, source_store) = fixture(BUILD);
    let installed = apply(&plan(&manifest, &source_store).unwrap(), &source_store).unwrap();
    let bundle = root.path().join("bundle");
    export_bundle(&source_store, "org.example.external", None, &bundle).unwrap();
    let destination = root.path().join("destination");
    let authorization = DestinationAuthorization {
        realms: vec![Realm::PresentationClient],
        capabilities: vec![],
        source: "test destination allowlist".into(),
    };
    assert!(
        import_bundle(
            &destination,
            "another.package",
            &bundle,
            authorization.clone()
        )
        .is_err()
    );
    assert!(!destination.join("selected/org.example.external").exists());
    let receipt = import_bundle(
        &destination,
        "org.example.external",
        &bundle,
        authorization.clone(),
    )
    .unwrap();
    assert_eq!(receipt["installation"], installed.plan_digest);
    assert_eq!(receipt["activation_status"], "pending-root-acknowledgement");
    let selected = catalogue::load(&destination).unwrap();
    assert!(
        selected
            .desired(Realm::SurfaceSession, "1")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        selected
            .desired(Realm::PresentationClient, "client")
            .unwrap()
            .len(),
        1
    );
    import_bundle(&destination, "org.example.external", &bundle, authorization).unwrap();
    assert_eq!(
        inspect(&destination, "org.example.external")
            .unwrap()
            .artifact_tree_digest,
        installed.artifact_tree_digest
    );
    let prior_revision = catalogue::selection_revision(&destination).unwrap();
    import_bundle(
        &destination,
        "org.example.external",
        &bundle,
        DestinationAuthorization {
            realms: vec![Realm::SurfaceSession],
            capabilities: vec![],
            source: "second authorized destination".into(),
        },
    )
    .unwrap();
    assert_ne!(
        prior_revision,
        catalogue::selection_revision(&destination).unwrap()
    );
    assert_eq!(
        catalogue::load(&destination)
            .unwrap()
            .desired(Realm::SurfaceSession, "1")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn bundle_identity_allowlist_does_not_grant_runtime_capabilities() {
    use brood_package_manifest::{
        extension::Realm,
        installation::{DestinationAuthorization, export_bundle, import_bundle},
    };
    let (root, manifest, source_store) = fixture(BUILD);
    let source = fs::read_to_string(&manifest).unwrap().replace(
        "placement = \"session.wasm\"",
        "placement = \"session.wasm\"\ncapabilities = [\"filesystem:workspace-read\"]",
    );
    fs::write(&manifest, source).unwrap();
    apply(&plan(&manifest, &source_store).unwrap(), &source_store).unwrap();
    let bundle = root.path().join("bundle");
    export_bundle(&source_store, "org.example.external", None, &bundle).unwrap();
    let destination = root.path().join("destination");
    let error = import_bundle(
        &destination,
        "org.example.external",
        &bundle,
        DestinationAuthorization {
            realms: vec![Realm::SurfaceSession],
            capabilities: vec![],
            source: "only identity was approved".into(),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("not granted"));
    assert!(!destination.join("selected/org.example.external").exists());
}

#[test]
fn state_gate_survives_removal_and_requires_exact_manual_completion() {
    use brood_package_manifest::{
        catalogue,
        extension::Realm,
        installation::{SelectionAction, change_selection, complete_migration, migration_status},
    };
    let (_root, manifest, store) = fixture(BUILD);
    let base = fs::read_to_string(&manifest)
        .unwrap()
        .replace("migration = \"none\"", "migration = \"manual\"");
    fs::write(
        &manifest,
        base.replace(
            "state-compatibility = \"stateless\"",
            "state-compatibility = \"v1\"",
        ),
    )
    .unwrap();
    let first = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    change_selection(&store, "org.example.external", SelectionAction::Remove).unwrap();
    fs::write(
        &manifest,
        base.replace(
            "state-compatibility = \"stateless\"",
            "state-compatibility = \"v2\"",
        ),
    )
    .unwrap();
    let second = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    assert_eq!(
        migration_status(&store, "org.example.external").unwrap()["status"],
        "migration-required"
    );
    let desired = catalogue::load(&store)
        .unwrap()
        .desired(Realm::SurfaceSession, "1")
        .unwrap();
    assert!(desired[0].blocked.as_ref().unwrap().contains("v1"));
    assert!(
        complete_migration(
            &store,
            "org.example.external",
            &first.plan_digest,
            "v1",
            "stale selection"
        )
        .is_err()
    );
    assert!(
        complete_migration(
            &store,
            "org.example.external",
            &second.plan_digest,
            "wrong",
            "stale state"
        )
        .is_err()
    );
    complete_migration(
        &store,
        "org.example.external",
        &second.plan_digest,
        "v1",
        "operator migrated state while roots were stopped",
    )
    .unwrap();
    assert!(
        catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "1")
            .unwrap()[0]
            .blocked
            .is_none()
    );
    fs::write(
        &manifest,
        base.replace(
            "state-compatibility = \"stateless\"",
            "state-compatibility = \"v1\"",
        ),
    )
    .unwrap();
    apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    assert_eq!(
        migration_status(&store, "org.example.external").unwrap()["state"]["compatibility"],
        "v2"
    );
    assert_eq!(
        migration_status(&store, "org.example.external").unwrap()["status"],
        "migration-required"
    );
}

#[test]
fn dependency_graph_is_captured_convergent_and_blocks_dependents() {
    use brood_package_manifest::{
        catalogue,
        extension::Realm,
        installation::{SelectionAction, change_selection},
    };
    let (_root, manifest, store) = fixture(BUILD);
    let source = manifest.parent().unwrap();
    let dependency = source.join("vendor");
    fs::create_dir(&dependency).unwrap();
    for file in ["build.sh", "README.md", "interface.json"] {
        fs::copy(source.join(file), dependency.join(file)).unwrap();
    }
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(
        dependency.join("package.toml"),
        original.replace("org.example.external", "org.example.dependency"),
    )
    .unwrap();
    fs::write(&manifest, format!("{original}\n[[extension.dependencies]]\npackage = \"org.example.dependency\"\nmanifest = \"vendor/package.toml\"\n")).unwrap();
    let resolved = plan(&manifest, &store).unwrap();
    fs::remove_dir_all(source).unwrap();
    let installed = apply(&resolved, &store).unwrap();
    let graph = vec![
        (
            "org.example.dependency",
            installed.dependencies["org.example.dependency"].as_str(),
        ),
        ("org.example.external", installed.plan_digest.as_str()),
    ];
    fs::write(
        store.join("pending-publication.json"),
        serde_json::to_vec(&graph).unwrap(),
    )
    .unwrap();
    fs::remove_file(store.join("selected/org.example.dependency")).unwrap();
    assert!(catalogue::load(&store).is_err());
    let dependency_artifact = store
        .join("objects")
        .join(&installed.dependencies["org.example.dependency"])
        .join("session.wasm");
    let original_artifact = fs::read(&dependency_artifact).unwrap();
    fs::write(&dependency_artifact, b"corrupted").unwrap();
    assert!(brood_package_manifest::installation::recover(&store).is_err());
    assert!(!store.join("selected/org.example.dependency").exists());
    fs::write(dependency_artifact, original_artifact).unwrap();
    brood_package_manifest::installation::recover(&store).unwrap();
    assert!(!store.join("pending-publication.json").exists());
    brood_package_manifest::installation::recover(&store).unwrap();
    assert_eq!(
        installed.dependencies["org.example.dependency"],
        inspect(&store, "org.example.dependency")
            .unwrap()
            .plan_digest
    );
    apply(&resolved, &store).unwrap();
    assert_eq!(fs::read_dir(store.join("attempts")).unwrap().count(), 2);
    assert!(
        catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "test")
            .unwrap()
            .iter()
            .all(|actor| actor.blocked.is_none())
    );
    change_selection(&store, "org.example.dependency", SelectionAction::Disable).unwrap();
    let actors = catalogue::load(&store)
        .unwrap()
        .desired(Realm::SurfaceSession, "test")
        .unwrap();
    assert_eq!(actors.len(), 1);
    assert!(actors[0].blocked.as_ref().unwrap().contains("disabled"));
    change_selection(&store, "org.example.dependency", SelectionAction::Enable).unwrap();
    assert!(
        catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "test")
            .unwrap()
            .iter()
            .all(|actor| actor.blocked.is_none())
    );
}

#[test]
fn parent_failure_does_not_select_successfully_built_dependency() {
    let (_root, manifest, store) = fixture("exit 23\n");
    let source = manifest.parent().unwrap();
    let dependency = source.join("vendor");
    fs::create_dir(&dependency).unwrap();
    for file in ["README.md", "interface.json"] {
        fs::copy(source.join(file), dependency.join(file)).unwrap();
    }
    fs::write(dependency.join("build.sh"), BUILD).unwrap();
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(
        dependency.join("package.toml"),
        original.replace("org.example.external", "org.example.dependency"),
    )
    .unwrap();
    fs::write(&manifest, format!("{original}\n[[extension.dependencies]]\npackage = \"org.example.dependency\"\nmanifest = \"vendor/package.toml\"\n")).unwrap();
    let resolved = plan(&manifest, &store).unwrap();
    fs::remove_dir_all(source).unwrap();
    assert!(apply(&resolved, &store).is_err());
    assert!(inspect(&store, "org.example.dependency").is_err());
    assert!(inspect(&store, "org.example.external").is_err());
    assert_eq!(fs::read_dir(store.join("objects")).unwrap().count(), 1);
}

#[test]
fn diagnostic_schemas_are_captured_and_invalid_replacements_preserve_selection() {
    let (_root, manifest, store) = fixture(BUILD);
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(&manifest, format!("{original}\n[[extension.diagnostics]]\nid = \"org.example.external.health\"\nversion = 1\nschema = \"diagnostic.json\"\n")).unwrap();
    let schema = manifest.parent().unwrap().join("diagnostic.json");
    fs::write(&schema, r#"{"type":"boolean"}"#).unwrap();
    let installed = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    let knowledge =
        brood_package_manifest::installation::knowledge(&store, "org.example.external").unwrap();
    assert_eq!(
        knowledge["diagnostic_schemas"]["org.example.external.health"]["type"],
        "boolean"
    );
    fs::write(&schema, r#"{"type":"not-a-json-schema-type"}"#).unwrap();
    assert!(apply(&plan(&manifest, &store).unwrap(), &store).is_err());
    assert_eq!(
        inspect(&store, "org.example.external").unwrap().plan_digest,
        installed.plan_digest
    );
    fs::write(
        &manifest,
        fs::read_to_string(&manifest)
            .unwrap()
            .replace("org.example.external.health", "another.package.health"),
    )
    .unwrap();
    assert!(plan(&manifest, &store).is_err());
}

#[test]
fn host_contract_requirements_are_visible_in_plans_and_gate_activation() {
    use brood_package_manifest::{catalogue, extension::Realm, installation::describe_plan};
    let (_root, manifest, store) = fixture(BUILD);
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        format!("{original}\n[extension.host-interfaces]\n\"brood.introspection\" = [99]\n"),
    )
    .unwrap();
    let resolved = plan(&manifest, &store).unwrap();
    assert!(
        describe_plan(&resolved).unwrap()["host_compatibility"]["blocked"]
            .as_str()
            .unwrap()
            .contains("version 1")
    );
    apply(&resolved, &store).unwrap();
    let catalogue = catalogue::load(&store).unwrap();
    let actors = catalogue.desired(Realm::SurfaceSession, "test").unwrap();
    assert!(actors[0].blocked.as_ref().unwrap().contains("version 1"));
    assert!(
        catalogue
            .resolve(
                Realm::SurfaceSession,
                "test",
                &actors[0].id,
                &actors[0].reference(),
                &[]
            )
            .is_err()
    );
    fs::write(
        &manifest,
        format!("{original}\n[extension.host-interfaces]\n\"brood.introspection\" = [1, 99]\n"),
    )
    .unwrap();
    apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    assert!(
        brood_package_manifest::catalogue::load(&store)
            .unwrap()
            .desired(Realm::SurfaceSession, "test")
            .unwrap()[0]
            .blocked
            .is_none()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn timed_out_build_preserves_selection_and_retires_its_process_group() {
    let (_root, manifest, store) = fixture(BUILD);
    let selected = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    let text = fs::read_to_string(&manifest).unwrap().replace(
        "[extension.build]",
        "[extension.build]\ntimeout-seconds = 1",
    );
    fs::write(&manifest, text).unwrap();
    fs::write(
        manifest.parent().unwrap().join("build.sh"),
        "while true; do :; done &\necho $! > descendant.pid\nwait\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert!(apply(&plan(&manifest, &store).unwrap(), &store).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(
        inspect(&store, "org.example.external").unwrap().plan_digest,
        selected.plan_digest
    );
    let attempt = fs::read_dir(store.join("attempts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("work/descendant.pid").exists())
        .unwrap();
    let failure: serde_json::Value =
        serde_json::from_slice(&fs::read(attempt.join("failure.json")).unwrap()).unwrap();
    assert!(failure["message"].as_str().unwrap().contains("timeout"));
    let pid = fs::read_to_string(attempt.join("work/descendant.pid")).unwrap();
    let process = PathBuf::from(format!("/proc/{}/stat", pid.trim()));
    for _ in 0..100 {
        let running = fs::read_to_string(&process)
            .ok()
            .is_some_and(|stat| !stat.split(") ").nth(1).unwrap().starts_with('Z'));
        if !running {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed-out build descendant remains running");
}

#[test]
fn oversized_build_outputs_never_become_selected() {
    let (_root, manifest, store) = fixture("printf '%*s' 67108865 '' > session.wasm\n");
    let error = apply(&plan(&manifest, &store).unwrap(), &store).unwrap_err();
    assert!(format!("{error:#}").contains("outputs exceed 64 MiB"));
    assert!(inspect(&store, "org.example.external").is_err());
}

#[test]
fn noisy_build_logs_are_exactly_bounded_and_preserved() {
    let (_root, manifest, store) =
        fixture("printf '%*s' 900 ''\nprintf '%*s' 900 '' >&2\nprintf '%*s' 900 ''\n");
    let text = fs::read_to_string(&manifest).unwrap().replace(
        "[extension.build]",
        "[extension.build]\nlog-limit-bytes = 1024",
    );
    fs::write(&manifest, text).unwrap();
    let error = apply(&plan(&manifest, &store).unwrap(), &store).unwrap_err();
    assert!(format!("{error:#}").contains("log limit"));
    let attempt = fs::read_dir(store.join("attempts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(attempt.join("build.stdout")).unwrap().len(),
        1024
    );
    assert_eq!(
        fs::metadata(attempt.join("build.stderr")).unwrap().len(),
        900
    );
    assert!(attempt.join("failure.json").exists());
    assert!(inspect(&store, "org.example.external").is_err());
}

#[test]
fn lock_holder_subprocess() {
    let Some(store) = std::env::var_os("BROOD_TEST_LOCK_STORE") else {
        return;
    };
    let store = PathBuf::from(store);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.join("apply.lock"))
        .unwrap();
    lock.lock().unwrap();
    fs::write(store.join("test-lock-held"), b"held").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn killed_mutator_does_not_leave_a_stale_store_lock() {
    let (_root, manifest, store) = fixture(BUILD);
    apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_subprocess"])
        .env("BROOD_TEST_LOCK_STORE", &store)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    while !store.join("test-lock-held").exists() {
        if began.elapsed() > std::time::Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("lock helper failed to become ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mutation_blocked = brood_package_manifest::installation::recover(&store).is_err();
    let observation_blocked = brood_package_manifest::catalogue::load(&store).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(mutation_blocked);
    assert!(observation_blocked);
    brood_package_manifest::installation::recover(&store).unwrap();
    assert!(brood_package_manifest::catalogue::load(&store).is_ok());
}

#[test]
fn resource_expectations_survive_planning_installation_and_realm_delivery() {
    use brood_package_manifest::{catalogue, contract, extension::Realm};
    let (_root, manifest, store) = fixture(BUILD);
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        original.replace(
            "scope = \"per-realm\"",
            "scope = \"per-realm\"\nresource-expectations = { timers = 1, memory-bytes = 1048576 }",
        ),
    )
    .unwrap();
    let resolved = plan(&manifest, &store).unwrap();
    fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
    apply(&resolved, &store).unwrap();
    let catalogue = catalogue::load(&store).unwrap();
    let actors = catalogue.desired(Realm::SurfaceSession, "test").unwrap();
    let update = contract::RealmUpdate {
        version: 1,
        revision: catalogue.revision,
        tick: 1,
        actors,
        unavailable_services: Default::default(),
    };
    let decoded = contract::RealmUpdate::decode(&serde_json::to_vec(&update).unwrap()).unwrap();
    assert_eq!(decoded.actors[0].resource_expectations["timers"], 1);
    assert_eq!(
        decoded.actors[0].resource_expectations["memory-bytes"],
        1048576
    );
    assert!(decoded.actors[0].capabilities.is_empty());
}

fn with_prebuilt(manifest: &std::path::Path, version: u32) {
    use brood_package_manifest::installation::digest;
    let bytes = b"\0asm\x0d\0\x01\0";
    fs::write(manifest.parent().unwrap().join("prebuilt.wasm"), bytes).unwrap();
    let mut source = fs::read_to_string(manifest).unwrap();
    source.push_str(&format!(
        r#"
[[extension.prebuilt]]
id = "portable"
host-interfaces = {{ "brood.introspection" = [{version}] }}
[extension.prebuilt.artifacts]
session = {{ path = "prebuilt.wasm", sha256 = "{digest}" }}
presentation = {{ path = "prebuilt.wasm", sha256 = "{digest}" }}
"#,
        digest = digest(bytes)
    ));
    fs::write(manifest, source).unwrap();
}

#[test]
fn prebuilt_plan_survives_missing_build_environment_and_source_removal() {
    let (_root, manifest, store) = fixture("exit 77");
    with_prebuilt(&manifest, 1);
    let source = fs::read_to_string(&manifest).unwrap();
    let source = source
        .lines()
        .map(|line| {
            if line.starts_with("executable =") {
                "executable = '/nix/store/unavailable-toolchain/bin/bash'"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&manifest, source).unwrap();
    fs::remove_file(manifest.parent().unwrap().join("build.sh")).unwrap();
    let resolved = plan(&manifest, &store).unwrap();
    assert_eq!(resolved.prebuilt.as_deref(), Some("portable"));
    assert!(resolved.executable_digest.is_empty());
    assert!(
        !resolved
            .inputs
            .contains_key(std::path::Path::new("build.sh"))
    );
    fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
    let installed = apply(&resolved, &store).unwrap();
    assert_eq!(installed.artifacts.len(), 2);
    assert_eq!(
        apply(&resolved, &store).unwrap().plan_digest,
        installed.plan_digest
    );
    let knowledge =
        brood_package_manifest::installation::knowledge(&store, "org.example.external").unwrap();
    assert_eq!(knowledge["prebuilt"], "portable");
}

#[test]
fn unavailable_or_incompatible_prebuilt_uses_the_captured_recipe() {
    for missing in [true, false] {
        let (_root, manifest, store) = fixture(BUILD);
        with_prebuilt(&manifest, if missing { 1 } else { 99 });
        if missing {
            fs::remove_file(manifest.parent().unwrap().join("prebuilt.wasm")).unwrap();
        }
        let resolved = plan(&manifest, &store).unwrap();
        assert!(resolved.prebuilt.is_none());
        fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
        assert!(apply(&resolved, &store).is_ok());
    }
}

#[test]
fn corrupt_prebuilt_does_not_silently_fall_back_or_replace_selection() {
    let (_root, manifest, store) = fixture(BUILD);
    let old = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    with_prebuilt(&manifest, 1);
    let resolved = plan(&manifest, &store).unwrap();
    let mut altered = resolved.clone();
    altered
        .inputs
        .get_mut(std::path::Path::new("prebuilt.wasm"))
        .unwrap()
        .push(0);
    assert!(apply(&altered, &store).is_err());
    fs::write(manifest.parent().unwrap().join("prebuilt.wasm"), b"corrupt").unwrap();
    assert!(
        format!("{:#}", plan(&manifest, &store).unwrap_err())
            .contains("prebuilt artifact digest mismatch")
    );
    assert_eq!(
        inspect(&store, "org.example.external").unwrap().plan_digest,
        old.plan_digest
    );
    // Once captured, later source corruption cannot change the authorized plan.
    assert_eq!(
        apply(&resolved, &store).unwrap().plan_digest,
        resolved.digest
    );
}

#[test]
fn document_links_are_captured_and_missing_bundle_knowledge_is_rejected() {
    use brood_package_manifest::{
        contract::BUNDLE_MAGIC,
        extension::Realm,
        installation::{self, digest},
    };
    let (_root, manifest, store) = fixture(BUILD);
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(&manifest, original.replace("category = \"example\"", "category = \"example\"\nreferences = [\"manifest\"]\nschemas = [\"schema:interface:example.peer\"]")).unwrap();
    let installed = apply(&plan(&manifest, &store).unwrap(), &store).unwrap();
    let knowledge = installation::knowledge(&store, "org.example.external").unwrap();
    assert_eq!(
        knowledge["documents"][0]["references"],
        serde_json::json!(["manifest"])
    );
    for field in ["references", "schemas"] {
        fs::write(
            &manifest,
            original.replace(
                "category = \"example\"",
                &format!("category = \"example\"\n{field} = [\"missing\"]"),
            ),
        )
        .unwrap();
        assert!(plan(&manifest, &store).is_err());
        assert_eq!(
            inspect(&store, "org.example.external").unwrap().plan_digest,
            installed.plan_digest
        );
    }
    let bundle_path = store.join("test.bundle");
    installation::export_bundle(&store, "org.example.external", None, &bundle_path).unwrap();
    let bundle_bytes = fs::read(&bundle_path).unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&bundle_bytes[BUNDLE_MAGIC.len()..]).unwrap();
    let mut incomplete = knowledge;
    incomplete["documents"] = serde_json::json!([]);
    let knowledge_bytes = serde_json::to_vec_pretty(&incomplete).unwrap();
    let knowledge_digest = digest(&knowledge_bytes);
    bundle["files"]["brood-knowledge.json"] = serde_json::json!(knowledge_bytes);
    bundle["installation"]["knowledge_digest"] = serde_json::json!(knowledge_digest);
    let source: Vec<u8> =
        serde_json::from_value(bundle["files"]["brood-package.toml"].clone()).unwrap();
    let artifacts: std::collections::BTreeMap<String, String> =
        serde_json::from_value(bundle["installation"]["artifacts"].clone()).unwrap();
    bundle["installation"]["artifact_tree_digest"] = serde_json::json!(digest(
        &serde_json::to_vec(&(
            artifacts,
            knowledge_digest,
            String::from_utf8(source).unwrap()
        ))
        .unwrap()
    ));
    let mut bytes = BUNDLE_MAGIC.to_vec();
    bytes.extend(serde_json::to_vec(&bundle).unwrap());
    fs::write(&bundle_path, bytes).unwrap();
    let destination = store.join("destination");
    let error = installation::import_bundle(
        &destination,
        "org.example.external",
        &bundle_path,
        installation::DestinationAuthorization {
            realms: vec![Realm::SurfaceSession],
            capabilities: vec![],
            source: "test".into(),
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("document set differs"));
    assert!(!destination.join("selected/org.example.external").exists());
}
