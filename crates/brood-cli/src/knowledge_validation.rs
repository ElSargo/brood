//! Shared build/runtime knowledge checks; no runtime or filesystem authority.
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

// One registry drives both schema generation and build-time source association
// checks. The build callback consumes paths without compiling host runtime code.
macro_rules! schema_entries {
    ($visitor:ident) => { $visitor! {
        "brood.realm-init-config.v1", "crates/brood-cli/src/realm_init.rs", crate::realm_init::RootConfig;
        "brood.runtime-config.v1", "crates/brood-cli/src/configuration.rs", crate::configuration::BroodConfig;
        "brood.introspection.request.v1", "crates/brood-package-contract/src/introspection.rs", crate::package_runtime::contract::introspection::Request;
        "brood.introspection.response.v1", "crates/brood-package-contract/src/introspection.rs", crate::package_runtime::contract::introspection::Response;
        "brood.package-instance.v1", "crates/brood-cli/src/package_runtime.rs", crate::package_runtime::InstanceRecord;
        "brood.package-activation.v1", "crates/brood-package-contract/src/lib.rs", crate::package_runtime::contract::RealmUpdate;
        "brood.package-policy.v1", "crates/brood-package-contract/src/lib.rs", crate::package_runtime::contract::PolicyReport;
        "brood.package-service-request.v1", "crates/brood-package-contract/src/services.rs", crate::package_runtime::contract::services::Request;
        "brood.package-service-event.v1", "crates/brood-package-contract/src/services.rs", crate::package_runtime::contract::services::Event;
        "brood.package-peer.v1", "crates/brood-package-contract/src/services.rs", crate::package_runtime::contract::services::Peer;
    }};
}
pub(crate) use schema_entries;

pub fn schema_sources() -> BTreeMap<&'static str, &'static str> {
    macro_rules! sources { ($($id:literal, $source:literal, $ty:ty;)*) => {
        BTreeMap::from([$ (($id, $source)),*])
    }; }
    self::schema_entries!(sources)
}

/// Wiki references outside fenced/inline code resolve relative to their source.
pub fn references(id: &str, content: &str) -> Result<Vec<String>, String> {
    if !Path::new(id)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        return Ok(vec![]);
    }
    let mut result = BTreeSet::new();
    let mut fence = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        for prose in prose_segments(line) {
            for reference in prose.split("[[").skip(1) {
                let Some((reference, _)) = reference.split_once("]]") else {
                    continue;
                };
                let reference = reference
                    .split('|')
                    .next()
                    .unwrap_or_default()
                    .split('#')
                    .next()
                    .unwrap_or_default();
                if reference.is_empty() {
                    continue;
                }
                let mut path = Path::new(id)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(reference);
                if path.extension().is_none() {
                    path.set_extension("md");
                }
                let mut parts = Vec::new();
                for component in path.components() {
                    match component {
                        Component::Normal(part) => {
                            parts.push(part.to_str().ok_or("non-UTF8 knowledge reference")?);
                        }
                        Component::CurDir => {}
                        Component::ParentDir => {
                            if parts.pop().is_none() {
                                return Err(format!(
                                    "reference escapes bundle: {id} -> {reference}"
                                ));
                            }
                        }
                        _ => {
                            return Err(format!(
                                "absolute knowledge reference: {id} -> {reference}"
                            ));
                        }
                    }
                }
                result.insert(parts.join("/"));
            }
        }
    }
    Ok(result.into_iter().collect())
}

fn prose_segments(mut line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut delimiter = None;
    while let Some(index) = line.find('`') {
        let (before, rest) = line.split_at(index);
        let width = rest.bytes().take_while(|byte| *byte == b'`').count();
        if delimiter.is_none() {
            segments.push(before);
            delimiter = Some(width);
        } else if delimiter == Some(width) {
            delimiter = None;
        }
        line = rest.split_at(width).1;
    }
    if delimiter.is_none() {
        segments.push(line);
    }
    segments
}

pub fn validate(entries: &[Value], require_generated: bool) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for entry in entries {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or("knowledge entry ID missing")?;
        if !ids.insert(id) {
            return Err(format!("duplicate knowledge document: {id}"));
        }
    }
    for entry in entries {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or("knowledge entry ID missing")?;
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .ok_or("knowledge entry content missing")?;
        for reference in references(id, content)? {
            if !ids.contains(reference.as_str()) {
                return Err(format!(
                    "unresolved knowledge reference: {id} -> {reference}"
                ));
            }
        }
    }
    for (id, source) in schema_sources() {
        if !ids.contains(source) {
            return Err(format!("schema source missing: {id} -> {source}"));
        }
        if require_generated {
            let entry = entries
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
                .ok_or_else(|| format!("required schema missing: {id}"))?;
            let schema: Value = serde_json::from_str(
                entry
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("schema text missing")?,
            )
            .map_err(|error| error.to_string())?;
            if schema.get("$id").and_then(Value::as_str) != Some(id)
                || schema.get("x-source-document").and_then(Value::as_str) != Some(source)
            {
                return Err(format!("schema association mismatch: {id}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "tests fail immediately when fixture construction or expected rejection fails"
)]
mod tests {
    #[test]
    fn references_and_schema_sources_cannot_silently_disappear() {
        use serde_json::json;
        assert_eq!(
            super::references(
                "docs/a.md",
                "[[b]] `[[inline]]` ``[[also-inline]]``\n```\n[[code]]\n```\n"
            )
            .unwrap(),
            vec!["docs/b.md"]
        );
        assert!(super::references("docs/a.md", "[[../../outside]]").is_err());
        let sources = super::schema_sources();
        let mut entries: Vec<_> = sources
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|source| json!({"id": source, "content": ""}))
            .collect();
        entries.push(json!({"id": "docs/a.md", "content": "[[b]]"}));
        assert!(
            super::validate(&entries, false)
                .unwrap_err()
                .contains("unresolved")
        );
        entries.push(json!({"id": "docs/b.md", "content": "available"}));
        assert!(super::validate(&entries, false).is_ok());
        for (id, source) in sources {
            entries.push(json!({"id": id, "content": json!({"$id": id, "x-source-document": source}).to_string()}));
        }
        assert!(super::validate(&entries, true).is_ok());
        entries.last_mut().unwrap()["content"] = json!("{}");
        assert!(
            super::validate(&entries, true)
                .unwrap_err()
                .contains("association mismatch")
        );
    }
}
