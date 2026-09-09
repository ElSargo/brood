use anyhow::{Context, Result, ensure};
use brood_cli::knowledge::{KnowledgeCommand, bundle, endpoint_query, query};

#[test]
fn offline_contracts_and_generated_parser_are_discoverable() -> Result<()> {
    let bundle = bundle()?;
    let entries = bundle
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .context("entries")?;
    ensure!(
        entries
            .iter()
            .any(|entry| entry.get("id").and_then(serde_json::Value::as_str)
                == Some("crates/brood-wasm/wit/brood-plugin.wit"))
    );
    ensure!(bundle.get("command_reference").is_some());
    let page = query(&KnowledgeCommand::Search {
        query: "actor".into(),
        offset: 0,
        limit: 2,
    })?;
    ensure!(
        page.get("entries")
            .and_then(serde_json::Value::as_array)
            .context("page")?
            .len()
            == 2
    );
    ensure!(page.get("next_offset").and_then(serde_json::Value::as_u64) == Some(2));
    let document = query(&KnowledgeCommand::Get {
        id: "examples/wasm-init-child/src/lib.rs".into(),
    })?;
    ensure!(
        document
            .get("content")
            .and_then(serde_json::Value::as_str)
            .context("example content")?
            .contains("Guest")
    );
    Ok(())
}

#[test]
fn endpoint_queries_report_identity_and_do_not_export_server_files() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("must-not-exist.json");
    let encoded = endpoint_query(
        &["export".into(), path.display().to_string()],
        "test-endpoint",
    )?;
    let response: serde_json::Value = serde_json::from_str(&encoded)?;
    ensure!(!path.exists());
    ensure!(response.get("error").is_some_and(|error| !error.is_null()));
    ensure!(response.get("endpoint").and_then(serde_json::Value::as_str) == Some("test-endpoint"));
    let missing: serde_json::Value = serde_json::from_str(&endpoint_query(
        &["get".into(), "absent".into()],
        "test-endpoint",
    )?)?;
    ensure!(missing.get("error").is_some_and(|error| !error.is_null()));
    Ok(())
}
