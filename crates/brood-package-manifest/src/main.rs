use brood_package_manifest::{INSTALLED_MANIFEST_FILE, INSTALLED_ROOT_FILE, Manifest};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let Some(command) = arguments.next() else {
        return Err(usage().into());
    };

    match command.to_str() {
        Some("validate") => {
            let manifest_path = next_path(&mut arguments)?;
            ensure_finished(arguments)?;
            let manifest = Manifest::from_path(&manifest_path)?;
            println!(
                "package={} root={} artifacts={}",
                manifest.package.id,
                manifest.package.root,
                manifest.artifacts.len()
            );
        }
        Some("plan") => {
            let manifest_path = next_path(&mut arguments)?;
            ensure_finished(arguments)?;
            let manifest = Manifest::from_path(&manifest_path)?;
            println!(
                "package\t{}\t{}\t{}",
                manifest.package.id, manifest.package.version, manifest.package.root
            );
            for artifact in manifest.installation_order()? {
                println!(
                    "artifact\t{}\t{}\t{}",
                    artifact.id,
                    artifact.source.display(),
                    artifact.placement.display()
                );
            }
        }
        Some("assemble") => {
            let manifest_path = next_path(&mut arguments)?;
            let source_root = next_path(&mut arguments)?;
            let output_root = next_path(&mut arguments)?;
            ensure_finished(arguments)?;
            assemble(&manifest_path, &source_root, &output_root)?;
        }
        _ => return Err(usage().into()),
    }
    Ok(())
}

fn next_path(arguments: &mut impl Iterator<Item = OsString>) -> Result<PathBuf, &'static str> {
    arguments.next().map(PathBuf::from).ok_or_else(usage)
}

fn ensure_finished(mut arguments: impl Iterator<Item = OsString>) -> Result<(), &'static str> {
    if arguments.next().is_some() {
        Err(usage())
    } else {
        Ok(())
    }
}

const fn usage() -> &'static str {
    "usage: brood-package-manifest <validate MANIFEST | plan MANIFEST | assemble MANIFEST SOURCE_ROOT OUTPUT_ROOT>"
}

fn assemble(
    manifest_path: &Path,
    source_root: &Path,
    output_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let manifest = Manifest::from_path(manifest_path)?;
    if output_root.exists() {
        return Err(format!("package output already exists: {}", output_root.display()).into());
    }

    let order = manifest.installation_order()?;
    for artifact in &order {
        let source = source_root.join(&artifact.source);
        if !source.is_file() {
            return Err(format!(
                "artifact {} source is not a regular file: {}",
                artifact.id,
                source.display()
            )
            .into());
        }
    }

    fs::create_dir_all(output_root)?;
    for artifact in order {
        let source = source_root.join(&artifact.source);
        let destination = output_root.join(&artifact.placement);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination)?;
        println!(
            "installed={} source={} placement={}",
            artifact.id,
            source.display(),
            artifact.placement.display()
        );
    }

    fs::copy(manifest_path, output_root.join(INSTALLED_MANIFEST_FILE))?;
    let root = manifest.root_artifact()?;
    fs::write(
        output_root.join(INSTALLED_ROOT_FILE),
        format!("{}\n", root.placement.display()),
    )?;
    println!(
        "assembled={} root={}",
        manifest.package.id,
        root.placement.display()
    );
    Ok(())
}
