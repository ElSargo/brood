use std::path::Path;

use anyhow::{Context, Result};
use wasmtime::{Caller, Engine, Linker, Module, Store, TypedFunc};

use crate::layout::VisibleLayoutNode;

const MAX_MAP_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MapHookResponse {
    Abstain,
    Patch {
        patches: Vec<MapLayoutPatch>,
    },
    ReplaceVisibleTree {
        root: VisibleLayoutNode,
        #[serde(default)]
        focus: Option<u64>,
    },
    Reject {
        reason: String,
    },
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MapLayoutPatch {
    PlacePane {
        pane: u64,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        focus: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapHookInput {
    pub pending_pane: u64,
    pub visible_panes: Vec<u64>,
}

#[derive(Default)]
struct WasmMapStore {
    pending_pane: u64,
    visible_panes: Vec<u64>,
    response: Option<String>,
}

pub struct WasmMapHook {
    store: Store<WasmMapStore>,
    on_map: TypedFunc<(i64, i32), i32>,
}

impl WasmMapHook {
    pub fn load(path: &Path) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
        let mut linker = Linker::new(&engine);

        linker
            .func_wrap(
                "manifold",
                "visible_pane_id",
                |caller: Caller<'_, WasmMapStore>, index: i32| -> i64 {
                    let Ok(index) = usize::try_from(index) else {
                        return -1;
                    };
                    caller
                        .data()
                        .visible_panes
                        .get(index)
                        .and_then(|pane| i64::try_from(*pane).ok())
                        .unwrap_or(-1)
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.visible_pane_id import: {err}"))?;
        linker
            .func_wrap(
                "manifold",
                "set_map_response",
                |mut caller: Caller<'_, WasmMapStore>, ptr: i32, len: i32| -> i32 {
                    let response = match read_guest_string(&mut caller, ptr, len) {
                        Ok(response) => response,
                        Err(err) => {
                            tracing::warn!(?err, "WASM map hook passed invalid response string");
                            return 0;
                        }
                    };
                    caller.data_mut().response = Some(response);
                    1
                },
            )
            .map_err(|err| anyhow::anyhow!("define manifold.set_map_response import: {err}"))?;

        let mut store = Store::new(&engine, WasmMapStore::default());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|err| anyhow::anyhow!("instantiate WASM map hook: {err}"))?;
        let on_map = instance
            .get_typed_func::<(i64, i32), i32>(&mut store, "on_map_v1")
            .map_err(|err| anyhow::anyhow!("find exported on_map_v1(pending, count): {err}"))?;

        Ok(Self { store, on_map })
    }

    pub fn handle_map(&mut self, input: MapHookInput) -> Result<MapHookOutcome> {
        self.store.data_mut().pending_pane = input.pending_pane;
        self.store.data_mut().visible_panes = input.visible_panes;
        self.store.data_mut().response = None;

        let pending = i64::try_from(input.pending_pane).context("convert pending pane id")?;
        let visible_count =
            i32::try_from(self.store.data().visible_panes.len()).context("visible pane count")?;
        let handled = self
            .on_map
            .call(&mut self.store, (pending, visible_count))
            .map_err(|err| anyhow::anyhow!("call WASM map hook: {err}"))?
            != 0;

        if !handled {
            return Ok(MapHookOutcome::Abstain);
        }

        let Some(response) = self.store.data_mut().response.take() else {
            return Ok(MapHookOutcome::Abstain);
        };
        let response = serde_json::from_str(&response).context("parse WASM map hook response")?;
        Ok(MapHookOutcome::Response(response))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapHookOutcome {
    Abstain,
    Response(MapHookResponse),
}

fn read_guest_string(caller: &mut Caller<'_, WasmMapStore>, ptr: i32, len: i32) -> Result<String> {
    anyhow::ensure!(ptr >= 0, "guest string pointer must be non-negative");
    anyhow::ensure!(len >= 0, "guest string length must be non-negative");
    let len = usize::try_from(len).context("convert guest string length")?;
    anyhow::ensure!(
        len <= MAX_MAP_RESPONSE_BYTES,
        "guest map response exceeds {MAX_MAP_RESPONSE_BYTES} bytes"
    );
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .context("guest must export memory for set_map_response(ptr, len)")?;
    let ptr = usize::try_from(ptr).context("convert guest string pointer")?;
    let mut bytes = vec![0; len];
    memory
        .read(caller, ptr, &mut bytes)
        .context("read guest memory")?;
    String::from_utf8(bytes).context("guest map response must be UTF-8")
}

#[cfg(test)]
mod tests {
    use crate::layout::{VisibleLayoutAxis, VisibleLayoutNode};

    use super::MapHookResponse;

    #[test]
    fn parses_replace_visible_tree_response() {
        let response: MapHookResponse = serde_json::from_str(
            r#"{
                "type": "replace-visible-tree",
                "focus": 1,
                "root": {
                    "type": "split",
                    "axis": "horizontal",
                    "children": [
                        { "type": "pane", "pane": 1 },
                        { "type": "stack", "panes": [2, 3] }
                    ]
                }
            }"#,
        )
        .expect("parse response");

        assert_eq!(
            response,
            MapHookResponse::ReplaceVisibleTree {
                focus: Some(1),
                root: VisibleLayoutNode::Split {
                    axis: VisibleLayoutAxis::Horizontal,
                    children: vec![
                        VisibleLayoutNode::Pane { pane: 1 },
                        VisibleLayoutNode::Stack { panes: vec![2, 3] },
                    ],
                    flexes: None,
                }
            }
        );
    }
}
