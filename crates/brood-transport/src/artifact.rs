//! Symmetric content-addressed artifact exchange.
//!
//! This module owns bounded transfer and cache integrity. Execution authority
//! remains with the destination runtime: receiving bytes never grants a
//! capability or installs an actor by itself.

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow, ensure};
use brood_protocol::{
    ARTIFACT_ABI_VERSION, ArtifactDigest, ArtifactKind, ArtifactManifest, ArtifactMessage,
    ArtifactPackageMember, ArtifactRealm, MAX_ARTIFACT_CHUNK_BYTES,
};
use sha2::{Digest, Sha256};

const MAX_INCOMING_ARTIFACTS: usize = 4;
const MAX_INCOMING_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PACKAGE_MEMBERS: usize = 256;
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// One locally available artifact offered to a peer.
#[derive(Clone, Debug)]
pub struct ArtifactOffer {
    pub manifest: ArtifactManifest,
    path: PathBuf,
}

/// Build offers for every WASM member beneath one package root directory.
pub fn package_offers(spec: &str, realm: ArtifactRealm) -> Result<Vec<ArtifactOffer>> {
    let (package, root_path) = spec
        .split_once('=')
        .context("artifact package must use ID=ROOT_WASM_PATH")?;
    let root_path = Path::new(root_path)
        .canonicalize()
        .with_context(|| format!("resolve artifact package root {root_path}"))?;
    let package_root = root_path
        .parent()
        .context("artifact package root file has no parent")?
        .to_path_buf();
    let mut files = Vec::new();
    collect_wasm_files(&package_root, &mut files)?;
    files.sort();
    ensure!(
        files.len() <= MAX_PACKAGE_MEMBERS,
        "artifact package contains more than {MAX_PACKAGE_MEMBERS} WASM members"
    );
    ensure!(
        files.iter().any(|path| path == &root_path),
        "artifact package root is not a WASM file beneath its package directory"
    );
    let mut offers = Vec::with_capacity(files.len());
    for path in files {
        let relative = path
            .strip_prefix(&package_root)
            .context("artifact package member escaped its root")?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let digest = digest_file(&path)?;
        let digest_prefix = digest_hex(digest).chars().take(32).collect::<String>();
        let id = format!("member-{digest_prefix}");
        offers.push(
            ArtifactOffer::from_path(&id, realm, &path, Vec::new())?.in_package(
                package,
                &relative,
                path == root_path,
            )?,
        );
    }
    Ok(offers)
}

fn collect_wasm_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(root).with_context(|| format!("read artifact package {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("read artifact package {}", root.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!("inspect artifact package member {}", entry.path().display())
        })?;
        if file_type.is_dir() && entry.file_name() == "deps" {
            // A source-checkout package root commonly points into Cargo's
            // `target/<triple>/release` directory. The adjacent `deps`
            // directory contains compiler copies of those same WASM files,
            // not actor package members; cataloguing both would advertise one
            // content digest twice and reject the connection.
            continue;
        }
        if file_type.is_dir() {
            collect_wasm_files(&entry.path(), files)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == "wasm")
        {
            files.push(entry.path().canonicalize().with_context(|| {
                format!("resolve artifact package member {}", entry.path().display())
            })?);
        }
    }
    Ok(())
}

impl ArtifactOffer {
    /// Parse `ID=PATH`, hash the file, and declare its destination realm.
    pub fn parse(spec: &str, realm: ArtifactRealm) -> Result<Self> {
        let (id, path) = spec
            .split_once('=')
            .context("artifact offer must use ID=PATH")?;
        Self::from_path(id, realm, Path::new(path), Vec::new())
    }

    /// Hash and validate one local executable artifact.
    pub fn from_path(
        id: &str,
        realm: ArtifactRealm,
        path: &Path,
        capabilities: Vec<String>,
    ) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve offered artifact {}", path.display()))?;
        let bytes = fs::metadata(&path)
            .with_context(|| format!("inspect offered artifact {}", path.display()))?
            .len();
        let manifest = ArtifactManifest {
            id: id.to_string(),
            realm,
            kind: ArtifactKind::WasmActor,
            digest: digest_file(&path)?,
            bytes,
            abi_version: ARTIFACT_ABI_VERSION,
            capabilities,
            package: None,
        };
        manifest
            .validate()
            .context("validate offered artifact manifest")?;
        Ok(Self { manifest, path })
    }

    /// Declare this offer as one member of a lazily hydrated package.
    pub fn in_package(mut self, package: &str, path: &str, root: bool) -> Result<Self> {
        self.manifest.package = Some(ArtifactPackageMember {
            package: package.to_string(),
            path: path.to_string(),
            root,
        });
        self.manifest.validate()?;
        Ok(self)
    }
}

