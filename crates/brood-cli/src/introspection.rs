//! Common bounded responses for independently queryable package observations.
use crate::package_runtime::contract::introspection::{
    Collection, Error, MAX_RESPONSE_BYTES, Request, Response, SCHEMA_ID,
};
use serde_json::{Value, json};

#[must_use]
pub fn response(
    endpoint: &str,
    authority: &str,
    request: Result<Request, String>,
    records: Result<Vec<Value>, String>,
) -> Response {
    let mut result = Response {
        schema: SCHEMA_ID.into(),
        version: 1,
        endpoint: endpoint.into(),
        observed_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| {
                u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
            }),
        authority: authority.into(),
        collection: None,
        revision: None,
        offset: 0,
        next_offset: None,
        total: 0,
        data: vec![],
        error: None,
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            result.error = Some(Error {
                code: "invalid-request".into(),
                message,
            });
            return result;
        }
    };
    result.collection = Some(request.collection);
    result.offset = request.offset;
    let records = match records {
        Ok(records) => records,
        Err(message) => {
            result.error = Some(Error {
                code: "observation-unavailable".into(),
                message,
            });
            return result;
        }
    };
    let mut identity = json!(records);
    strip_observation_clock(&mut identity);
    let revision = brood_package_manifest::installation::digest(
        &serde_json::to_vec(&identity).unwrap_or_default(),
    );
    result.revision = Some(revision.clone());
    result.total = records.len();
    if request
        .revision
        .as_ref()
        .is_some_and(|expected| expected != &revision)
    {
        result.error = Some(Error {
            code: "revision-changed".into(),
            message: "observation changed; restart pagination at offset zero".into(),
        });
        return result;
    }
    if request.offset > records.len() {
        result.error = Some(Error {
            code: "invalid-offset".into(),
            message: "offset exceeds observation size".into(),
        });
        return result;
    }
    for record in records.into_iter().skip(request.offset).take(request.limit) {
        result.data.push(record);
        // Reserve space for the final cursor and any error metadata.
        if serde_json::to_vec(&result).map_or(true, |bytes| bytes.len() > MAX_RESPONSE_BYTES - 512)
        {
            result.data.pop();
            if result.data.is_empty() {
                result.error = Some(Error {
                    code: "record-too-large".into(),
                    message: "record exceeds the bounded response size".into(),
                });
            }
            break;
        }
    }
    let next = request.offset + result.data.len();
    result.next_offset = (next < result.total && result.error.is_none()).then_some(next);
    result
}

#[must_use]
pub fn discovery() -> Vec<Value> {
    vec![
        json!({"id": "brood.introspection.request.v1", "version": 1, "request": {"version": 1, "collection": "discovery", "offset": 0, "limit": 20, "revision": null}, "collections": ["discovery", "installations", "instances", "root-policy", "bindings", "peers", "endpoints", "remote-instances", "failures", "diagnostics", "bundled-description", "documents", "package-description", "package-documents"], "maximum_request_bytes": 4096, "maximum_response_bytes": MAX_RESPONSE_BYTES, "maximum_page_items": 100, "pagination": "Pass revision and next_offset from the previous page. A changed observation returns revision-changed. Endpoint observations are independent, not globally atomic.", "response_schema": SCHEMA_ID, "host_interfaces": crate::package_runtime::contract::HOST_INTERFACES, "resource_expectation_units": crate::package_runtime::contract::RESOURCE_UNITS, "schemas": schemas(), "schema_documents": generated_schemas().keys().collect::<Vec<_>>()}),
    ]
}

