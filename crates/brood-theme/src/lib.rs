//! Host-side Helix 25.07.1 theme resolution. See docs/progress/themes.md.
use anyhow::{Context, Result, bail, ensure};
use brood_graphics::theme::{self, RoleStyle, ThemeSnapshot, UnderlineStyle};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::PathBuf,
};
use toml::{Table, Value};

include!(concat!(env!("OUT_DIR"), "/bundled.rs"));

/// Search roots ordered highest priority first. Bundled files are the final root.
#[derive(Clone, Debug, Default)]
pub struct ThemeLoader {
    pub directories: Vec<PathBuf>,
}

impl ThemeLoader {
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names: BTreeSet<String> = BUNDLED.iter().map(|(name, _)| (*name).into()).collect();
        for directory in &self.directories {
            if !directory.exists() {
                continue;
            }
            for entry in std::fs::read_dir(directory)? {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "toml")
                    && let Some(name) = path.file_stem().and_then(|name| name.to_str())
                {
                    names.insert(name.into());
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    pub fn load(&self, name: &str, revision: u64) -> Result<ThemeSnapshot> {
        let mut chain = Vec::new();
        let mut table = self.inherit(name, &mut chain)?;
        let palette = table
            .remove("palette")
            .unwrap_or_else(|| Value::Table(Table::new()));
        let palette = palette.as_table().context("palette must be a table")?;
        let mut colors = BTreeMap::new();
        for (key, value) in palette {
            let text = value.as_str().context("palette colors must be strings")?;
            // Helix palette entries are literal RGB or numeric ANSI values, not aliases.
            colors.insert(
                key.as_str(),
                literal(text).with_context(|| format!("palette {key}"))?,
            );
        }
        let mut roles = BTreeMap::new();
        for (scope, value) in table {
            roles.insert(
                scope.clone(),
                parse_style(value, &colors)
                    .with_context(|| format!("theme {name}, role {scope}"))?,
            );
        }
        let snapshot = ThemeSnapshot {
            revision,
            name: name.into(),
            roles,
        };
        snapshot.encode().map_err(anyhow::Error::msg)?;
        Ok(snapshot)
    }

    fn inherit(&self, name: &str, chain: &mut Vec<String>) -> Result<Table> {
        ensure!(
            !name.is_empty()
                && name.len() <= 256
                && !name.contains(['/', '\\'])
                && name != "."
                && name != "..",
            "invalid theme name {name:?}"
        );
        ensure!(
            chain.len() < 32,
            "theme inheritance exceeds 32 levels: {chain:?}"
        );
        ensure!(
            !chain.iter().any(|entry| entry == name),
            "theme inheritance cycle: {chain:?} -> {name}"
        );
        chain.push(name.into());
        let mut source = None;
        for directory in &self.directories {
            let path = directory.join(format!("{name}.toml"));
            match std::fs::File::open(&path) {
                Ok(file) => {
                    let mut contents = String::new();
                    file.take(theme::MAX_THEME_BYTES as u64 + 1)
                        .read_to_string(&mut contents)?;
                    ensure!(
                        contents.len() <= theme::MAX_THEME_BYTES,
                        "theme {} exceeds byte limit",
                        path.display()
                    );
                    source = Some(contents);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("read theme {}", path.display()));
                }
            }
        }
        let source = source
            .or_else(|| {
                BUNDLED
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).into())
            })
            .with_context(|| format!("theme {name:?} not found (inheritance {chain:?})"))?;
        let mut table: Table =
            toml::from_str(&source).with_context(|| format!("parse theme {name}"))?;
        if let Some(parent) = table.remove("inherits") {
            let mut base = self.inherit(
                parent.as_str().context("inherits must be a theme name")?,
                chain,
            )?;
            if let Some(palette) = table.remove("palette") {
                let palette = palette.as_table().context("palette must be a table")?;
                let base_palette = base
                    .entry("palette")
                    .or_insert_with(|| Value::Table(Table::new()))
                    .as_table_mut()
                    .context("parent palette must be a table")?;
                base_palette.extend(palette.clone());
            }
            // Helix replaces complete scope styles, but merges palette entries.
            base.extend(table);
            table = base;
        }
        chain.pop();
        ensure!(
            table.len() <= theme::MAX_THEME_ROLES + 1,
            "too many theme roles"
        );
        Ok(table)
    }
}

fn literal(text: &str) -> Result<u32> {
    if let Some(hex) = text.strip_prefix('#') {
        ensure!(
            hex.len() >= 6 && hex.is_ascii(),
            "expected #RRGGBB, got {text:?}"
        );
        return u32::from_str_radix(hex.get(..6).context("short hex color")?, 16)
            .context("invalid RGB color");
    }
    let index: u8 = text
        .parse()
        .with_context(|| format!("invalid color {text:?}"))?;
    Ok(indexed(index))
}

fn indexed(index: u8) -> u32 {
    const ANSI: [u32; 16] = [
        0x0000_0000,
        0x0080_0000,
        0x0000_8000,
        0x0080_8000,
        0x0000_0080,
        0x0080_0080,
        0x0000_8080,
        0x00c0_c0c0,
        0x0080_8080,
        0x00ff_0000,
        0x0000_ff00,
        0x00ff_ff00,
        0x0000_00ff,
        0x00ff_00ff,
        0x0000_ffff,
        0x00ff_ffff,
    ];
    if let Some(color) = ANSI.get(usize::from(index)) {
        return *color;
    }
    let index = u32::from(index);
    if index >= 232 {
        let v = 8 + (index - 232) * 10;
        return v * 0x0001_0101;
    }
    let cube = index - 16;
    let component = |v| if v == 0 { 0 } else { 55 + 40 * v };
    (component(cube / 36) << 16) | (component(cube / 6 % 6) << 8) | component(cube % 6)
}

