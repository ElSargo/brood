#[path = "src/knowledge_validation.rs"]
mod knowledge_validation;
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn collect(root: &Path, path: &Path, entries: &mut Vec<serde_json::Value>) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    children.sort();
    for child in children {
        if child
            .file_name()
            .is_some_and(|name| name == "target" || name == ".git")
        {
            continue;
        }
        if child.is_dir() {
            collect(root, &child, entries)?;
            continue;
        }
        let extension = child
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !matches!(extension, "md" | "wit" | "witx" | "rs" | "toml") {
            continue;
        }
        println!("cargo:rerun-if-changed={}", child.display());
        let id = child
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 document path")?;
        let category = if id.starts_with("docs/targets/") {
            "target"
        } else if id.starts_with("docs/progress/") {
            "progress"
        } else if id.starts_with("docs/frictions/") {
            "friction"
        } else if matches!(extension, "wit" | "witx" | "rs" | "toml") {
            "contract-source"
        } else {
            "reference"
        };
        entries.push(serde_json::json!({"id": id, "category": category, "content": fs::read_to_string(&child)?}));
    }
    Ok(())
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed=BROOD_BUILD_SOURCE_INFO");
    let packaged_source: Option<serde_json::Value> = env::var("BROOD_BUILD_SOURCE_INFO")
        .ok()
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut entries = Vec::new();
    for directory in [
        "docs",
        "crates/brood-wasm/wit",
        "crates/brood-plugin-sdk",
        "crates/brood-package-contract",
        "crates/brood-cli/src",
        "crates/brood-package-manifest/src",
        "examples/wasm-init-child",
        "examples/package-init",
        "examples/self-describing-pair",
    ] {
        let path = root.join(directory);
        // Watching a whole example crate also watches its target directory,
        // making successful guest builds invalidate the host knowledge bundle.
        if directory == "docs" || directory.ends_with("/src") || directory.ends_with("/wit") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        collect(&root, &path, &mut entries)?;
    }
    for required in [
        "crates/brood-wasm/wit/brood-plugin.wit",
        "crates/brood-plugin-sdk/src/lib.rs",
        "examples/wasm-init-child/src/lib.rs",
    ] {
        if !entries.iter().any(|entry| entry["id"] == required) {
            return Err(format!("missing extension contract: {required}").into());
        }
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    // Missing .git files in Nix sources must not force every Cargo invocation
    // to rerun this build script. Resolve worktree paths through Git as well.
    for git_path in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", git_path]) {
            let path = root.join(path);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        let path = root.join(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    for file in [
        "crates/broodd/src/config.rs",
        "crates/brood-package-manifest/Cargo.toml",
        "crates/brood-package-manifest/README.md",
        "crates/brood-package-manifest/Cargo.lock",
        "Cargo.lock",
    ] {
        let path = root.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        entries.push(serde_json::json!({"id": file, "category": "contract-source", "content": fs::read_to_string(path)?}));
    }
    // Include referenced text assets, including historical JSON evidence, so
    // every internal document reference resolves inside the exported bundle.
    let canonical_root = root.canonicalize()?;
    let mut cursor = 0;
    while cursor < entries.len() {
        let entry = entries
            .get(cursor)
            .ok_or("knowledge traversal cursor missing")?;
        let id = entry["id"].as_str().ok_or("missing document ID")?;
        let references = knowledge_validation::references(
            id,
            entry["content"]
                .as_str()
                .ok_or("missing document content")?,
        )?;
        for reference in references {
            if entries.iter().any(|entry| entry["id"] == reference) {
                continue;
            }
            let path = root
                .join(&reference)
                .canonicalize()
                .map_err(|error| format!("unresolved knowledge reference {reference}: {error}"))?;
            if !path.starts_with(&canonical_root) || fs::metadata(&path)?.len() > 2 * 1024 * 1024 {
                return Err(format!("invalid referenced knowledge asset: {reference}").into());
            }
            println!("cargo:rerun-if-changed={}", path.display());
            let category = if reference.starts_with("docs/progress/") {
                "progress"
            } else {
                "reference"
            };
            entries.push(serde_json::json!({"id": reference, "category": category, "content": fs::read_to_string(path)?}));
        }
        cursor += 1;
    }
    knowledge_validation::validate(&entries, false)?;
    let encoded = serde_json::to_vec(&entries)?;
    let bundle = serde_json::json!({
        "schema_version": 1, "version": env!("CARGO_PKG_VERSION"),
        "source_revision": git(&["rev-parse", "HEAD"]).map(serde_json::Value::String).or_else(|| packaged_source.as_ref().and_then(|source| source.get("revision")).cloned()),
        "local_modifications": git(&["status", "--porcelain"]),
        "packaged_source": packaged_source,
        "target": env::var("TARGET")?,
        "profile": env::var("PROFILE")?,
        "optimization": env::var("OPT_LEVEL")?,
        "debug": env::var("DEBUG")?,
        "compiler": Command::new(env::var("RUSTC")?).arg("-vV").output().ok().filter(|output| output.status.success()).map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
        "features": env::vars().filter(|(key, _)| key.starts_with("CARGO_FEATURE_")).map(|(key, _)| key).collect::<Vec<_>>(),
        "content_digest": format!("{:x}", Sha256::digest(&encoded)),
        "source_snapshot": "extension sources only; full host source and build dependencies are not bundled",
        "entries": entries
    });
    fs::write(
        Path::new(&env::var("OUT_DIR")?).join("knowledge.json.zst"),
        zstd::stream::encode_all(serde_json::to_vec(&bundle)?.as_slice(), 3)?,
    )?;
    Ok(())
}
