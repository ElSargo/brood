//! Bounded synchronous pointer-policy provider support.
//!
//! A pointer policy makes only the immediate routing and capture decision. It
//! may enqueue ordinary actor messages, but it cannot wait for actor work or
//! submit layout state through this lane.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Instance, Linker, Module, Store};

use crate::{
    api::{GuestAllocation, GuestArena, GuestArenaAllocator},
    pointer_policy_link::PointerPolicyExport,
    policy_provider::{
        POLICY_PROVIDER_RESPONSE_ABSTAIN, POLICY_PROVIDER_RESPONSE_ERROR,
        POLICY_PROVIDER_RESPONSE_OK, PolicyProviderDeclaration, PolicyProviderKind,
        PolicyProviderResponse, PublicationFrameLease, WasmPolicyProvider,
    },
    resize::InteractionId,
    resolved_layout::{LayoutItemId, LayoutLeaseToken, LocalPoint},
};

pub const DEFAULT_POINTER_POLICY_ARENA_BYTES: usize = 128 * 1024;
pub const MAX_POINTER_POLICY_ARENA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_POINTER_POLICY_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_POINTER_POLICY_RESULT_BYTES: usize = 64 * 1024;
pub const MAX_POINTER_POLICY_MESSAGES: usize = 64;
pub const MAX_POINTER_POLICY_ACTOR_BYTES: usize = 256;
pub const MAX_POINTER_POLICY_MESSAGE_BYTES: usize = 64 * 1024;

const POINTER_MODIFIER_MASK: u32 = 0b1111;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerPhase {
    Enter,
    Leave,
    Motion,
    ButtonPress,
    ButtonRelease,
    Axis,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointerPolicyQuery {
    pub interaction: Option<InteractionId>,
    pub event_sequence: u64,
    pub lease: LayoutLeaseToken,
    pub zone: LayoutItemId,
    pub phase: PointerPhase,
    pub local_position: LocalPoint,
    pub button: Option<u32>,
    pub modifiers: u32,
    pub captured: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerDelivery {
    #[default]
    PassThrough,
    Consume,
    Modify,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerCapture {
    #[default]
    Unchanged,
    Acquire,
    Release,
}

/// The deliberately small set of pointer fields a provider may replace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PointerModification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_position: Option<LocalPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerPolicyActorMessage {
    pub actor: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PointerPolicyResult {
    pub delivery: PointerDelivery,
    pub capture: PointerCapture,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modification: Option<PointerModification>,
    #[serde(default)]
    pub messages: Vec<PointerPolicyActorMessage>,
}

/// One provider call and the immutable actor publication visible at that call.
#[derive(Debug, Clone)]
pub struct PointerPolicyInvocation<'a> {
    query: PointerPolicyQuery,
    publication: Option<&'a PublicationFrameLease>,
}

pub struct WasmPointerPolicy {
    store: Store<()>,
    allocator: GuestArenaAllocator,
    arena: Option<GuestArena>,
    entry: WasmPointerPolicyEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PointerPolicyGuestBuffers {
    query: GuestAllocation,
    state: GuestAllocation,
}

pub struct WasmPointerPolicyEntry {
    func: PointerPolicyExport,
}

impl<'a> PointerPolicyInvocation<'a> {
    pub const fn new(
        query: PointerPolicyQuery,
        publication: Option<&'a PublicationFrameLease>,
    ) -> Self {
        Self { query, publication }
    }

    pub const fn query(&self) -> &PointerPolicyQuery {
        &self.query
    }

    pub const fn publication(&self) -> Option<&PublicationFrameLease> {
        self.publication
    }
}

impl WasmPointerPolicy {
    pub fn load(path: &Path, provider_name: &str) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|error| anyhow::anyhow!("load {}: {error}", path.display()))?;
        let declaration = discover_pointer_provider(&module, provider_name)?;
        Self::load_from_module(engine, &module, &declaration)
    }

    pub fn load_from_module(
        engine: Engine,
        module: &Module,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        ensure!(
            declaration.kind == PolicyProviderKind::Pointer,
            "policy provider {:?} is not a pointer provider",
            declaration.name
        );
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(
                crate::HOST_IMPORT_MODULE,
                crate::READ_MESSAGE_IMPORT,
                || -> i32 { crate::READ_MESSAGE_CLOSED },
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "define inert actor read_message import for pointer provider module: {error}"
                )
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|error| anyhow::anyhow!("instantiate WASM pointer policy: {error}"))?;
        Self::load_from_instance(store, &instance, declaration)
    }

    pub fn load_from_instance(
        mut store: Store<()>,
        instance: &Instance,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        let allocator = GuestArenaAllocator::load(&mut store, instance)?;
        let provider = WasmPolicyProvider::new(declaration.clone());
        let entry =
            crate::pointer_policy_link::load_pointer_policy_entry(&mut store, instance, &provider)?;
        Ok(Self {
            store,
            allocator,
            arena: None,
            entry,
        })
    }

    pub fn handle_pointer(
        &mut self,
        invocation: PointerPolicyInvocation<'_>,
    ) -> Result<PointerPolicyResult> {
        validate_pointer_query(invocation.query())?;
        let query = serde_json::to_vec(invocation.query())
            .context("encode pointer policy invocation context")?;
        let state = invocation
            .publication()
            .map_or(&[][..], PublicationFrameLease::bytes);
        let buffers = self.prepare_invocation_buffers(&query, state)?;
        let response_ptr = self.entry.call(&mut self.store, &buffers)?;
        let result =
            read_pointer_policy_result(self.allocator.memory().data(&self.store), response_ptr)?;
        validate_pointer_result(invocation.query(), &result)?;
        Ok(result)
    }

    fn prepare_invocation_buffers(
        &mut self,
        query: &[u8],
        state: &[u8],
    ) -> Result<PointerPolicyGuestBuffers> {
        ensure!(
            query.len() <= MAX_POINTER_POLICY_QUERY_BYTES,
            "pointer policy query has {} bytes but limit is {MAX_POINTER_POLICY_QUERY_BYTES}",
            query.len()
        );
        let required_capacity = invocation_arena_capacity(query.len(), state.len())?;
        self.ensure_arena_capacity(required_capacity)?;
        let arena = self
            .arena
            .as_mut()
            .context("pointer policy arena is not allocated")?;
        arena.reset();
        let query = self.allocator.copy_bytes(
            &mut self.store,
            arena,
            query,
            crate::DEFAULT_GUEST_ARENA_ALIGN,
        )?;
        let state = self.allocator.copy_bytes(
            &mut self.store,
            arena,
            state,
            crate::DEFAULT_GUEST_ARENA_ALIGN,
        )?;
        Ok(PointerPolicyGuestBuffers { query, state })
    }

    fn ensure_arena_capacity(&mut self, required_capacity: usize) -> Result<()> {
        ensure!(
            required_capacity <= MAX_POINTER_POLICY_ARENA_BYTES,
            "pointer policy invocation needs {required_capacity} bytes but limit is {MAX_POINTER_POLICY_ARENA_BYTES}"
        );
        if let Some(arena) = &self.arena
            && arena_capacity(arena)? >= required_capacity
        {
            return Ok(());
        }
        let capacity = DEFAULT_POINTER_POLICY_ARENA_BYTES
            .max(required_capacity)
            .next_power_of_two()
            .min(MAX_POINTER_POLICY_ARENA_BYTES);
        let old_arena = self.arena.take();
        self.arena = Some(self.allocator.allocate(&mut self.store, capacity)?);
        if let Some(old_arena) = old_arena {
            self.allocator
                .free(&mut self.store, old_arena)
                .context("free previous pointer policy arena")?;
        }
        Ok(())
    }
}

