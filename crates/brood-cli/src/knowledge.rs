//! Offline, versioned description of the querying executable.
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

#[derive(Debug, Parser)]
pub struct KnowledgeArgs {
    #[command(subcommand)]
    pub command: KnowledgeCommand,
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    /// Describe the bundle and supported query operations.
    Describe,
    /// Search document IDs and text with bounded pagination.
    Search {
        query: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
    },
    /// Retrieve one document by its stable ID.
    Get { id: String },
    /// Export the complete indexed JSON bundle to a new file.
    Export { path: PathBuf },
}

/// Read contracts without connecting to an endpoint or starting guest code.
pub fn bundle() -> Result<Value> {
    let mut bundle: Value =
        serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/knowledge.json")))?;
    let command = crate::Args::command();
    bundle
        .as_object_mut()
        .context("invalid knowledge envelope")?
        .insert("command_reference".into(), describe_command(&command));
    let generated = crate::introspection::generated_schemas();
    let entries = bundle
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .context("missing knowledge entries")?;
    for (id, schema) in generated {
        entries.push(json!({"id": id, "category": "generated-contract", "content": serde_json::to_string_pretty(&schema)?}));
    }
    crate::knowledge_validation::validate(entries, true).map_err(anyhow::Error::msg)?;
    // Include generated structural contracts in the exported content identity.
    let digest = brood_package_manifest::installation::digest(&serde_json::to_vec(entries)?);
    bundle
        .as_object_mut()
        .context("invalid knowledge envelope")?
        .insert("content_digest".into(), json!(digest));
    Ok(bundle)
}

fn describe_command(command: &clap::Command) -> Value {
    json!({
        "name": command.get_name(),
        "about": command.get_about().map(ToString::to_string),
        "arguments": command.get_arguments().map(|arg| json!({
            "id": arg.get_id().as_str(), "long": arg.get_long(), "short": arg.get_short(),
            "help": arg.get_help().map(ToString::to_string), "required": arg.is_required_set()
        })).collect::<Vec<_>>(),
        "subcommands": command.get_subcommands().map(describe_command).collect::<Vec<_>>()
    })
}

/// Query the same indexed description used by CLI export.
pub fn query(command: &KnowledgeCommand) -> Result<Value> {
    let mut bundle = bundle()?;
    let entries = bundle
        .as_object_mut()
        .context("invalid knowledge envelope")?
        .remove("entries")
        .context("missing knowledge entries")?;
    let entries = entries
        .as_array()
        .context("invalid embedded knowledge index")?;
    match command {
        KnowledgeCommand::Describe => {
            let metadata = bundle
                .as_object_mut()
                .context("invalid knowledge envelope")?;
            metadata.insert(
                "operations".into(),
                json!(["describe", "search", "get", "export"]),
            );
            metadata.insert("entry_count".into(), json!(entries.len()));
            Ok(bundle)
        }
        KnowledgeCommand::Get { id } => entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some(id))
            .cloned()
            .context(format!("unknown knowledge document: {id}")),
        KnowledgeCommand::Search {
            query,
            offset,
            limit,
        } => {
            if !(1..=100).contains(limit) {
                bail!("knowledge page limit must be between 1 and 100");
            }
            let needle = query.to_lowercase();
            let matching: Vec<_> = entries
                .iter()
                .filter(|entry| {
                    entry["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
                        || entry["content"]
                            .as_str()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect();
            let page: Vec<_> = matching
                .iter()
                .skip(*offset)
                .take(*limit as usize)
                .map(|entry| json!({"id": entry["id"], "category": entry["category"]}))
                .collect();
            let end = offset.saturating_add(page.len());
            Ok(
                json!({"schema_version": 1, "bundle_digest": bundle.get("content_digest"),
                "total": matching.len(), "next_offset": (end < matching.len()).then_some(end), "entries": page}),
            )
        }
        KnowledgeCommand::Export { .. } => bail!("export is a file operation"),
    }
}

pub fn run(args: &KnowledgeArgs) -> Result<()> {
    if let KnowledgeCommand::Export { path } = &args.command {
        use std::io::Write;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create knowledge export {}", path.display()))?;
        output.write_all(&serde_json::to_vec_pretty(&bundle()?)?)?;
    } else {
        let result = response(&args.command, "querying-binary");
        println!("{}", serde_json::to_string_pretty(&result)?);
        if result.get("error").is_some_and(|error| !error.is_null()) {
            bail!("knowledge query failed");
        }
    }
    Ok(())
}

/// Versioned response shared by offline CLI and connected endpoint queries.
/// A plugin may use the ordinary control channel to request the same document.
#[must_use]
pub fn response(command: &KnowledgeCommand, endpoint: &str) -> Value {
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    match query(command) {
        Ok(data) => {
            json!({"schema_version": 1, "endpoint": endpoint, "observed_at_unix_ms": observed_at,
            "data": data, "error": null})
        }
        Err(error) => {
            json!({"schema_version": 1, "endpoint": endpoint, "observed_at_unix_ms": observed_at,
            "data": null, "error": {"code": "knowledge-query-failed", "message": error.to_string()}})
        }
    }
}

/// Parse a connected read-only request using the exact CLI parser.
pub fn endpoint_query(arguments: &[String], endpoint: &str) -> Result<String> {
    let parsed = KnowledgeArgs::try_parse_from(
        std::iter::once("knowledge").chain(arguments.iter().map(String::as_str)),
    );
    let value = match parsed {
        Ok(args) => response(&args.command, endpoint),
        Err(error) => json!({"schema_version": 1, "endpoint": endpoint,
            "data": null, "error": {"code": "invalid-request", "message": error.to_string()}}),
    };
    Ok(serde_json::to_string(&value)?)
}