/// A verified artifact made available to the destination runtime.
#[derive(Clone, Debug)]
pub struct InstalledArtifact {
    pub manifest: ArtifactManifest,
    pub path: PathBuf,
}

/// Observable completion or rejection from the symmetric exchange.
#[derive(Clone, Debug)]
pub enum ArtifactExchangeEvent {
    Installed(InstalledArtifact),
    PeerAccepted(ArtifactManifest),
    PeerRejected {
        manifest: Option<ArtifactManifest>,
        digest: ArtifactDigest,
        reason: String,
    },
}

/// State-machine output after handling one peer message.
#[derive(Default)]
pub struct ArtifactExchangeUpdate {
    pub outbound: Vec<ArtifactMessage>,
    pub events: Vec<ArtifactExchangeEvent>,
}

/// Per-connection artifact transfer state.
pub struct ArtifactExchange {
    accepted_realm: ArtifactRealm,
    allowed_ids: HashSet<String>,
    allowed_packages: HashSet<String>,
    cache: ArtifactCache,
    offers: HashMap<ArtifactDigest, ArtifactOffer>,
    served: HashSet<ArtifactDigest>,
    incoming: HashMap<ArtifactDigest, IncomingArtifact>,
    incoming_bytes: u64,
    ready: HashSet<ArtifactDigest>,
    accepted: HashSet<ArtifactDigest>,
    available: HashMap<ArtifactDigest, ArtifactManifest>,
    package_members: HashMap<(String, String), ArtifactDigest>,
    package_roots: HashMap<String, ArtifactDigest>,
}

impl ArtifactExchange {
    /// Construct an exchange with destination-owned authorization.
    pub fn new(
        accepted_realm: ArtifactRealm,
        allowed_ids: impl IntoIterator<Item = String>,
        cache_root: PathBuf,
        offers: Vec<ArtifactOffer>,
    ) -> Result<Self> {
        let mut offer_ids = HashSet::new();
        let mut offer_map = HashMap::new();
        for offer in offers {
            offer
                .manifest
                .validate()
                .context("validate local artifact offer")?;
            ensure!(
                offer_ids.insert(offer.manifest.id.clone()),
                "duplicate artifact offer id {:?}",
                offer.manifest.id
            );
            ensure!(
                offer_map.insert(offer.manifest.digest, offer).is_none(),
                "duplicate artifact offer content digest"
            );
        }
        Ok(Self {
            accepted_realm,
            allowed_ids: allowed_ids.into_iter().collect(),
            allowed_packages: HashSet::new(),
            cache: ArtifactCache::new(cache_root),
            offers: offer_map,
            served: HashSet::new(),
            incoming: HashMap::new(),
            incoming_bytes: 0,
            ready: HashSet::new(),
            accepted: HashSet::new(),
            available: HashMap::new(),
            package_members: HashMap::new(),
            package_roots: HashMap::new(),
        })
    }

    /// Authorize package members offered by the authenticated peer.
    #[must_use]
    pub fn with_allowed_packages(mut self, packages: impl IntoIterator<Item = String>) -> Self {
        self.allowed_packages.extend(packages);
        self
    }

    /// Advertise every local offer after the network lane is established.
    #[must_use]
    pub fn initial_messages(&self) -> Vec<ArtifactMessage> {
        let mut manifests = self
            .offers
            .values()
            .map(|offer| offer.manifest.clone())
            .collect::<Vec<_>>();
        manifests.sort_by(|left, right| left.id.cmp(&right.id));
        manifests.into_iter().map(ArtifactMessage::Offer).collect()
    }