pub fn records(
    request: &Request,
    instances: &Value,
    catalogue: &Value,
    services: &Value,
) -> Result<Vec<Value>, String> {
    let collection = request.collection;
    if collection == Collection::Discovery {
        return Ok(discovery());
    }
    let data = match collection {
        Collection::Discovery => return Ok(discovery()),
        Collection::BundledDescription | Collection::Documents => return bundled_records(request),
        Collection::PackageDescription | Collection::PackageDocuments => {
            return package_records(request, catalogue);
        }
        Collection::Instances => instances.clone(),
        Collection::Diagnostics => json!(diagnostic_records(instances)),
        Collection::Installations => catalogue
            .get("installations")
            .unwrap_or(&Value::Null)
            .clone(),
        Collection::RootPolicy => catalogue.get("root_policy").unwrap_or(&Value::Null).clone(),
        Collection::Failures => catalogue.get("failures").unwrap_or(&Value::Null).clone(),
        Collection::Bindings => services.get("bindings").unwrap_or(&Value::Null).clone(),
        Collection::Peers => services.get("peers").unwrap_or(&Value::Null).clone(),
        Collection::Endpoints => services.get("endpoints").unwrap_or(&Value::Null).clone(),
        Collection::RemoteInstances => services.get("instances").unwrap_or(&Value::Null).clone(),
    };
    data.as_array()
        .cloned()
        .ok_or_else(|| "this endpoint does not support the requested observation".into())
}

fn strip_observation_clock(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("observed_at_unix_ms");
            object.remove("observation_age_ms");
            object.remove("resources_observed_at_unix_ms");
            for value in object.values_mut() {
                strip_observation_clock(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_observation_clock(value);
            }
        }
        _ => {}
    }
}

pub fn snapshot_query(endpoint: &str, bytes: &[u8], snapshot: &Value) -> Response {
    let request = Request::decode(bytes);
    let data = request.as_ref().map_err(Clone::clone).and_then(|request| {
        records(
            request,
            snapshot.get("instances").unwrap_or(&Value::Null),
            snapshot.get("catalogue").unwrap_or(&Value::Null),
            snapshot.get("services").unwrap_or(&Value::Null),
        )
    });
    response(
        endpoint,
        "host-observed; remote records retain endpoint-reported authority",
        request,
        data,
    )
}

pub fn offline_query(store: &std::path::Path, bytes: &[u8]) -> Response {
    let request = Request::decode(bytes);
    let records = request.as_ref().map_err(Clone::clone).and_then(|request| match request.collection {
        Collection::Discovery => Ok(discovery()),
        Collection::BundledDescription | Collection::Documents => bundled_records(request),
        Collection::PackageDescription | Collection::PackageDocuments => {
            let package = request.package.as_deref().ok_or("package required")?;
            let revision = request.installation.as_deref().ok_or("installation required")?;
            let installed = brood_package_manifest::installation::inspect_revision(store, package, revision).map_err(|error| error.to_string())?;
            let knowledge = brood_package_manifest::installation::knowledge_revision(store, package, revision).map_err(|error| error.to_string())?;
            package_records(request, &json!({"knowledge": knowledge, "knowledge_digest": installed.knowledge_digest}))
        }
        Collection::Installations | Collection::Failures => {
            let catalogue = brood_package_manifest::catalogue::load(store).map_err(|error| error.to_string())?;
            if request.collection == Collection::Failures { return Ok(catalogue.failures.into_iter().map(Value::String).collect()); }
            Ok(catalogue.packages.iter().map(|package| json!({"installation": package.installation, "enabled": package.enabled, "destination_authorization": package.authorization, "state": package.state})).collect())
        }
        _ => Err("live observation requires a connected endpoint".into()),
    });
    let bundled = request.as_ref().is_ok_and(|request| {
        matches!(
            request.collection,
            Collection::BundledDescription | Collection::Documents
        )
    });
    response(
        &if bundled {
            "querying-binary".into()
        } else {
            format!("local-store:{}", store.display())
        },
        if bundled {
            "answering-executable-bundled-description"
        } else {
            "local-verified-installation"
        },
        request,
        records,
    )
}

/// Bundled facts belong to the answering executable and require no live guest.
struct BundledRecords {
    description: Vec<Value>,
    documents: Vec<Value>,
    searchable: std::collections::BTreeMap<String, String>,
}