impl WasmPointerPolicyEntry {
    pub(crate) const fn from_export(func: PointerPolicyExport) -> Self {
        Self { func }
    }

    fn call<T: 'static>(
        &self,
        mut store: impl wasmtime::AsContextMut<Data = T>,
        buffers: &PointerPolicyGuestBuffers,
    ) -> Result<i32> {
        self.func
            .call(
                &mut store,
                (
                    buffers.query.ptr(),
                    buffers.query.len(),
                    buffers.state.ptr(),
                    buffers.state.len(),
                ),
            )
            .map_err(|error| anyhow::anyhow!("call WASM pointer policy: {error}"))
    }
}

fn discover_pointer_provider(
    module: &Module,
    provider_name: &str,
) -> Result<PolicyProviderDeclaration> {
    let inventory = crate::WasmModuleInventory::discover(module)?;
    inventory
        .policy_providers
        .into_iter()
        .find(|provider| {
            provider.kind == PolicyProviderKind::Pointer && provider.name == provider_name
        })
        .with_context(|| {
            format!("WASM module does not declare pointer policy provider {provider_name:?}")
        })
}

fn read_pointer_policy_result(memory: &[u8], response_ptr: i32) -> Result<PointerPolicyResult> {
    if response_ptr == POLICY_PROVIDER_RESPONSE_ABSTAIN {
        return Ok(PointerPolicyResult::default());
    }
    ensure!(
        response_ptr != POLICY_PROVIDER_RESPONSE_ERROR,
        "WASM pointer policy returned provider error"
    );
    ensure!(
        response_ptr > 0,
        "WASM pointer policy returned invalid response pointer {response_ptr}"
    );
    let response = PolicyProviderResponse::read_from_memory(memory, response_ptr)?;
    if response.status == POLICY_PROVIDER_RESPONSE_ABSTAIN {
        return Ok(PointerPolicyResult::default());
    }
    ensure!(
        response.status != POLICY_PROVIDER_RESPONSE_ERROR,
        "WASM pointer policy returned provider error"
    );
    ensure!(
        response.status == POLICY_PROVIDER_RESPONSE_OK,
        "WASM pointer policy returned invalid descriptor status {}",
        response.status
    );
    ensure!(
        response.reserved == 0,
        "WASM pointer policy returned non-zero reserved descriptor data"
    );
    let result_len =
        usize::try_from(response.payload_len).context("convert pointer policy result length")?;
    ensure!(
        result_len <= MAX_POINTER_POLICY_RESULT_BYTES,
        "WASM pointer policy returned {result_len} bytes but limit is {MAX_POINTER_POLICY_RESULT_BYTES}"
    );
    let start =
        usize::try_from(response.payload_ptr).context("convert pointer policy result pointer")?;
    let end = start
        .checked_add(result_len)
        .context("pointer policy result range overflowed")?;
    let bytes = memory.get(start..end).with_context(|| {
        format!("pointer policy result range {start}..{end} is outside guest memory")
    })?;
    serde_json::from_slice(bytes).context("decode WASM pointer policy result")
}