    /// Validate and apply one peer message.
    pub fn handle(&mut self, message: ArtifactMessage) -> Result<ArtifactExchangeUpdate> {
        message
            .validate()
            .context("validate artifact exchange message")?;
        match message {
            ArtifactMessage::Offer(manifest) => self.handle_offer(manifest),
            ArtifactMessage::Request { digest } => self.handle_request(digest),
            ArtifactMessage::Chunk {
                digest,
                offset,
                bytes,
            } => Ok(self.handle_chunk(digest, offset, bytes)),
            ArtifactMessage::Complete { digest } => self.handle_complete(digest),
            ArtifactMessage::Accepted { digest } => Ok(self.handle_accepted(digest)),
            ArtifactMessage::Rejected { digest, reason } => {
                Ok(self.handle_rejected(digest, reason))
            }
        }
    }

    /// Report the destination runtime's installation result and produce the
    /// peer acknowledgement. Verified bytes are not accepted before this.
    pub fn finish_installation(
        &mut self,
        digest: ArtifactDigest,
        result: std::result::Result<(), String>,
    ) -> Result<ArtifactMessage> {
        ensure!(
            self.ready.remove(&digest),
            "artifact installation completion is not pending"
        );
        Ok(match result {
            Ok(()) => {
                self.accepted.insert(digest);
                ArtifactMessage::Accepted { digest }
            }
            Err(reason) => ArtifactMessage::Rejected {
                digest,
                reason: bounded_rejection_reason(reason),
            },
        })
    }

    /// Request a previously advertised member when the destination package
    /// resolver discovers that its local file is absent.
    pub fn request_package_member(
        &mut self,
        package: &str,
        path: &str,
    ) -> Result<ArtifactExchangeUpdate> {
        let digest = self
            .package_members
            .get(&(package.to_string(), path.to_string()))
            .copied()
            .with_context(|| {
                format!("peer did not advertise package member {package:?}:{path:?}")
            })?;
        let manifest = self
            .available
            .get(&digest)
            .cloned()
            .context("advertised package member lost its manifest")?;
        self.begin_receive(manifest)
    }

    /// Resolve a package path to its advertised content digest.
    #[must_use]
    pub fn package_member_digest(&self, package: &str, path: &str) -> Option<ArtifactDigest> {
        self.package_members
            .get(&(package.to_string(), path.to_string()))
            .copied()
    }

    fn handle_offer(&mut self, manifest: ArtifactManifest) -> Result<ArtifactExchangeUpdate> {
        if manifest.realm != self.accepted_realm {
            return Ok(rejection(
                manifest.digest,
                format!(
                    "artifact targets {:?}; this endpoint accepts {:?}",
                    manifest.realm, self.accepted_realm
                ),
            ));
        }
        let package_allowed = manifest
            .package
            .as_ref()
            .is_some_and(|member| self.allowed_packages.contains(&member.package));
        if !self.allowed_ids.contains(&manifest.id) && !package_allowed {
            return Ok(rejection(
                manifest.digest,
                format!("artifact id {:?} is not authorized", manifest.id),
            ));
        }
        if !manifest.capabilities.is_empty() {
            return Ok(rejection(
                manifest.digest,
                "network artifact capability grants are not configured".to_string(),
            ));
        }
        if let Some(existing) = self.available.get(&manifest.digest) {
            if existing != &manifest {
                return Ok(rejection(
                    manifest.digest,
                    "artifact digest was offered with conflicting metadata".to_string(),
                ));
            }
            return Ok(ArtifactExchangeUpdate::default());
        }
        if let Some(member) = &manifest.package {
            let key = (member.package.clone(), member.path.clone());
            if self.package_members.contains_key(&key) {
                return Ok(rejection(
                    manifest.digest,
                    "package path was advertised more than once".to_string(),
                ));
            }
            if member.root && self.package_roots.contains_key(&member.package) {
                return Ok(rejection(
                    manifest.digest,
                    "package advertised more than one init root".to_string(),
                ));
            }
            self.package_members.insert(key, manifest.digest);
            if member.root {
                self.package_roots
                    .insert(member.package.clone(), manifest.digest);
            }
        }
        self.available.insert(manifest.digest, manifest.clone());
        if manifest.package.as_ref().is_some_and(|member| !member.root) {
            return Ok(ArtifactExchangeUpdate::default());
        }
        self.begin_receive(manifest)
    }