fn bundled_records(request: &Request) -> Result<Vec<Value>, String> {
    static BUNDLE: std::sync::OnceLock<Result<BundledRecords, String>> = std::sync::OnceLock::new();
    let bundle = BUNDLE.get_or_init(|| {
        let mut bundle = crate::knowledge::bundle().map_err(|error| error.to_string())?;
        let metadata = bundle.as_object_mut().ok_or("invalid bundled knowledge")?;
        let mut entries = metadata.remove("entries").and_then(|value| value.as_array().cloned()).ok_or("missing bundled documents")?;
        if let Some(reference) = metadata.remove("command_reference") {
            entries.push(json!({"id": "brood.commands.v1", "category": "generated-contract", "content": serde_json::to_string_pretty(&reference).map_err(|error| error.to_string())?}));
        }
        metadata.insert("authority".into(), json!("answering-executable-bundled-description"));
        metadata.insert("command_reference_document".into(), json!("brood.commands.v1"));
        metadata.insert("document_count".into(), json!(entries.len()));
        let bundle_digest = metadata.get("content_digest").cloned().unwrap_or(Value::Null);
        let mut documents = Vec::new();
        let mut searchable = std::collections::BTreeMap::new();
        for entry in entries {
            let content = entry.get("content").and_then(Value::as_str).ok_or("document content unavailable")?;
            let id = entry.get("id").and_then(Value::as_str).ok_or("document ID unavailable")?;
            searchable.insert(id.to_owned(), format!("{}\n{}", id.to_lowercase(), content.to_lowercase()));
            let chunks = document_chunks(content);
            let content_digest = brood_package_manifest::installation::digest(content.as_bytes());
            for (part, chunk) in chunks.iter().enumerate() {
                documents.push(json!({"id": id, "category": entry.get("category"), "part": part, "parts": chunks.len(), "content": chunk, "content_digest": content_digest, "bundle_digest": bundle_digest, "authority": "answering-executable-bundled-description"}));
            }
        }
        Ok(BundledRecords { description: vec![bundle], documents, searchable })
    }).as_ref().map_err(Clone::clone)?;
    if request.collection == Collection::BundledDescription {
        return Ok(bundle.description.clone());
    }
    let needle = request.search.as_ref().map(|value| value.to_lowercase());
    // Search matches whole documents; all chunks of a matching document remain
    // retrievable, rather than silently omitting its nonmatching portions.
    let matched: std::collections::BTreeSet<_> = bundle
        .searchable
        .iter()
        .filter(|(_, content)| {
            needle
                .as_ref()
                .is_none_or(|needle| content.contains(needle))
        })
        .map(|(id, _)| id.as_str())
        .collect();
    let result: Vec<_> = bundle
        .documents
        .iter()
        .filter(|entry| {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
            request.document.as_ref().is_none_or(|wanted| wanted == id) && matched.contains(id)
        })
        .cloned()
        .collect();
    if request.document.is_some() && result.is_empty() && request.search.is_none() {
        return Err("unknown bundled document ID".into());
    }
    Ok(result)
}

