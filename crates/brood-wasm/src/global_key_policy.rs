use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Instance, Linker, Module, Store};

use crate::{
    api::{GuestAllocation, GuestArena, GuestArenaAllocator, Registry},
    global_key_policy_link::{
        GLOBAL_KEY_POLICY_RESULT_ABSTAIN, GLOBAL_KEY_POLICY_RESULT_ERROR, GlobalKeyPolicyExport,
    },
    policy_provider::{
        PolicyProviderDeclaration, PolicyProviderKind, PolicyProviderResponse,
        PublicationFrameLease,
    },
};

pub const ABI_KEY_RELEASED: i32 = 0;
pub const ABI_KEY_PRESSED: i32 = 1;
pub const ABI_INPUT_MODE_PASSTHROUGH: i32 = 0;
pub const ABI_INPUT_MODE_NORMAL: i32 = 1;
pub const ABI_MOD_CTRL: i32 = 1;
pub const ABI_MOD_ALT: i32 = 2;
pub const ABI_MOD_SHIFT: i32 = 4;
pub const DEFAULT_GLOBAL_KEY_POLICY_ARENA_BYTES: usize = 128 * 1024;
pub const MAX_GLOBAL_KEY_POLICY_ARENA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_GLOBAL_KEY_POLICY_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_GLOBAL_KEY_POLICY_RESULT_BYTES: usize = 64 * 1024;
pub const HOST_ACTOR_NAME: &str = "host";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// The complete key event offered to a synchronous policy provider.
///
/// Host input code constructs this value before entering the WASM runtime. The
/// adapter serializes it as one bounded query envelope, so adding event context
/// does not grow a parallel list of scalar arguments through the host.
pub struct GlobalKeyPolicyInput {
    pub key_code: i32,
    pub key_state: i32,
    pub modifiers: i32,
    pub input_mode: i32,
    pub repeat: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlobalKeyDecision {
    Abstain,
    Consume,
    Forward,
    ForwardWithModifiers,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum HostMessage {
    LayoutAction {
        action: String,
    },
    FocusPane {
        pane: u64,
    },
    SpawnApp {
        argv: Vec<String>,
    },
    FocusOrSpawnApp {
        app_id: String,
        argv: Vec<String>,
    },
    SwitchActiveSessionNext,
    CreateAndAttachSession,
    ToggleShortcutPassthrough,
    SetShortcutPassthrough {
        enabled: bool,
    },
    SetClipboard {
        text: String,
    },
    RequestClipboardPaste {
        pane: u64,
    },
    SetResolvedLayout {
        #[serde(default)]
        lease: Option<crate::resolved_layout::LayoutLeaseToken>,
        #[serde(default)]
        revision: Option<u64>,
        layout: crate::resolved_layout::ResolvedLayout,
        #[serde(default)]
        rest: Option<crate::resolved_layout::LayoutChildSlot>,
        focus: Option<u64>,
    },
    ClaimRootLayout,
    DelegateLayoutRest {
        lease: crate::resolved_layout::LayoutLeaseToken,
        revision: u64,
        actor: String,
        panes: Vec<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalKeyActorMessage {
    pub actor: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalKeyPolicyOutcome {
    pub decision: GlobalKeyDecision,
    #[serde(default)]
    pub messages: Vec<GlobalKeyActorMessage>,
}

pub struct WasmGlobalKeyPolicy {
    store: Store<()>,
    allocator: GuestArenaAllocator,
    arena: Option<GuestArena>,
    entry: WasmGlobalKeyPolicyEntry,
}

/// One host-side policy invocation, including the immutable actor publication
/// visible at the same decision point.
#[derive(Debug, Clone)]
pub struct GlobalKeyPolicyInvocation<'a> {
    input: GlobalKeyPolicyInput,
    publication: Option<&'a PublicationFrameLease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalKeyPolicyGuestBuffers {
    pub query: GuestAllocation,
    pub state: GuestAllocation,
}

pub struct WasmGlobalKeyPolicyEntry {
    func: GlobalKeyPolicyExport,
}

impl GlobalKeyPolicyInput {
    pub const fn new(
        key_code: i32,
        key_state: i32,
        modifiers: i32,
        input_mode: i32,
        repeat: bool,
    ) -> Self {
        Self {
            key_code,
            key_state,
            modifiers,
            input_mode,
            repeat,
        }
    }
}

impl<'a> GlobalKeyPolicyInvocation<'a> {
    pub const fn new(
        input: GlobalKeyPolicyInput,
        publication: Option<&'a PublicationFrameLease>,
    ) -> Self {
        Self { input, publication }
    }

    pub const fn input(&self) -> &GlobalKeyPolicyInput {
        &self.input
    }

    pub const fn publication(&self) -> Option<&PublicationFrameLease> {
        self.publication
    }
}

impl Default for GlobalKeyPolicyOutcome {
    fn default() -> Self {
        Self {
            decision: GlobalKeyDecision::Abstain,
            messages: Vec::new(),
        }
    }
}

impl WasmGlobalKeyPolicy {
    pub fn load(path: &Path, provider_name: &str) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
        let declaration = discover_key_provider(&module, provider_name)?;
        Self::load_from_module(engine, &module, &declaration)
    }

    pub fn load_from_module(
        engine: Engine,
        module: &Module,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        ensure!(
            declaration.kind == PolicyProviderKind::Key,
            "policy provider {:?} is not a key provider",
            declaration.name
        );
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(
                crate::HOST_IMPORT_MODULE,
                crate::READ_MESSAGE_IMPORT,
                || -> i32 { crate::READ_MESSAGE_CLOSED },
            )
            .map_err(|err| {
                anyhow::anyhow!(
                    "define inert actor read_message import for policy provider module: {err}"
                )
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|err| anyhow::anyhow!("instantiate WASM global key policy: {err}"))?;
        Self::load_from_instance(store, &instance, declaration)
    }

    pub fn load_from_instance(
        mut store: Store<()>,
        instance: &Instance,
        declaration: &PolicyProviderDeclaration,
    ) -> Result<Self> {
        let allocator = GuestArenaAllocator::load(&mut store, instance)?;
        let provider = crate::policy_provider::WasmPolicyProvider::new(declaration.clone());
        let entry = crate::global_key_policy_link::load_global_key_policy_entry(
            &mut store, instance, &provider,
        )?;
        Ok(Self {
            store,
            allocator,
            arena: None,
            entry,
        })
    }

    pub fn handle_key(
        &mut self,
        invocation: GlobalKeyPolicyInvocation<'_>,
    ) -> Result<GlobalKeyPolicyOutcome> {
        let query = serde_json::to_vec(invocation.input())
            .context("encode global key policy invocation context")?;
        let state = invocation
            .publication()
            .map_or(&[][..], PublicationFrameLease::bytes);
        let buffers = self.prepare_invocation_buffers(&query, state)?;

        let response_ptr = self.entry.call(&mut self.store, &buffers)?;
        read_policy_outcome(self.allocator.memory().data(&self.store), response_ptr)
    }

    fn prepare_invocation_buffers(
        &mut self,
        query: &[u8],
        state: &[u8],
    ) -> Result<GlobalKeyPolicyGuestBuffers> {
        ensure!(
            query.len() <= MAX_GLOBAL_KEY_POLICY_QUERY_BYTES,
            "global key policy query has {} bytes but limit is {MAX_GLOBAL_KEY_POLICY_QUERY_BYTES}",
            query.len()
        );

        let required_capacity = invocation_arena_capacity(query.len(), state.len())?;
        self.ensure_arena_capacity(required_capacity)?;
        let arena = self
            .arena
            .as_mut()
            .context("global key policy arena is not allocated")?;
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
        Ok(GlobalKeyPolicyGuestBuffers { query, state })
    }

    fn ensure_arena_capacity(&mut self, required_capacity: usize) -> Result<()> {
        ensure!(
            required_capacity <= MAX_GLOBAL_KEY_POLICY_ARENA_BYTES,
            "global key policy invocation needs {required_capacity} bytes but limit is {MAX_GLOBAL_KEY_POLICY_ARENA_BYTES}"
        );
        if let Some(arena) = &self.arena
            && arena_capacity(arena)? >= required_capacity
        {
            return Ok(());
        }

        let capacity = DEFAULT_GLOBAL_KEY_POLICY_ARENA_BYTES
            .max(required_capacity)
            .next_power_of_two()
            .min(MAX_GLOBAL_KEY_POLICY_ARENA_BYTES);
        let old_arena = self.arena.take();
        self.arena = Some(self.allocator.allocate(&mut self.store, capacity)?);
        if let Some(old_arena) = old_arena {
            self.allocator
                .free(&mut self.store, old_arena)
                .context("free previous global key policy arena")?;
        }
        Ok(())
    }
}

impl WasmGlobalKeyPolicyEntry {
    pub(crate) const fn from_export(func: GlobalKeyPolicyExport) -> Self {
        Self { func }
    }

    fn call<T: 'static>(
        &self,
        mut store: impl wasmtime::AsContextMut<Data = T>,
        buffers: &GlobalKeyPolicyGuestBuffers,
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
            .map_err(|err| anyhow::anyhow!("call WASM global key policy: {err}"))
    }
}

pub fn host_message(message: HostMessage) -> Result<GlobalKeyActorMessage> {
    Ok(GlobalKeyActorMessage {
        actor: HOST_ACTOR_NAME.to_string(),
        payload: serde_json::to_vec(&message).context("encode host message")?,
    })
}

fn invocation_arena_capacity(query_len: usize, state_len: usize) -> Result<usize> {
    query_len
        .checked_add(state_len)
        .and_then(|size| size.checked_add(crate::DEFAULT_GUEST_ARENA_ALIGN * 2))
        .context("global key policy invocation arena size overflowed")
}

fn arena_capacity(arena: &GuestArena) -> Result<usize> {
    usize::try_from(arena.capacity()).context("convert global key policy arena capacity")
}

fn discover_key_provider(
    module: &Module,
    provider_name: &str,
) -> Result<PolicyProviderDeclaration> {
    let inventory = crate::WasmModuleInventory::discover(module)?;
    inventory
        .policy_providers
        .into_iter()
        .find(|provider| provider.kind == PolicyProviderKind::Key && provider.name == provider_name)
        .with_context(|| {
            format!("WASM module does not declare key policy provider {provider_name:?}")
        })
}

fn read_policy_outcome(memory: &[u8], response_ptr: i32) -> Result<GlobalKeyPolicyOutcome> {
    if response_ptr == GLOBAL_KEY_POLICY_RESULT_ABSTAIN {
        return Ok(GlobalKeyPolicyOutcome::default());
    }
    ensure!(
        response_ptr != GLOBAL_KEY_POLICY_RESULT_ERROR,
        "WASM global key policy returned provider error"
    );
    ensure!(
        response_ptr > 0,
        "WASM global key policy returned invalid response pointer {response_ptr}"
    );

    let response = PolicyProviderResponse::read_from_memory(memory, response_ptr)?;
    if response.status == GLOBAL_KEY_POLICY_RESULT_ABSTAIN {
        return Ok(GlobalKeyPolicyOutcome::default());
    }
    ensure!(
        response.status != GLOBAL_KEY_POLICY_RESULT_ERROR,
        "WASM global key policy returned provider error"
    );
    ensure!(
        response.status == crate::POLICY_PROVIDER_RESPONSE_OK,
        "WASM global key policy returned invalid descriptor status {}",
        response.status
    );
    let result_len =
        usize::try_from(response.payload_len).context("convert key policy result length")?;
    ensure!(
        result_len <= MAX_GLOBAL_KEY_POLICY_RESULT_BYTES,
        "WASM global key policy returned {result_len} bytes but limit is {MAX_GLOBAL_KEY_POLICY_RESULT_BYTES}"
    );
    let start =
        usize::try_from(response.payload_ptr).context("convert key policy result pointer")?;
    let end = start
        .checked_add(result_len)
        .context("key policy result range overflowed")?;
    let bytes = memory.get(start..end).with_context(|| {
        format!("key policy result range {start}..{end} is outside guest memory")
    })?;
    serde_json::from_slice(bytes).context("decode WASM global key policy result")
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "host adapters pass owned decision envelopes"
)]
pub fn enqueue_policy_messages(
    registry: &Registry,
    messages: Vec<GlobalKeyActorMessage>,
) -> Result<()> {
    for message in messages {
        ensure!(
            !message.actor.is_empty(),
            "key policy message actor must not be empty"
        );
        registry
            .try_post_named_message(&message.actor, message.payload)
            .with_context(|| format!("enqueue key policy message for actor {:?}", message.actor))?;
    }
    Ok(())
}
