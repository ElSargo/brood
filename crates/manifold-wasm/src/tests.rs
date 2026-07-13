use anyhow::{Context, Result, ensure};
use wasmtime::{Engine, Module};

use crate::{
    ActorEpochOutcome, ActorOutbox, GlobalKeyDecision, GlobalKeyPolicyInput,
    GlobalKeyPolicyInvocation, Registry, WasmActor, WasmActorConfig, WasmGlobalKeyPolicy,
    WasmModuleInventory, actor::pack_actor_outbox_slice, api::async_epoch_engine,
    global_key_policy::enqueue_policy_messages, policy_provider::PolicyProviderDeclaration,
};

#[test]
fn actors_messages_publications_and_policy_providers_work_end_to_end() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build test runtime")?
        .block_on(async {
            let actor_engine = async_epoch_engine()?;
            let actor_module = module_from_wat(&actor_engine, &actor_module_wat()?)?;
            let inventory = WasmModuleInventory::discover(&actor_module)?;
            ensure!(inventory.actors.len() == 2, "expected two actors");

            let mut registry = Registry::new();
            let mut host_mailbox = registry.register_host_mailbox(8)?;
            let actor_a_mailbox = registry.register_actor("a", 8)?;
            let actor_a_id = actor_a_mailbox.actor_id();
            let actor_b_mailbox = registry.register_actor("b", 8)?;
            let actor_b_id = actor_b_mailbox.actor_id();

            let mut actor_a = WasmActor::load_from_module(
                actor_engine.clone(),
                &actor_module,
                actor_a_mailbox,
                WasmActorConfig::default(),
                "manifold_actor_a_epoch_run",
            )
            .await?;
            let mut actor_b = WasmActor::load_from_module(
                actor_engine,
                &actor_module,
                actor_b_mailbox,
                WasmActorConfig::default(),
                "manifold_actor_b_epoch_run",
            )
            .await?;

            registry.try_post_named_message("a", b"start".to_vec())?;
            let outcome = actor_a.run_one_epoch_and_dispatch(&mut registry).await?;
            ensure!(
                matches!(outcome, ActorEpochOutcome::Continue { outbox: Some(_) }),
                "actor a should return an outbox"
            );
            ensure!(
                host_mailbox.read_message().await? == b"hi".to_vec(),
                "actor a should send a host message"
            );
            ensure!(
                registry
                    .latest_default_publication_frame(actor_a_id)?
                    .context("actor a publication")?
                    .bytes()
                    == b"a",
                "actor a should publish state"
            );

            let outcome = actor_b.run_one_epoch_and_dispatch(&mut registry).await?;
            ensure!(
                matches!(outcome, ActorEpochOutcome::Continue { outbox: Some(_) }),
                "actor b should return an outbox"
            );
            let b_state = registry
                .latest_default_publication_frame(actor_b_id)?
                .context("actor b publication")?;
            ensure!(b_state.bytes() == b"b", "actor b should publish state");

            let policy_engine = Engine::default();
            let policy_module = module_from_wat(&policy_engine, &policy_module_wat()?)?;
            let policy_inventory = WasmModuleInventory::discover(&policy_module)?;
            ensure!(
                policy_inventory.policy_providers.len() == 2,
                "expected two policy providers"
            );

            let consume_decl = policy_provider(&policy_inventory.policy_providers, "consume")?;
            let mut consume_policy = WasmGlobalKeyPolicy::load_from_module(
                policy_engine.clone(),
                &policy_module,
                consume_decl,
            )?;
            let outcome = consume_policy.handle_key(GlobalKeyPolicyInvocation::new(
                GlobalKeyPolicyInput::new(
                    32,
                    crate::ABI_KEY_PRESSED,
                    crate::ABI_MOD_CTRL,
                    crate::ABI_INPUT_MODE_NORMAL,
                ),
                Some(&b_state),
            ))?;
            ensure!(
                outcome.decision == GlobalKeyDecision::Consume,
                "consume policy should consume"
            );
            enqueue_policy_messages(&registry, outcome.messages)?;
            ensure!(
                host_mailbox.read_message().await? == b"ok".to_vec(),
                "policy should send a host message"
            );

            let abstain_decl = policy_provider(&policy_inventory.policy_providers, "abstain")?;
            let mut abstain_policy = WasmGlobalKeyPolicy::load_from_module(
                policy_engine.clone(),
                &policy_module,
                abstain_decl,
            )?;
            let outcome = abstain_policy.handle_key(GlobalKeyPolicyInvocation::new(
                GlobalKeyPolicyInput::new(
                    32,
                    crate::ABI_KEY_PRESSED,
                    crate::ABI_MOD_CTRL,
                    crate::ABI_INPUT_MODE_NORMAL,
                ),
                Some(&b_state),
            ))?;
            ensure!(
                outcome.decision == GlobalKeyDecision::Abstain,
                "abstain policy should abstain"
            );
            ensure!(
                outcome.messages.is_empty(),
                "abstain policy should not send messages"
            );

            Ok(())
        })
}

#[test]
fn key_policy_invocation_groups_the_complete_event_context() -> Result<()> {
    let invocation = GlobalKeyPolicyInvocation::new(
        GlobalKeyPolicyInput::new(
            32,
            crate::ABI_KEY_RELEASED,
            crate::ABI_MOD_CTRL | crate::ABI_MOD_SHIFT,
            crate::ABI_INPUT_MODE_PASSTHROUGH,
        ),
        None,
    );

    ensure!(
        serde_json::to_string(invocation.input())?
            == r#"{"key_code":32,"key_state":0,"modifiers":5,"input_mode":0}"#,
        "typed key invocation should serialize one complete event envelope"
    );
    ensure!(
        invocation.publication().is_none(),
        "invocation should preserve the absence of published state"
    );
    Ok(())
}