fn color(value: &Value, palette: &BTreeMap<&str, u32>, background: bool) -> Result<u32> {
    let text = value.as_str().context("color must be a string")?;
    if let Some(rgb) = palette.get(text) {
        return Ok(*rgb);
    }
    if text == "default" {
        return Ok(if background {
            theme::ashen::BACKGROUND_RGB
        } else {
            theme::ashen::TEXT_RGB
        });
    }
    let names = [
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "gray",
        "light-gray",
        "light-red",
        "light-green",
        "light-yellow",
        "light-blue",
        "light-magenta",
        "light-cyan",
        "white",
    ];
    if let Some(index) = names.iter().position(|name| *name == text) {
        return Ok(indexed(u8::try_from(index)?));
    }
    literal(text)
}

fn parse_style(value: Value, palette: &BTreeMap<&str, u32>) -> Result<RoleStyle> {
    let mut style = RoleStyle::default();
    let Value::Table(entries) = value else {
        style.fg = Some(color(&value, palette, false)?);
        return Ok(style);
    };
    for (key, value) in entries {
        match key.as_str() {
            "fg" => style.fg = Some(color(&value, palette, false)?),
            "bg" => style.bg = Some(color(&value, palette, true)?),
            "modifiers" => {
                let mut bits = 0;
                for modifier in value.as_array().context("modifiers must be an array")? {
                    bits |= match modifier.as_str().context("modifier must be a string")? {
                        "bold" => theme::BOLD,
                        "dim" => theme::DIM,
                        "italic" => theme::ITALIC,
                        "slow_blink" => theme::SLOW_BLINK,
                        "rapid_blink" => theme::RAPID_BLINK,
                        "reversed" => theme::REVERSED,
                        "hidden" => theme::HIDDEN,
                        "crossed_out" => theme::CROSSED_OUT,
                        "underlined" => {
                            style.underline_style = Some(UnderlineStyle::Line);
                            0
                        }
                        other => bail!("unsupported modifier {other:?}"),
                    };
                }
                style.modifiers = Some(bits);
            }
            "underline" => {
                for (key, value) in value.as_table().context("underline must be a table")? {
                    match key.as_str() {
                        "color" => style.underline_color = Some(color(value, palette, false)?),
                        "style" => {
                            style.underline_style = Some(
                                match value.as_str().context("underline style must be a string")? {
                                    "line" => UnderlineStyle::Line,
                                    "curl" => UnderlineStyle::Curl,
                                    "dashed" => UnderlineStyle::Dashed,
                                    "dotted" => UnderlineStyle::Dotted,
                                    "double_line" => UnderlineStyle::DoubleLine,
                                    other => bail!("unsupported underline style {other:?}"),
                                },
                            );
                        }
                        other => bail!("unsupported underline attribute {other:?}"),
                    }
                }
            }
            other => bail!("unsupported style attribute {other:?}"),
        }
    }
    Ok(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_themes_resolve_and_round_trip() -> Result<()> {
        for (name, _) in BUNDLED {
            let theme = ThemeLoader::default().load(name, 17)?;
            assert_eq!(
                ThemeSnapshot::decode(&theme.encode().map_err(anyhow::Error::msg)?)
                    .map_err(anyhow::Error::msg)?,
                theme
            );
            assert_ne!(theme.fg("ui.text"), theme.bg("ui.background"));
        }
        Ok(())
    }
    #[test]
    fn inheritance_partial_styles_defaults_and_cycles() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("parent.toml"),
            "\"ui.text\" = { fg = \"ink\", bg = \"#ffffff\" }\n[palette]\nink = \"#123456\"\n",
        )?;
        std::fs::write(
            dir.path().join("child.toml"),
            "inherits = \"parent\"\n\"ui.text.focus\" = { modifiers = [\"bold\", \"slow_blink\"], underline = { style = \"curl\" } }\n\"ui.background\" = { bg = \"default\" }\n[palette]\nink = \"#654321\"\n",
        )?;
        let loader = ThemeLoader {
            directories: vec![dir.path().into()],
        };
        let theme = loader.load("child", 2)?;
        assert_eq!(theme.style("ui.text.other").fg, Some(0x0065_4321));
        let partial = theme.style("ui.text.focus.child");
        assert_eq!(partial.fg, None);
        assert_eq!(partial.modifiers, Some(theme::BOLD | theme::SLOW_BLINK));
        assert_eq!(partial.over(theme.style("ui.text")).fg, Some(0x0065_4321));
        assert_eq!(
            theme.style("ui.background").bg,
            Some(theme::ashen::BACKGROUND_RGB)
        );
        std::fs::write(dir.path().join("parent.toml"), "inherits = \"child\"")?;
        assert!(loader.load("child", 3).is_err());
        assert!(loader.load("missing", 3).is_err());
        Ok(())
    }
    #[test]
    fn invalid_attributes_and_references_are_errors() {
        for source in [
            "fg = \"unknown\"",
            "modifiers = [\"sparkle\"]",
            "underline = { style = \"wave\" }",
            "unknown = 1",
        ] {
            let value: Value = toml::from_str(source).unwrap();
            assert!(parse_style(value, &BTreeMap::new()).is_err());
        }
    }
}
