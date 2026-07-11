use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, ensure};
use wit_component::ComponentEncoder;

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .context("usage: componentize <input> <output>")?,
    );
    let output = PathBuf::from(
        args.next()
            .context("usage: componentize <input> <output>")?,
    );
    ensure!(
        args.next().is_none(),
        "usage: componentize <input> <output>"
    );

    let module = fs::read(&input).with_context(|| format!("read {}", input.display()))?;
    let component = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()
        .context("encode component")?;
    fs::write(&output, component).with_context(|| format!("write {}", output.display()))?;
    Ok(())
}