fn package_records(request: &Request, catalogue: &Value) -> Result<Vec<Value>, String> {
    let knowledge = catalogue.get("knowledge").ok_or("exact installation knowledge unavailable at this endpoint; historical revisions can be queried from the local store")?;
    if knowledge.get("plan_digest").and_then(Value::as_str) != request.installation.as_deref()
        || knowledge
            .pointer("/manifest/package/id")
            .and_then(Value::as_str)
            != request.package.as_deref()
    {
        return Err("knowledge does not match the requested package installation".into());
    }
    let identity = json!({"package": request.package, "installation": request.installation, "knowledge_digest": catalogue.get("knowledge_digest"), "artifacts": knowledge.get("artifacts")});
    if request.collection == Collection::PackageDescription {
        return Ok(vec![
            json!({"identity": identity, "authority": "verified-installed-knowledge", "purpose": knowledge.pointer("/manifest/extension/purpose"), "source_revision": knowledge.pointer("/manifest/extension/source-revision"), "dependencies": knowledge.get("dependencies"), "prebuilt": knowledge.get("prebuilt"), "manifest_document": "manifest", "documents_collection": "package-documents"}),
        ]);
    }
    let mut entries = knowledge
        .get("documents")
        .and_then(Value::as_array)
        .cloned()
        .ok_or("installed documents unavailable")?;
    let mut structural = vec![(
        "manifest".to_owned(),
        knowledge
            .get("manifest")
            .cloned()
            .ok_or("installed manifest unavailable")?,
    )];
    for (key, prefix) in [
        ("schemas", "schema:interface:"),
        ("diagnostic_schemas", "schema:diagnostic:"),
    ] {
        if let Some(schemas) = knowledge.get(key).and_then(Value::as_object) {
            structural.extend(
                schemas
                    .iter()
                    .map(|(id, schema)| (format!("{prefix}{id}"), schema.clone())),
            );
        }
    }
    if let Some(schema) = knowledge
        .get("configuration_schema")
        .filter(|value| !value.is_null())
    {
        structural.push(("schema:configuration".into(), schema.clone()));
    }
    for (id, value) in structural {
        entries.push(json!({"id": id, "category": "installed-contract", "content": serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?}));
    }
    let needle = request.search.as_ref().map(|value| value.to_lowercase());
    let mut records = Vec::new();
    for entry in entries {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or("installed document ID unavailable")?;
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .ok_or("installed document content unavailable")?;
        if request.document.as_ref().is_some_and(|wanted| wanted != id)
            || needle.as_ref().is_some_and(|needle| {
                !id.to_lowercase().contains(needle) && !content.to_lowercase().contains(needle)
            })
        {
            continue;
        }
        let chunks = document_chunks(content);
        let digest = brood_package_manifest::installation::digest(content.as_bytes());
        for (part, content) in chunks.iter().enumerate() {
            records.push(json!({"identity": identity, "authority": "verified-installed-knowledge", "id": id, "category": entry.get("category"), "references": entry.get("references"), "schemas": entry.get("schemas"), "part": part, "parts": chunks.len(), "content": content, "content_digest": digest}));
        }
    }
    if records.is_empty() && request.document.is_some() && request.search.is_none() {
        return Err("unknown installed document ID".into());
    }
    Ok(records)
}

fn document_chunks(mut content: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    while !content.is_empty() {
        let mut end = content.len().min(4096);
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, remaining) = content.split_at(end);
        chunks.push(chunk);
        content = remaining;
    }
    if chunks.is_empty() {
        chunks.push("");
    }
    chunks
}

fn generated_schema<T: schemars::JsonSchema>(id: &str) -> Value {
    let mut schema = json!(schemars::schema_for!(T));
    if let Some(object) = schema.as_object_mut() {
        object.insert("$id".into(), json!(id));
        object.insert("x-rust-type".into(), json!(std::any::type_name::<T>()));
    }
    schema
}

/// Structural contracts are generated from the same serializable types used
/// by the running hosts. Decoder and authority checks remain semantic gates.
#[must_use]
pub fn generated_schemas() -> std::collections::BTreeMap<String, Value> {
    macro_rules! generate { ($($id:literal, $source:literal, $ty:ty;)*) => {
        [$({
            let mut schema = generated_schema::<$ty>($id);
            if let Some(object) = schema.as_object_mut() { object.insert("x-source-document".into(), json!($source)); }
            ($id.to_owned(), schema)
        }),*].into_iter().collect()
    }; }
    crate::knowledge_validation::schema_entries!(generate)
}

fn schemas() -> Value {
    json!({"request": generated_schema::<Request>("brood.introspection.request.v1"), "response": generated_schema::<Response>(SCHEMA_ID), "generation": "Rust serde contract types via schemars; runtime semantic validation additionally applies"})
}

fn diagnostic_records(instances: &Value) -> Vec<Value> {
    instances.as_array().into_iter().flatten().flat_map(|record| record.get("diagnostics").and_then(Value::as_object).into_iter().flat_map(move |diagnostics| diagnostics.values().map(move |diagnostic| json!({"realm": record.get("realm"), "scope": record.get("scope"), "actor": record.get("actor"), "generation": record.get("generation"), "lifecycle": record.get("lifecycle"), "identity": record.pointer("/declaration/identity"), "diagnostic": diagnostic})))).collect()
}

