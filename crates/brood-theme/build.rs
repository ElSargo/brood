use std::{env, fmt::Write, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let themes = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("themes");
    println!("cargo:rerun-if-changed=themes");
    let mut paths = fs::read_dir(themes)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "toml")
    });
    paths.sort();
    let mut source = String::from("pub const BUNDLED: &[(&str, &str)] = &[\n");
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or("invalid bundled theme name")?;
        let path = path.to_str().ok_or("invalid bundled theme path")?;
        writeln!(source, "({name:?}, include_str!({path:?})),")?;
    }
    source.push_str("];\n");
    fs::write(
        PathBuf::from(env::var("OUT_DIR")?).join("bundled.rs"),
        source,
    )?;
    Ok(())
}