fn validate_pointer_query(query: &PointerPolicyQuery) -> Result<()> {
    ensure!(query.event_sequence != 0, "pointer event sequence is zero");
    ensure!(
        query.lease.id != 0 && query.lease.generation != 0,
        "pointer query has an invalid layout lease token"
    );
    ensure!(query.zone.0 != 0, "pointer query has a zero zone identity");
    ensure!(
        valid_local_point(query.local_position),
        "pointer query has an invalid lease-local position"
    );
    ensure!(
        query.modifiers & !POINTER_MODIFIER_MASK == 0,
        "pointer query has unsupported modifier bits"
    );
    ensure!(
        query
            .interaction
            .is_none_or(|interaction| interaction.0 != 0),
        "pointer query has a zero interaction identity"
    );
    ensure!(
        query.captured == query.interaction.is_some(),
        "captured pointer query and interaction identity disagree"
    );
    let expects_button = matches!(
        query.phase,
        PointerPhase::ButtonPress | PointerPhase::ButtonRelease
    );
    ensure!(
        expects_button == query.button.is_some(),
        "pointer query phase and button disagree"
    );
    Ok(())
}

fn validate_pointer_result(query: &PointerPolicyQuery, result: &PointerPolicyResult) -> Result<()> {
    match result.delivery {
        PointerDelivery::Modify => {
            let modification = result
                .modification
                .as_ref()
                .context("modified pointer result has no modification")?;
            ensure!(
                modification.local_position.is_some() || modification.modifiers.is_some(),
                "modified pointer result changes no fields"
            );
            if let Some(position) = modification.local_position {
                ensure!(
                    valid_local_point(position),
                    "pointer result has an invalid modified position"
                );
            }
            if let Some(modifiers) = modification.modifiers {
                ensure!(
                    modifiers & !POINTER_MODIFIER_MASK == 0,
                    "pointer result has unsupported modified modifier bits"
                );
            }
        }
        PointerDelivery::PassThrough | PointerDelivery::Consume => ensure!(
            result.modification.is_none(),
            "unmodified pointer result supplied a modification"
        ),
    }
    match result.capture {
        PointerCapture::Unchanged => {}
        PointerCapture::Acquire => ensure!(
            !query.captured
                && matches!(query.phase, PointerPhase::ButtonPress)
                && result.delivery == PointerDelivery::Consume,
            "pointer capture can be acquired only by consuming an uncaptured press"
        ),
        PointerCapture::Release => ensure!(
            query.captured,
            "pointer capture cannot be released when capture is inactive"
        ),
    }
    ensure!(
        result.messages.len() <= MAX_POINTER_POLICY_MESSAGES,
        "pointer policy returned too many actor messages"
    );
    for message in &result.messages {
        ensure!(
            !message.actor.is_empty() && message.actor.len() <= MAX_POINTER_POLICY_ACTOR_BYTES,
            "pointer policy actor address is empty or oversized"
        );
        ensure!(
            message.payload.len() <= MAX_POINTER_POLICY_MESSAGE_BYTES,
            "pointer policy actor message is oversized"
        );
    }
    Ok(())
}

const fn valid_local_point(point: LocalPoint) -> bool {
    // Captured motion remains lease-local even after it leaves the zone or
    // lease viewport, so valid coordinates are not restricted to [0, 1].
    point.u.is_finite() && point.v.is_finite()
}

fn invocation_arena_capacity(query_len: usize, state_len: usize) -> Result<usize> {
    query_len
        .checked_add(state_len)
        .and_then(|size| size.checked_add(crate::DEFAULT_GUEST_ARENA_ALIGN * 2))
        .context("pointer policy invocation arena size overflowed")
}

fn arena_capacity(arena: &GuestArena) -> Result<usize> {
    usize::try_from(arena.capacity()).context("convert pointer policy arena capacity")
}