    fn begin_receive(&mut self, manifest: ArtifactManifest) -> Result<ArtifactExchangeUpdate> {
        if self.accepted.contains(&manifest.digest) {
            return Ok(ArtifactExchangeUpdate {
                outbound: vec![ArtifactMessage::Accepted {
                    digest: manifest.digest,
                }],
                events: Vec::new(),
            });
        }
        if self.ready.contains(&manifest.digest) {
            return Ok(ArtifactExchangeUpdate::default());
        }
        if let Some(path) = self.cache.lookup(&manifest)? {
            self.ready.insert(manifest.digest);
            return Ok(ArtifactExchangeUpdate {
                outbound: Vec::new(),
                events: vec![ArtifactExchangeEvent::Installed(InstalledArtifact {
                    manifest,
                    path,
                })],
            });
        }
        if self.incoming.len() >= MAX_INCOMING_ARTIFACTS
            || self.incoming_bytes.saturating_add(manifest.bytes) > MAX_INCOMING_ARTIFACT_BYTES
        {
            return Ok(rejection(
                manifest.digest,
                "artifact receive budget is full".to_string(),
            ));
        }
        if let Some(existing) = self.incoming.get(&manifest.digest) {
            if existing.manifest == manifest {
                return Ok(ArtifactExchangeUpdate::default());
            }
            return Ok(rejection(
                manifest.digest,
                "artifact digest was offered with conflicting metadata".to_string(),
            ));
        }
        self.incoming_bytes = self.incoming_bytes.saturating_add(manifest.bytes);
        let capacity = usize::try_from(manifest.bytes).context("artifact size exceeds usize")?;
        let digest = manifest.digest;
        self.incoming.insert(
            digest,
            IncomingArtifact {
                manifest,
                bytes: Vec::with_capacity(capacity),
            },
        );
        Ok(ArtifactExchangeUpdate {
            outbound: vec![ArtifactMessage::Request { digest }],
            events: Vec::new(),
        })
    }

    fn handle_request(&mut self, digest: ArtifactDigest) -> Result<ArtifactExchangeUpdate> {
        let Some(offer) = self.offers.get(&digest) else {
            return Ok(rejection(
                digest,
                "requested artifact is not offered".to_string(),
            ));
        };
        if !self.served.insert(digest) {
            return Ok(rejection(
                digest,
                "artifact was already served on this connection".to_string(),
            ));
        }
        let bytes = fs::read(&offer.path)
            .with_context(|| format!("read offered artifact {}", offer.path.display()))?;
        if u64::try_from(bytes.len()).ok() != Some(offer.manifest.bytes)
            || digest_bytes(&bytes) != offer.manifest.digest
        {
            return Ok(rejection(
                digest,
                "offered artifact changed after its manifest was created".to_string(),
            ));
        }
        let mut outbound = Vec::with_capacity(
            bytes
                .len()
                .div_ceil(MAX_ARTIFACT_CHUNK_BYTES)
                .saturating_add(1),
        );
        for (index, chunk) in bytes.chunks(MAX_ARTIFACT_CHUNK_BYTES).enumerate() {
            let offset = index
                .checked_mul(MAX_ARTIFACT_CHUNK_BYTES)
                .and_then(|offset| u64::try_from(offset).ok())
                .context("artifact chunk offset overflow")?;
            outbound.push(ArtifactMessage::Chunk {
                digest,
                offset,
                bytes: chunk.to_vec(),
            });
        }
        outbound.push(ArtifactMessage::Complete { digest });
        Ok(ArtifactExchangeUpdate {
            outbound,
            events: Vec::new(),
        })
    }

    fn handle_chunk(
        &mut self,
        digest: ArtifactDigest,
        offset: u64,
        bytes: Vec<u8>,
    ) -> ArtifactExchangeUpdate {
        let Some(incoming) = self.incoming.get_mut(&digest) else {
            return rejection(digest, "artifact chunk was not requested".to_string());
        };
        let expected_offset = u64::try_from(incoming.bytes.len()).unwrap_or(u64::MAX);
        let new_length =
            expected_offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if offset != expected_offset || new_length > incoming.manifest.bytes {
            let removed = self.incoming.remove(&digest);
            if let Some(removed) = removed {
                self.incoming_bytes = self.incoming_bytes.saturating_sub(removed.manifest.bytes);
            }
            return rejection(
                digest,
                "artifact chunk is out of order or exceeds its manifest".to_string(),
            );
        }
        incoming.bytes.extend(bytes);
        ArtifactExchangeUpdate::default()
    }

