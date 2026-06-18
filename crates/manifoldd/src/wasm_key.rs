use std::path::Path;

use anyhow::{Context, Result};
use smithay::backend::input::{KeyState, Keycode};
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::{
    layout::LayoutAction,
    state::{InputMode, Manifold},
};

const ABI_KEY_RELEASED: i32 = 0;
const ABI_KEY_PRESSED: i32 = 1;
const ABI_MODE_PASSTHROUGH: i32 = 0;
const ABI_MODE_NORMAL: i32 = 1;
const MAX_SPAWN_COMMAND_BYTES: usize = 4096;

#[derive(Debug)]
enum WasmKeyCommand {
    Layout(LayoutAction),
    SpawnApp(Vec<String>),
}

#[derive(Default)]
struct WasmKeyStore {
    pending_commands: Vec<WasmKeyCommand>,
}

pub struct WasmKeyHandler {
    store: Store<WasmKeyStore>,
    on_key: TypedFunc<(i32, i32, i32, i32), i32>,
}

impl WasmKeyHandler {
    pub fn load(path: &Path) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
        let mut linker = Linker::new(&engine);

        linker
            .func_wrap(
                "manifold",
                "focus_next",
                |mut caller: Caller<'_, WasmKeyStore>| {
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::Layout(LayoutAction::FocusNext));
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.focus_next import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "focus_previous",
                |mut caller: Caller<'_, WasmKeyStore>| {
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::Layout(LayoutAction::FocusPrevious));
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.focus_previous import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "toggle_fullscreen",
                |mut caller: Caller<'_, WasmKeyStore>| {
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::Layout(
                            LayoutAction::ToggleManagedFullscreen,
                        ));
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.toggle_fullscreen import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "close_active_window",
                |mut caller: Caller<'_, WasmKeyStore>| {
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::Layout(LayoutAction::CloseActivePane));
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.close_active_window import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "layout_action",
                |mut caller: Caller<'_, WasmKeyStore>, action: i32| -> i32 {
                    let Some(action) = LayoutAction::from_abi(action) else {
                        tracing::warn!(action, "WASM key handler requested unknown layout action");
                        return 0;
                    };
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::Layout(action));
                    1
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.layout_action import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "spawn_app",
                |mut caller: Caller<'_, WasmKeyStore>, ptr: i32, len: i32| -> i32 {
                    let command = match read_guest_string(&mut caller, ptr, len) {
                        Ok(command) => command,
                        Err(err) => {
                            tracing::warn!(
                                ?err,
                                "WASM key handler passed invalid spawn_app string"
                            );
                            return 0;
                        }
                    };
                    let argv = parse_spawn_command(&command);
                    if argv.is_empty() {
                        return 0;
                    }
                    caller
                        .data_mut()
                        .pending_commands
                        .push(WasmKeyCommand::SpawnApp(argv));
                    1
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.spawn_app import: {err}"))?;

        let mut store = Store::new(&engine, WasmKeyStore::default());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|err| anyhow::anyhow!("instantiate WASM key handler: {err}"))?;
        let on_key = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "on_key")
            .map_err(|err| {
                anyhow::anyhow!(
                    "find exported on_key(key_code, key_state, input_mode, ctrl_pressed): {err}"
                )
            })?;

        Ok(Self { store, on_key })
    }

    pub fn handle_key(
        &mut self,
        key_code: Keycode,
        key_state: KeyState,
        input_mode: InputMode,
        ctrl_pressed: bool,
    ) -> Result<WasmKeyOutcome> {
        self.store.data_mut().pending_commands.clear();
        let handled = self
            .on_key
            .call(
                &mut self.store,
                (
                    i32::try_from(key_code.raw()).unwrap_or(i32::MAX),
                    abi_key_state(key_state),
                    abi_input_mode(input_mode),
                    i32::from(ctrl_pressed),
                ),
            )
            .map_err(|err| anyhow::anyhow!("call WASM on_key: {err}"))?
            != 0;
        let commands = self
            .store
            .data_mut()
            .pending_commands
            .drain(..)
            .collect::<Vec<_>>();
        Ok(WasmKeyOutcome { handled, commands })
    }
}

pub struct WasmKeyOutcome {
    handled: bool,
    commands: Vec<WasmKeyCommand>,
}

impl WasmKeyOutcome {
    pub fn apply(self, state: &mut Manifold) -> bool {
        for command in self.commands {
            match command {
                WasmKeyCommand::Layout(action) => {
                    state.apply_layout_action(action);
                }
                WasmKeyCommand::SpawnApp(argv) => {
                    if let Err(err) = state.spawn_hosted_app_argv(&argv) {
                        tracing::warn!(?err, ?argv, "WASM key handler failed to spawn app");
                    }
                }
            }
        }
        self.handled
    }
}

fn abi_key_state(state: KeyState) -> i32 {
    match state {
        KeyState::Released => ABI_KEY_RELEASED,
        KeyState::Pressed => ABI_KEY_PRESSED,
    }
}

fn abi_input_mode(mode: InputMode) -> i32 {
    match mode {
        InputMode::Passthrough => ABI_MODE_PASSTHROUGH,
        InputMode::Normal => ABI_MODE_NORMAL,
    }
}

fn read_guest_string(caller: &mut Caller<'_, WasmKeyStore>, ptr: i32, len: i32) -> Result<String> {
    anyhow::ensure!(ptr >= 0, "guest string pointer must be non-negative");
    anyhow::ensure!(len >= 0, "guest string length must be non-negative");
    let len = usize::try_from(len).context("convert guest string length")?;
    anyhow::ensure!(
        len <= MAX_SPAWN_COMMAND_BYTES,
        "guest spawn command exceeds {MAX_SPAWN_COMMAND_BYTES} bytes"
    );
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .context("guest must export memory for spawn_app(ptr, len)")?;
    let ptr = usize::try_from(ptr).context("convert guest string pointer")?;
    let mut bytes = vec![0; len];
    read_memory(memory, caller, ptr, &mut bytes)?;
    String::from_utf8(bytes).context("guest spawn command must be UTF-8")
}

fn read_memory(
    memory: Memory,
    caller: &mut Caller<'_, WasmKeyStore>,
    ptr: usize,
    bytes: &mut [u8],
) -> Result<()> {
    memory.read(caller, ptr, bytes).context("read guest memory")
}

fn parse_spawn_command(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::parse_spawn_command;

    #[test]
    fn parse_spawn_command_splits_whitespace() {
        assert_eq!(
            parse_spawn_command("foot --title demo"),
            vec!["foot", "--title", "demo"]
        );
    }
}
