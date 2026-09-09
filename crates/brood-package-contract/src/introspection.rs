//! Read-only introspection contract shared by operator and guest callers.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_REQUEST_BYTES: usize = 4096;
pub const MAX_RESPONSE_BYTES: usize = 48 * 1024;
pub const SCHEMA_ID: &str = "brood.introspection.response.v1";

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Collection {
    Discovery,
    BundledDescription,
    Documents,
    PackageDescription,
    PackageDocuments,
    Installations,
    Instances,
    RootPolicy,
    Bindings,
    Peers,
    Endpoints,
    RemoteInstances,
    Failures,
    Diagnostics,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub version: u32,
    pub collection: Collection,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 100)))]
    pub limit: usize,
    /// Reject a changed observation rather than silently combine different pages.
    #[serde(default)]
    pub revision: Option<String>,
    /// Exact bundled document ID; supported only by the documents collection.
    #[serde(default)]
    pub document: Option<String>,
    /// Case-insensitive document ID/content search; supported only by documents.
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub installation: Option<String>,
}
const fn default_limit() -> usize {
    20
}
impl Request {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("introspection request exceeds limit".into());
        }
        let request: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if request.version != 1 {
            return Err("unsupported introspection version".into());
        }
        if !(1..=100).contains(&request.limit) {
            return Err("page limit must be between 1 and 100".into());
        }
        if (request.document.is_some() || request.search.is_some())
            && !matches!(
                request.collection,
                Collection::Documents | Collection::PackageDocuments
            )
        {
            return Err("document and search filters require the documents collection".into());
        }
        let package_query = matches!(
            request.collection,
            Collection::PackageDescription | Collection::PackageDocuments
        );
        if package_query != (request.package.is_some() && request.installation.is_some())
            || !package_query && (request.package.is_some() || request.installation.is_some())
        {
            return Err("package knowledge requires both package and exact installation, exclusively on package knowledge collections".into());
        }
        Ok(request)
    }
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: String,
    pub message: String,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub schema: String,
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub version: u32,
    pub endpoint: String,
    pub observed_at_unix_ms: u64,
    pub authority: String,
    pub collection: Option<Collection>,
    pub revision: Option<String>,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total: usize,
    pub data: Vec<Value>,
    pub error: Option<Error>,
}