    fn handle_complete(&mut self, digest: ArtifactDigest) -> Result<ArtifactExchangeUpdate> {
        let Some(incoming) = self.incoming.remove(&digest) else {
            return Ok(rejection(
                digest,
                "artifact completion was not requested".to_string(),
            ));
        };
        self.incoming_bytes = self.incoming_bytes.saturating_sub(incoming.manifest.bytes);
        if u64::try_from(incoming.bytes.len()).ok() != Some(incoming.manifest.bytes)
            || digest_bytes(&incoming.bytes) != incoming.manifest.digest
        {
            return Ok(rejection(
                digest,
                "artifact content does not match its manifest".to_string(),
            ));
        }
        let path = self.cache.store(&incoming.manifest, &incoming.bytes)?;
        self.ready.insert(digest);
        Ok(ArtifactExchangeUpdate {
            outbound: Vec::new(),
            events: vec![ArtifactExchangeEvent::Installed(InstalledArtifact {
                manifest: incoming.manifest,
                path,
            })],
        })
    }

    fn handle_accepted(&self, digest: ArtifactDigest) -> ArtifactExchangeUpdate {
        let events = self
            .offers
            .get(&digest)
            .map(|offer| ArtifactExchangeEvent::PeerAccepted(offer.manifest.clone()))
            .into_iter()
            .collect();
        ArtifactExchangeUpdate {
            outbound: Vec::new(),
            events,
        }
    }

    fn handle_rejected(&self, digest: ArtifactDigest, reason: String) -> ArtifactExchangeUpdate {
        ArtifactExchangeUpdate {
            outbound: Vec::new(),
            events: vec![ArtifactExchangeEvent::PeerRejected {
                manifest: self.offers.get(&digest).map(|offer| offer.manifest.clone()),
                digest,
                reason,
            }],
        }
    }
}

struct IncomingArtifact {
    manifest: ArtifactManifest,
    bytes: Vec<u8>,
}

struct ArtifactCache {
    root: PathBuf,
}

impl ArtifactCache {
    const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn lookup(&self, manifest: &ArtifactManifest) -> Result<Option<PathBuf>> {
        let path = self.path(manifest.digest);
        let Ok(metadata) = fs::metadata(&path) else {
            return Ok(None);
        };
        if metadata.len() != manifest.bytes || digest_file(&path)? != manifest.digest {
            return Ok(None);
        }
        Ok(Some(path))
    }

    fn store(&self, manifest: &ArtifactManifest, bytes: &[u8]) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create artifact cache {}", self.root.display()))?;
        let final_path = self.path(manifest.digest);
        if let Some(path) = self.lookup(manifest)? {
            return Ok(path);
        }
        let artifact_root = final_path
            .parent()
            .context("artifact cache path has no parent")?;
        fs::create_dir_all(artifact_root).with_context(|| {
            format!("create artifact package cache {}", artifact_root.display())
        })?;
        let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path =
            artifact_root.join(format!("artifact.part-{}-{sequence}", std::process::id()));
        let store_result = (|| {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .with_context(|| format!("create artifact cache file {}", temp_path.display()))?;
            file.write_all(bytes)
                .with_context(|| format!("write artifact cache file {}", temp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync artifact cache file {}", temp_path.display()))?;
            fs::rename(&temp_path, &final_path).with_context(|| {
                format!(
                    "publish artifact cache file {} as {}",
                    temp_path.display(),
                    final_path.display()
                )
            })
        })();
        if store_result.is_err() {
            drop(fs::remove_file(&temp_path));
        }
        store_result?;
        Ok(final_path)
    }

    fn path(&self, digest: ArtifactDigest) -> PathBuf {
        self.root.join(digest_hex(digest)).join("artifact.wasm")
    }
}

/// Default per-user content-addressed artifact cache.
#[must_use]
pub fn default_artifact_cache() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map_or_else(
            || {
                env::var_os("HOME").map_or_else(
                    || env::temp_dir().join("brood"),
                    |home| PathBuf::from(home).join(".cache").join("brood"),
                )
            },
            PathBuf::from,
        )
        .join("artifacts")
}

