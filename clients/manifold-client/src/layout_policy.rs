use std::path::Path;

use anyhow::{Context, Result, ensure};
use manifold_layout::{
    InputBehavior, LayoutAxis, LayoutItemId, LayoutItemKind, LayoutRect, LayoutSize, LayoutState,
    LocalPoint, LocalTransform, ResolvedLayout, ResolvedLayoutItem,
};
use manifold_wasm::{LayoutPolicyInput, WasmLayoutPolicy};

pub struct ClientLayoutPolicy {
    wasm: Option<WasmLayoutPolicy>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutResolution {
    pub layout: ResolvedLayout,
    pub focus: Option<u64>,
    pub source: LayoutSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutSource {
    NativeFallback,
    WasmPolicy,
}

impl ClientLayoutPolicy {
    pub fn load(spec: Option<&[String]>) -> Result<Self> {
        let wasm = spec.map(load_wasm_layout_policy).transpose()?;
        Ok(Self { wasm })
    }

    pub fn resolve(
        &mut self,
        panes: &[u64],
        focused_pane: Option<u64>,
        presentation_size: LayoutSize,
    ) -> LayoutResolution {
        if let Some(policy) = self.wasm.as_mut() {
            let input = LayoutPolicyInput::new(
                panes.to_vec(),
                focused_pane,
                manifold_wasm::LayoutSize {
                    w: presentation_size.w,
                    h: presentation_size.h,
                },
            );
            match policy.resolve_layout(input, None) {
                Ok(Some(outcome)) => {
                    let candidate = convert_wasm_layout(outcome.layout);
                    match candidate.validate_for_panes(panes) {
                        Ok(layout) => {
                            let focus = outcome
                                .focus
                                .filter(|pane| layout.pane_item(*pane).is_some());
                            return LayoutResolution {
                                layout,
                                focus,
                                source: LayoutSource::WasmPolicy,
                            };
                        }
                        Err(error) => {
                            eprintln!(
                                "WASM layout policy returned an invalid flat layout; using native fallback: {error}"
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("WASM layout policy failed; using native fallback: {error:#}");
                }
            }
        }

        LayoutResolution {
            layout: native_flat_layout(panes, presentation_size),
            focus: focused_pane.filter(|pane| panes.contains(pane)),
            source: LayoutSource::NativeFallback,
        }
    }
}

fn load_wasm_layout_policy(spec: &[String]) -> Result<WasmLayoutPolicy> {
    ensure!(
        spec.len() == 2,
        "layout policy requires a provider name and WASM path"
    );
    let provider = spec.first().context("layout policy provider is missing")?;
    let path = spec.get(1).context("layout policy path is missing")?;
    let policy = WasmLayoutPolicy::load(Path::new(path), provider)
        .with_context(|| format!("load presentation layout policy {provider:?} from {path}"))?;
    eprintln!("loaded presentation layout policy provider={provider} path={path}");
    Ok(policy)
}

fn convert_wasm_layout(layout: manifold_wasm::ResolvedLayout) -> ResolvedLayout {
    ResolvedLayout {
        items: layout
            .items
            .into_iter()
            .map(|item| ResolvedLayoutItem {
                id: LayoutItemId(item.id.0),
                kind: match item.kind {
                    manifold_wasm::LayoutItemKind::Pane { pane } => LayoutItemKind::Pane { pane },
                    manifold_wasm::LayoutItemKind::Placeholder => LayoutItemKind::Placeholder,
                    manifold_wasm::LayoutItemKind::Handle => LayoutItemKind::Handle,
                    manifold_wasm::LayoutItemKind::Chrome => LayoutItemKind::Chrome,
                    manifold_wasm::LayoutItemKind::Overlay => LayoutItemKind::Overlay,
                },
                z: item.z,
                transform: LocalTransform {
                    offset: LocalPoint {
                        u: item.transform.offset.u,
                        v: item.transform.offset.v,
                    },
                    size: LocalPoint {
                        u: item.transform.size.u,
                        v: item.transform.size.v,
                    },
                },
                input: match item.input {
                    manifold_wasm::InputBehavior::None => InputBehavior::None,
                    manifold_wasm::InputBehavior::Hit => InputBehavior::Hit,
                    manifold_wasm::InputBehavior::Drag => InputBehavior::Drag,
                    manifold_wasm::InputBehavior::Pane => InputBehavior::Pane,
                },
            })
            .collect(),
    }
}

fn native_flat_layout(panes: &[u64], extent: LayoutSize) -> ResolvedLayout {
    if panes.is_empty() {
        return ResolvedLayout::empty();
    }

    let mut tree = LayoutState::default();
    for _ in 1..panes.len() {
        let active_rect = tree.active_rect(extent).unwrap_or(LayoutRect {
            x: 0,
            y: 0,
            w: extent.w,
            h: extent.h,
        });
        let axis = if active_rect.w >= active_rect.h {
            LayoutAxis::Horizontal
        } else {
            LayoutAxis::Vertical
        };
        let Some(next_leaf) = tree.split_active(axis) else {
            break;
        };
        tree.set_active_leaf(next_leaf);
    }

    ResolvedLayout {
        items: panes
            .iter()
            .copied()
            .zip(tree.resolve(extent))
            .enumerate()
            .map(|(index, (pane, region))| ResolvedLayoutItem {
                id: LayoutItemId(pane),
                kind: LayoutItemKind::Pane { pane },
                z: index as f64,
                transform: LocalTransform::from_pixel_rect(region.rect, extent),
                input: InputBehavior::Pane,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_fallback_is_a_valid_flat_layout() {
        let panes = [11, 12, 13];
        let layout = native_flat_layout(&panes, LayoutSize { w: 1200, h: 800 });
        let validation = layout.validate_for_panes(&panes);
        assert!(validation.is_ok());
        let Ok(layout) = validation else {
            return;
        };
        assert_eq!(
            layout
                .pane_item(11)
                .map(|item| item.pixel_rect(LayoutSize { w: 1200, h: 800 })),
            Some(LayoutRect {
                x: 0,
                y: 0,
                w: 600,
                h: 800,
            })
        );
        assert_eq!(
            layout
                .pane_item(13)
                .map(|item| item.pixel_rect(LayoutSize { w: 1200, h: 800 })),
            Some(LayoutRect {
                x: 600,
                y: 400,
                w: 600,
                h: 400,
            })
        );
    }
}