fn policy_provider<'a>(
    providers: &'a [PolicyProviderDeclaration],
    name: &str,
) -> Result<&'a PolicyProviderDeclaration> {
    providers
        .iter()
        .find(|provider| provider.name == name)
        .with_context(|| format!("missing policy provider {name:?}"))
}

fn module_from_wat(engine: &Engine, wat: &str) -> Result<Module> {
    let bytes = wat::parse_str(wat).context("parse wat")?;
    Module::new(engine, bytes).map_err(|err| anyhow::anyhow!("compile wat module: {err}"))
}

fn actor_module_wat() -> Result<String> {
    let a_outbox = serde_json::to_vec(&ActorOutbox {
        messages: vec![
            crate::ActorOutboxMessage {
                actor: "b".to_string(),
                payload: b"ping".to_vec(),
            },
            crate::ActorOutboxMessage {
                actor: crate::HOST_ACTOR_NAME.to_string(),
                payload: b"hi".to_vec(),
            },
        ],
        publications: vec![crate::ActorPublication {
            payload: b"a".to_vec(),
        }],
    })?;
    let b_outbox = serde_json::to_vec(&ActorOutbox {
        messages: Vec::new(),
        publications: vec![crate::ActorPublication {
            payload: b"b".to_vec(),
        }],
    })?;
    let a_ptr = 4096;
    let b_ptr = a_ptr + a_outbox.len() + 64;
    let a_return = pack_actor_outbox_slice(i32::try_from(a_ptr)?, i32::try_from(a_outbox.len())?)?;
    let b_return = pack_actor_outbox_slice(i32::try_from(b_ptr)?, i32::try_from(b_outbox.len())?)?;

    Ok(format!(
        r#"(module
  (import "manifold" "read_message" (func $read_message (result i32)))
  (memory (export "memory") 4)
  (global $heap (mut i32) (i32.const 65536))
  (func (export "manifold_arena_alloc") (param $capacity i32) (param $align i32) (result i32)
    (local $ptr i32)
    global.get $heap
    local.set $ptr
    global.get $heap
    local.get $capacity
    i32.add
    global.set $heap
    local.get $ptr)
  (func (export "manifold_arena_free") (param i32) (param i32) (param i32))
  (func (export "manifold_actor_a_epoch_run") (param i64) (param i32) (param i32) (result i64)
    call $read_message
    drop
    i64.const {a_return})
  (func (export "manifold_actor_b_epoch_run") (param i64) (param i32) (param i32) (result i64)
    call $read_message
    drop
    i64.const {b_return})
  (data (i32.const {a_ptr}) "{a_data}")
  (data (i32.const {b_ptr}) "{b_data}"))"#,
        a_data = wat_data(&a_outbox),
        b_data = wat_data(&b_outbox),
    ))
}

fn policy_module_wat() -> Result<String> {
    let consume = serde_json::to_vec(&crate::GlobalKeyPolicyOutcome {
        decision: GlobalKeyDecision::Consume,
        messages: vec![crate::GlobalKeyActorMessage {
            actor: crate::HOST_ACTOR_NAME.to_string(),
            payload: b"ok".to_vec(),
        }],
    })?;
    let consume_ptr = 4096;
    let consume_response_ptr = consume_ptr + consume.len() + 64;
    Ok(format!(
        r#"(module
  (memory (export "memory") 32)
  (global $heap (mut i32) (i32.const 65536))
  (func (export "manifold_arena_alloc") (param $capacity i32) (param $align i32) (result i32)
    (local $ptr i32)
    global.get $heap
    local.set $ptr
    global.get $heap
    local.get $capacity
    i32.add
    global.set $heap
    local.get $ptr)
  (func (export "manifold_arena_free") (param i32) (param i32) (param i32))
  (func $response (param $desc i32) (param $payload i32) (param $len i32) (result i32)
    local.get $desc
    i32.const 1
    i32.store
    local.get $desc
    i32.const 4
    i32.add
    local.get $payload
    i32.store
    local.get $desc
    i32.const 8
    i32.add
    local.get $len
    i32.store
    local.get $desc
    i32.const 12
    i32.add
    i32.const 0
    i32.store
    local.get $desc)
  (func (export "manifold_policy_provider_key_consume")
    (param $query_ptr i32) (param $query_len i32) (param $state_ptr i32) (param $state_len i32) (result i32)
    local.get $state_len
    i32.const 1
    i32.eq
    local.get $state_ptr
    i32.load8_u
    i32.const 98
    i32.eq
    i32.and
    if (result i32)
      i32.const {consume_response_ptr}
      i32.const {consume_ptr}
      i32.const {consume_len}
      call $response
    else
      i32.const 0
    end)
  (func (export "manifold_policy_provider_key_abstain")
    (param i32) (param i32) (param i32) (param i32) (result i32)
    i32.const 0)
  (data (i32.const {consume_ptr}) "{consume_data}"))"#,
        consume_len = consume.len(),
        consume_response_ptr = consume_response_ptr,
        consume_data = wat_data(&consume),
    ))
}

fn wat_data(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| match *byte {
            b'"' => "\\22".to_string(),
            b'\\' => "\\5c".to_string(),
            0x20..=0x7e => char::from(*byte).to_string(),
            byte => format!("\\{byte:02x}"),
        })
        .collect::<String>()
}