/// Compute the stable digest of an artifact file.
pub fn digest_file(path: &Path) -> Result<ArtifactDigest> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open artifact {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read artifact {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(
            buffer
                .get(..read)
                .ok_or_else(|| anyhow!("artifact read exceeded its buffer"))?,
        );
    }
    Ok(hasher.finalize().into())
}

/// Render a digest for diagnostics and cache names.
#[must_use]
pub fn digest_hex(digest: ArtifactDigest) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(
            HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'?'),
        ));
        output.push(char::from(
            HEX.get(usize::from(byte & 0x0f)).copied().unwrap_or(b'?'),
        ));
    }
    output
}

fn digest_bytes(bytes: &[u8]) -> ArtifactDigest {
    Sha256::digest(bytes).into()
}

fn rejection(digest: ArtifactDigest, reason: String) -> ArtifactExchangeUpdate {
    ArtifactExchangeUpdate {
        outbound: vec![ArtifactMessage::Rejected { digest, reason }],
        events: Vec::new(),
    }
}

fn bounded_rejection_reason(mut reason: String) -> String {
    if reason.len() <= brood_protocol::MAX_ARTIFACT_REJECTION_BYTES {
        return reason;
    }
    let mut end = brood_protocol::MAX_ARTIFACT_REJECTION_BYTES;
    while !reason.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    reason.truncate(end);
    reason
}

#[cfg(test)]
#[allow(
    clippy::unreachable,
    reason = "test infrastructure failures cannot produce a useful fallback value"
)]
mod tests {
    use std::{fs, path::PathBuf};

    use brood_protocol::{ArtifactMessage, ArtifactRealm};

    use super::{ArtifactExchange, ArtifactExchangeEvent, ArtifactOffer, package_offers};

