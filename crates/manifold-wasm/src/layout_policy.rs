use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Instance, Linker, Module, Store};

use crate::{
    api::{GuestAllocation, GuestArena, GuestArenaAllocator},
    layout_policy_link::{
        LAYOUT_POLICY_RESULT_ABSTAIN, LAYOUT_POLICY_RESULT_ERROR, LayoutPolicyExport,
    },
    policy_provider::{
        PolicyProviderDeclaration, PolicyProviderKind, PolicyProviderResponse,
        PublicationFrameLease,
    },
};

pub const DEFAULT_LAYOUT_POLICY_ARENA_BYTES: usize = 128 * 1024;
pub const MAX_LAYOUT_POLICY_ARENA_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LAYOUT_POLICY_QUERY_BYTES: usize = 64 * 1024;
pub const MAX_LAYOUT_POLICY_RESULT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayoutPolicyInput {
    pub visible_panes: Vec<u64>,
    #[serde(default)]
    pub focused_pane: Option<u64>,
    pub presentation_size: LayoutSize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutPolicyOutcome {
    pub layout: ResolvedLayout,
    #[serde(default)]
    pub focus: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedLayout {
    pub items: Vec<ResolvedLayoutItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayoutItemId(pub u64);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ResolvedLayoutItem {
    pub id: LayoutItemId,
    pub kind: LayoutItemKind,
    pub z: f64,
    pub transform: LocalTransform,
    pub input: InputBehavior,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayoutItemKind {
    Pane { pane: u64 },
    Placeholder,
    Handle,
    Chrome,
    Overlay,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputBehavior {
    None,
    Hit,
    Drag,
    Pane,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayoutSize {
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LocalPoint {
    pub u: f64,
    pub v: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LocalTransform {
    pub offset: LocalPoint,
    pub size: LocalPoint,
}

pub struct WasmLayoutPolicy {
    store: Store<()>,
    allocator: GuestArenaAllocator,
    arena: Option<GuestArena>,
    entry: WasmLayoutPolicyEntry,
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPolicyInvocationBuffers {
    pub query: GuestAllocation,
    pub state: GuestAllocation,
}

pub struct WasmLayoutPolicyEntry {
    func: LayoutPolicyExport,
}

impl LayoutPolicyInput {
    pub fn new(
        visible_panes: Vec<u64>,
        focused_pane: Option<u64>,
        presentation_size: LayoutSize,
    ) -> Self {
        Self {
            visible_panes,
            focused_pane,
            presentation_size,
        }
    }
}

impl ResolvedLayout {
    pub const fn empty() -> Self {
        Self { items: Vec::new() }
    }
}

impl WasmLayoutPolicy {
    pub fn load(path: &Path, provider_name: &str) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
        let declaration = discover_layout_provider(&module, provider_name)?;
        Self::load_from_module(engine, &module, &declaration)
    }

    pub fn load_from_module(
        engine: Engine,
        module: &Module,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        ensure!(
            declaration.kind == PolicyProviderKind::Layout,
            "policy provider {:?} is not a layout provider",
            declaration.name
        );
        let linker = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|err| anyhow::anyhow!("instantiate WASM layout policy: {err}"))?;
        Self::load_from_instance(store, &instance, declaration)
    }

    pub fn load_from_instance(
        mut store: Store<()>,
        instance: &Instance,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        let allocator = GuestArenaAllocator::load(&mut store, instance)?;
        let provider = crate::policy_provider::WasmPolicyProvider::new(declaration.clone());
        let entry =
            crate::layout_policy_link::load_layout_policy_entry(&mut store, instance, &provider)?;
        Ok(Self {
            store,
            allocator,
            arena: None,
            entry,
        })
    }

    pub fn resolve_layout(
        &mut self,
        input: LayoutPolicyInput,
        publication: Option<&PublicationFrameLease>,
    ) -> Result<Option<LayoutPolicyOutcome>> {
        let query = serde_json::to_vec(&input).context("encode layout policy query")?;
        let state = publication.map_or(&[][..], PublicationFrameLease::bytes);
        let buffers = self.prepare_invocation_buffers(&query, state)?;

        let response_ptr = self.entry.call(&mut self.store, &buffers)?;
        read_layout_policy_outcome(self.allocator.memory().data(&self.store), response_ptr)
    }

    pub fn prepare_invocation_buffers(
        &mut self,
        query: &[u8],
        state: &[u8],
    ) -> Result<LayoutPolicyInvocationBuffers> {
        ensure!(
            query.len() <= MAX_LAYOUT_POLICY_QUERY_BYTES,
            "layout policy query has {} bytes but limit is {MAX_LAYOUT_POLICY_QUERY_BYTES}",
            query.len()
        );

        let required_capacity = invocation_arena_capacity(query.len(), state.len())?;
        self.ensure_arena_capacity(required_capacity)?;
        let arena = self
            .arena
            .as_mut()
            .context("layout policy arena is not allocated")?;
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
        Ok(LayoutPolicyInvocationBuffers { query, state })
    }

    fn ensure_arena_capacity(&mut self, required_capacity: usize) -> Result<()> {
        ensure!(
            required_capacity <= MAX_LAYOUT_POLICY_ARENA_BYTES,
            "layout policy invocation needs {required_capacity} bytes but limit is {MAX_LAYOUT_POLICY_ARENA_BYTES}"
        );
        if let Some(arena) = &self.arena
            && arena_capacity(arena)? >= required_capacity
        {
            return Ok(());
        }

        let capacity = DEFAULT_LAYOUT_POLICY_ARENA_BYTES
            .max(required_capacity)
            .next_power_of_two()
            .min(MAX_LAYOUT_POLICY_ARENA_BYTES);
        let old_arena = self.arena.take();
        self.arena = Some(self.allocator.allocate(&mut self.store, capacity)?);
        if let Some(old_arena) = old_arena {
            self.allocator
                .free(&mut self.store, old_arena)
                .context("free previous layout policy arena")?;
        }
        Ok(())
    }
}

impl WasmLayoutPolicyEntry {
    pub(crate) const fn from_export(func: LayoutPolicyExport) -> Self {
        Self { func }
    }

    pub fn call<T: 'static>(
        &self,
        mut store: impl wasmtime::AsContextMut<Data = T>,
        buffers: &LayoutPolicyInvocationBuffers,
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
            .map_err(|err| anyhow::anyhow!("call WASM layout policy: {err}"))
    }
}

fn invocation_arena_capacity(query_len: usize, state_len: usize) -> Result<usize> {
    query_len
        .checked_add(state_len)
        .and_then(|size| size.checked_add(crate::DEFAULT_GUEST_ARENA_ALIGN * 2))
        .context("layout policy invocation arena size overflowed")
}

fn arena_capacity(arena: &GuestArena) -> Result<usize> {
    usize::try_from(arena.capacity()).context("convert layout policy arena capacity")
}

fn discover_layout_provider(
    module: &Module,
    provider_name: &str,
) -> Result<PolicyProviderDeclaration> {
    let inventory = crate::WasmModuleInventory::discover(module)?;
    inventory
        .policy_providers
        .into_iter()
        .find(|provider| {
            provider.kind == PolicyProviderKind::Layout && provider.name == provider_name
        })
        .with_context(|| {
            format!("WASM module does not declare layout policy provider {provider_name:?}")
        })
}

fn read_layout_policy_outcome(
    memory: &[u8],
    response_ptr: i32,
) -> Result<Option<LayoutPolicyOutcome>> {
    if response_ptr == LAYOUT_POLICY_RESULT_ABSTAIN {
        return Ok(None);
    }
    ensure!(
        response_ptr != LAYOUT_POLICY_RESULT_ERROR,
        "WASM layout policy returned provider error"
    );
    ensure!(
        response_ptr > 0,
        "WASM layout policy returned invalid response pointer {response_ptr}"
    );

    let response = PolicyProviderResponse::read_from_memory(memory, response_ptr)?;
    if response.status == LAYOUT_POLICY_RESULT_ABSTAIN {
        return Ok(None);
    }
    ensure!(
        response.status != LAYOUT_POLICY_RESULT_ERROR,
        "WASM layout policy returned provider error"
    );
    ensure!(
        response.status == crate::POLICY_PROVIDER_RESPONSE_OK,
        "WASM layout policy returned invalid descriptor status {}",
        response.status
    );
    let result_len =
        usize::try_from(response.payload_len).context("convert layout policy result length")?;
    ensure!(
        result_len <= MAX_LAYOUT_POLICY_RESULT_BYTES,
        "WASM layout policy returned {result_len} bytes but limit is {MAX_LAYOUT_POLICY_RESULT_BYTES}"
    );
    let start =
        usize::try_from(response.payload_ptr).context("convert layout policy result pointer")?;
    let end = start
        .checked_add(result_len)
        .context("layout policy result range overflowed")?;
    let bytes = memory.get(start..end).with_context(|| {
        format!("layout policy result range {start}..{end} is outside guest memory")
    })?;
    serde_json::from_slice(bytes).context("decode WASM layout policy result")
}