/// Guest identity comes from the host's live actor generation, never the query.
/// Cross-endpoint administration is not implied by a local package grant.
pub fn guest_query(
    query: &Value,
    realm: crate::package_runtime::contract::Realm,
    scope: &str,
    instances: &Value,
    catalogue: &Value,
    services: &Value,
) -> Response {
    let request = serde_json::to_vec(query)
        .map_err(|error| error.to_string())
        .and_then(|bytes| Request::decode(&bytes));
    let wanted_realm = json!(realm);
    let within_scope = |record: &Value| {
        record.get("realm") == Some(&wanted_realm)
            && record.get("scope").and_then(Value::as_str) == Some(scope)
    };
    let visible: Vec<_> = instances
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| within_scope(record))
        .cloned()
        .collect();
    let packages: Vec<_> = visible
        .iter()
        .filter_map(|record| {
            record
                .pointer("/declaration/identity/package")
                .and_then(Value::as_str)
        })
        .collect();
    let data = request.as_ref().map_err(Clone::clone).and_then(|request| {
        let list = |source: &Value, key: &str| source.get(key).and_then(Value::as_array).cloned().ok_or_else(|| "observation unavailable".to_string());
        match request.collection {
            Collection::Discovery => Ok(discovery()),
        Collection::BundledDescription | Collection::Documents => bundled_records(request),
            Collection::PackageDescription | Collection::PackageDocuments => {
                if !visible.iter().any(|record| record.pointer("/declaration/identity/package").and_then(Value::as_str) == request.package.as_deref()
                    && record.pointer("/declaration/identity/installation").and_then(Value::as_str) == request.installation.as_deref()) {
                    return Err("package installation is outside the caller's realm and scope".into());
                }
                package_records(request, catalogue)
            }
            Collection::Instances => Ok(visible.clone()),
            Collection::Diagnostics => Ok(diagnostic_records(&json!(visible))),
            Collection::Installations => Ok(list(catalogue, "installations")?.into_iter().filter(|record| record.pointer("/installation/package_id").and_then(Value::as_str).is_some_and(|package| packages.contains(&package))).collect()),
            Collection::RootPolicy => Ok(list(catalogue, "root_policy")?.into_iter().filter(&within_scope).collect()),
            Collection::Peers => Ok(list(services, "peers")?.into_iter().filter(|record| record.get("authority").and_then(Value::as_str) == Some("host-observed") && record.get("peer").is_some_and(&within_scope)).collect()),
            Collection::Bindings => {
                let endpoint = services.get("endpoint");
                Ok(list(services, "bindings")?.into_iter().filter(|binding| ["consumer", "provider"].iter().any(|role| binding.get(role).is_some_and(|peer| within_scope(peer) && peer.get("endpoint") == endpoint))).collect())
            }
            Collection::Failures => Ok(visible.iter().flat_map(|record| record.get("recent_failures").and_then(Value::as_array).into_iter().flatten().map(|failure| json!({"actor": record.get("actor"), "failure": failure}))).collect()),
            Collection::Endpoints | Collection::RemoteInstances => Err("cross-endpoint observation requires operator authority; bound peers are described by the bindings collection".into()),
        }
    });
    response(
        services
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or("host:unavailable"),
        &format!("host-observed; caller realm {realm:?}, scope {scope}"),
        request,
        data,
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    reason = "test fixtures use owned revisions and fail immediately on invalid setup"
)]
mod tests {
    use super::*;
    fn request(offset: usize, revision: Option<String>) -> Result<Request, String> {
        Request::decode(serde_json::to_string(&json!({"version": 1, "collection": "instances", "offset": offset, "limit": 2, "revision": revision})).unwrap().as_bytes())
    }
    #[test]
    fn generated_schemas_validate_the_actual_contract_envelopes() {
        let schemas = generated_schemas();
        for schema in schemas.values() {
            jsonschema::validator_for(schema).unwrap();
        }
        let request_schema =
            jsonschema::validator_for(&schemas["brood.introspection.request.v1"]).unwrap();
        let request = json!({"version": 1, "collection": "package-documents", "package": "example", "installation": "revision", "document": "overview", "limit": 1});
        assert!(request_schema.is_valid(&request));
        assert!(Request::decode(&serde_json::to_vec(&request).unwrap()).is_ok());
        for invalid in [
            json!({"version": 2, "collection": "instances"}),
            json!({"version": 1, "collection": "unknown"}),
            json!({"version": 1, "collection": "instances", "limit": 101}),
        ] {
            assert!(!request_schema.is_valid(&invalid));
            assert!(Request::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let page = response(
            "test",
            "host-observed",
            Request::decode(br#"{"version":1,"collection":"instances"}"#),
            Ok(vec![]),
        );
        assert!(
            jsonschema::validator_for(&schemas[SCHEMA_ID])
                .unwrap()
                .is_valid(&serde_json::to_value(&page).unwrap())
        );
        let event = crate::package_runtime::contract::services::Event::Inspection {
            request_id: 1,
            response: Box::new(page),
        };
        assert!(
            jsonschema::validator_for(&schemas["brood.package-service-event.v1"])
                .unwrap()
                .is_valid(&serde_json::to_value(event).unwrap())
        );
        let discovery = response(
            "test",
            "host-observed",
            Request::decode(br#"{"version":1,"collection":"discovery"}"#),
            Ok(discovery()),
        );
        assert!(discovery.error.is_none());
        assert!(
            !discovery.data.is_empty(),
            "generated discovery must fit the mailbox bound"
        );
        assert!(serde_json::to_vec(&discovery).unwrap().len() < MAX_RESPONSE_BYTES);
    }

    #[test]
    fn installed_knowledge_requires_the_exact_visible_installation() {
        let knowledge = json!({"plan_digest": "revision-a", "manifest": {"package": {"id": "example"}}, "documents": [{"id": "overview", "category": "reference", "content": "private scoped documentation"}]});
        let catalogue = json!({"knowledge": knowledge, "knowledge_digest": "digest-a"});
        let instances = json!([{"realm": "surface-session", "scope": "one", "declaration": {"identity": {"package": "example", "installation": "revision-a"}}}]);
        let query = json!({"version": 1, "collection": "package-documents", "package": "example", "installation": "revision-a", "document": "overview"});
        let realm = crate::package_runtime::contract::Realm::SurfaceSession;
        let allowed = guest_query(&query, realm, "one", &instances, &catalogue, &Value::Null);
        assert!(allowed.error.is_none());
        assert_eq!(allowed.data[0]["content"], "private scoped documentation");
        assert_eq!(allowed.data[0]["identity"]["knowledge_digest"], "digest-a");
        let denied = guest_query(&query, realm, "two", &instances, &catalogue, &Value::Null);
        assert_eq!(denied.total, 0);
        assert!(denied.revision.is_none());
        assert!(denied.error.unwrap().message.contains("outside"));
        let mut changed = catalogue;
        changed["knowledge"]["plan_digest"] = json!("revision-b");
        let stale = guest_query(&query, realm, "one", &instances, &changed, &Value::Null);
        assert!(stale.error.unwrap().message.contains("does not match"));
        assert!(
            Request::decode(
                br#"{"version":1,"collection":"package-documents","package":"example"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn bundled_documents_reconstruct_the_exact_offline_source_through_common_pages() {
        let id = "crates/brood-wasm/wit/brood-plugin.wit";
        let embedded = crate::knowledge::bundle().unwrap();
        let expected = embedded["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap();
        let mut offset = 0;
        let mut revision = None;
        let mut reconstructed = String::new();
        loop {
            let query = json!({"version": 1, "collection": "documents", "document": id, "limit": 1, "offset": offset, "revision": revision});
            let result = guest_query(
                &query,
                crate::package_runtime::contract::Realm::PresentationClient,
                "test",
                &Value::Null,
                &Value::Null,
                &json!({"endpoint": "fixture"}),
            );
            assert!(result.error.is_none());
            assert_eq!(result.endpoint, "fixture");
            assert!(serde_json::to_vec(&result).unwrap().len() < MAX_RESPONSE_BYTES);
            let chunk = &result.data[0];
            assert_eq!(chunk["id"], id);
            assert_eq!(chunk["part"], offset);
            reconstructed.push_str(chunk["content"].as_str().unwrap());
            revision = result.revision;
            match result.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert!(offset > 0, "fixture must exercise multiple pages");
        assert_eq!(reconstructed, expected);
        let invalid =
            Request::decode(br#"{"version":1,"collection":"instances","document":"private"}"#);
        assert!(invalid.is_err());
        assert_eq!(
            document_chunks(&"é".repeat(5000)).concat(),
            "é".repeat(5000)
        );
        let query = Request::decode(br#"{"version":1,"collection":"documents","document":"brood.commands.v1","search":"subcommands"}"#).unwrap();
        let commands = bundled_records(&query).unwrap();
        let text: String = commands
            .iter()
            .map(|chunk| chunk["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            embedded["command_reference"]
        );
    }

    #[test]
    fn pages_detect_changes_without_treating_observation_age_as_a_revision() {
        let records = vec![json!({"id": 1}), json!({"id": 2}), json!({"id": 3})];
        let first = response(
            "host",
            "host-observed",
            request(0, None),
            Ok(records.clone()),
        );
        assert_eq!(first.next_offset, Some(2));
        let second = response(
            "host",
            "host-observed",
            request(2, first.revision.clone()),
            Ok(records),
        );
        assert_eq!(second.data, vec![json!({"id": 3})]);
        assert_eq!(second.next_offset, None);
        let changed = response(
            "host",
            "host-observed",
            request(2, first.revision),
            Ok(vec![json!({"id": 4})]),
        );
        assert_eq!(changed.error.unwrap().code, "revision-changed");
        let before = response(
            "host",
            "host-observed",
            request(0, None),
            Ok(vec![json!({"id": 1, "observation_age_ms": 10})]),
        );
        let after = response(
            "host",
            "host-observed",
            request(0, before.revision),
            Ok(vec![json!({"id": 1, "observation_age_ms": 20})]),
        );
        assert!(after.error.is_none());
    }
    #[test]
    fn guest_visibility_is_scoped_before_pagination_and_revisions() {
        use crate::package_runtime::contract::Realm;
        let visible = json!({"realm": "surface-session", "scope": "one", "actor": "allowed", "declaration": {"identity": {"package": "a"}}});
        let hidden = json!({"realm": "surface-session", "scope": "two", "actor": "hidden"});
        let catalogue = json!({"installations": [{"installation": {"package_id": "a"}}, {"installation": {"package_id": "secret"}}]});
        let services = json!({"endpoint": "host"});
        let query = json!({"version": 1, "collection": "instances", "limit": 1});
        let first = guest_query(
            &query,
            Realm::SurfaceSession,
            "one",
            &json!([visible, hidden]),
            &catalogue,
            &services,
        );
        assert_eq!(first.total, 1);
        assert_eq!(first.data, vec![visible.clone()]);
        let second = guest_query(
            &query,
            Realm::SurfaceSession,
            "one",
            &json!([visible, {"realm": "daemon", "scope": "daemon", "actor": "changed"}]),
            &catalogue,
            &services,
        );
        assert_eq!(first.revision, second.revision);
        let installs = guest_query(
            &json!({"version": 1, "collection": "installations"}),
            Realm::SurfaceSession,
            "one",
            &json!([visible]),
            &catalogue,
            &services,
        );
        assert_eq!(installs.total, 1);
        let denied = guest_query(
            &json!({"version": 1, "collection": "endpoints"}),
            Realm::SurfaceSession,
            "one",
            &json!([]),
            &catalogue,
            &services,
        );
        assert_eq!(denied.error.unwrap().code, "observation-unavailable");
    }

    #[test]
    fn response_is_byte_bounded_and_unsupported_is_not_an_empty_success() {
        let large = json!({"text": "x".repeat(30_000)});
        let page = response(
            "host",
            "host-observed",
            request(0, None),
            Ok(vec![large.clone(), large]),
        );
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.next_offset, Some(1));
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_RESPONSE_BYTES);
        let unsupported = response(
            "host",
            "host-observed",
            request(0, None),
            Err("not observable".into()),
        );
        assert_eq!(unsupported.error.unwrap().code, "observation-unavailable");
        let invalid = response("host", "host-observed", Request::decode(b"{}"), Ok(vec![]));
        assert_eq!(invalid.error.unwrap().code, "invalid-request");
    }
}