    #[test]
    fn bidirectional_state_machine_transfers_only_authorized_content() {
        let root = test_root("exchange");
        fs::create_dir_all(&root).unwrap_or_else(|error| unreachable!("{error}"));
        let source_path = root.join("source.wasm");
        fs::write(&source_path, b"\0asm symmetric artifact")
            .unwrap_or_else(|error| unreachable!("{error}"));
        let offer =
            ArtifactOffer::from_path("workspace", ArtifactRealm::Client, &source_path, Vec::new())
                .unwrap_or_else(|error| unreachable!("{error}"));
        let mut sender = ArtifactExchange::new(
            ArtifactRealm::Session,
            Vec::<String>::new(),
            root.join("sender-cache"),
            vec![offer],
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let mut receiver = ArtifactExchange::new(
            ArtifactRealm::Client,
            ["workspace".to_string()],
            root.join("receiver-cache"),
            Vec::new(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"));

        let mut to_receiver = sender.initial_messages();
        let mut to_sender = Vec::new();
        let mut installed = None;
        while !to_receiver.is_empty() || !to_sender.is_empty() {
            for message in std::mem::take(&mut to_receiver) {
                let update = receiver
                    .handle(message)
                    .unwrap_or_else(|error| unreachable!("{error}"));
                to_sender.extend(update.outbound);
                for event in update.events {
                    if let ArtifactExchangeEvent::Installed(artifact) = event {
                        to_sender.push(
                            receiver
                                .finish_installation(artifact.manifest.digest, Ok(()))
                                .unwrap_or_else(|error| unreachable!("{error}")),
                        );
                        installed = Some(artifact);
                    }
                }
            }
            for message in std::mem::take(&mut to_sender) {
                let update = sender
                    .handle(message)
                    .unwrap_or_else(|error| unreachable!("{error}"));
                to_receiver.extend(update.outbound);
            }
        }
        let installed = installed.unwrap_or_else(|| unreachable!("artifact was not installed"));
        assert_eq!(
            fs::read(installed.path).unwrap_or_else(|error| unreachable!("{error}")),
            b"\0asm symmetric artifact"
        );

        let unauthorized = ArtifactMessage::Offer(
            ArtifactOffer::from_path("untrusted", ArtifactRealm::Client, &source_path, Vec::new())
                .unwrap_or_else(|error| unreachable!("{error}"))
                .manifest,
        );
        let update = receiver
            .handle(unauthorized)
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert!(matches!(
            update.outbound.as_slice(),
            [ArtifactMessage::Rejected { .. }]
        ));

        let wrong_realm = ArtifactMessage::Offer(
            ArtifactOffer::from_path(
                "workspace",
                ArtifactRealm::Session,
                &source_path,
                Vec::new(),
            )
            .unwrap_or_else(|error| unreachable!("{error}"))
            .manifest,
        );
        assert!(matches!(
            receiver
                .handle(wrong_realm)
                .unwrap_or_else(|error| unreachable!("{error}"))
                .outbound
                .as_slice(),
            [ArtifactMessage::Rejected { .. }]
        ));

        let capability_request = ArtifactMessage::Offer(
            ArtifactOffer::from_path(
                "workspace",
                ArtifactRealm::Client,
                &source_path,
                vec!["filesystem:workspace-read".to_string()],
            )
            .unwrap_or_else(|error| unreachable!("{error}"))
            .manifest,
        );
        assert!(matches!(
            receiver
                .handle(capability_request)
                .unwrap_or_else(|error| unreachable!("{error}"))
                .outbound
                .as_slice(),
            [ArtifactMessage::Rejected { .. }]
        ));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn package_catalog_fetches_root_eagerly_and_child_on_demand() {
        let root = test_root("package");
        fs::create_dir_all(&root).unwrap_or_else(|error| unreachable!("{error}"));
        let init_path = root.join("init.wasm");
        let child_path = root.join("child.wasm");
        fs::write(&init_path, b"\0asm init").unwrap_or_else(|error| unreachable!("{error}"));
        fs::write(&child_path, b"\0asm child").unwrap_or_else(|error| unreachable!("{error}"));
        let offers = package_offers(
            &format!("workspace={}", init_path.display()),
            ArtifactRealm::Session,
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let sender = ArtifactExchange::new(
            ArtifactRealm::Client,
            Vec::<String>::new(),
            root.join("sender-cache"),
            offers,
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let mut receiver = ArtifactExchange::new(
            ArtifactRealm::Session,
            Vec::<String>::new(),
            root.join("receiver-cache"),
            Vec::new(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"))
        .with_allowed_packages(["workspace".to_string()]);

        let mut root_requested = false;
        for message in sender.initial_messages() {
            let is_root = matches!(
                &message,
                ArtifactMessage::Offer(manifest)
                    if manifest.package.as_ref().is_some_and(|member| member.root)
            );
            let update = receiver
                .handle(message)
                .unwrap_or_else(|error| unreachable!("{error}"));
            if is_root {
                root_requested = matches!(
                    update.outbound.as_slice(),
                    [ArtifactMessage::Request { .. }]
                );
            } else {
                assert!(update.outbound.is_empty());
            }
        }
        assert!(root_requested);

        let child_digest = receiver
            .package_member_digest("workspace", "child.wasm")
            .unwrap_or_else(|| unreachable!("child was not catalogued"));
        let update = receiver
            .request_package_member("workspace", "child.wasm")
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            update.outbound,
            vec![ArtifactMessage::Request {
                digest: child_digest
            }]
        );
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn package_catalog_excludes_cargo_dependency_copies() {
        let root = test_root("package-cargo-deps");
        let deps = root.join("deps");
        fs::create_dir_all(&deps).unwrap_or_else(|error| unreachable!("{error}"));
        let init_path = root.join("init.component.wasm");
        fs::write(&init_path, b"\0asm init").unwrap_or_else(|error| unreachable!("{error}"));
        fs::write(deps.join("init.component.wasm"), b"\0asm init")
            .unwrap_or_else(|error| unreachable!("{error}"));

        let offers = package_offers(
            &format!("workspace={}", init_path.display()),
            ArtifactRealm::Session,
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(offers.len(), 1);
        ArtifactExchange::new(
            ArtifactRealm::Client,
            Vec::<String>::new(),
            root.join("cache"),
            offers,
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        drop(fs::remove_dir_all(root));
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("brood-artifact-{name}-{}", std::process::id()))
    }
}
