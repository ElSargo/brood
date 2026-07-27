use std::collections::{BTreeMap, BTreeSet};

use brood_protocol::BackendId;

/// A pane address qualified by the backend that owns its numeric ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QualifiedPane {
    pub backend: BackendId,
    pub pane: u64,
}

/// Client-side inventory of independently connected backend identities.
///
/// Transport handles remain owned by the event loop for now. This registry is
/// deliberately transport-neutral so view state can stop using bare backend-
/// local numbers before connection ownership is moved into the same map.
#[derive(Clone, Debug, Default)]
pub struct BackendRegistry {
    backends: BTreeMap<BackendId, BackendState>,
    view_panes: BTreeMap<QualifiedPane, u64>,
    backend_panes: BTreeMap<u64, QualifiedPane>,
    next_view_pane: u64,
    next_backend: u64,
}

#[derive(Clone, Debug, Default)]
struct BackendState {
    generation: Option<u64>,
    panes: BTreeSet<u64>,
}

impl BackendRegistry {
    pub(crate) fn allocate(&mut self, generation: Option<u64>) -> BackendId {
        loop {
            let backend = BackendId(self.next_backend.max(1));
            self.next_backend = backend.0.saturating_add(1);
            if !self.backends.contains_key(&backend) {
                self.observe(backend, generation);
                return backend;
            }
        }
    }

    pub(crate) fn observe(&mut self, backend: BackendId, generation: Option<u64>) {
        let state = self.backends.entry(backend).or_default();
        if generation.is_some() {
            state.generation = generation;
        }
    }

    pub(crate) fn replace_panes(
        &mut self,
        backend: BackendId,
        generation: Option<u64>,
        panes: impl IntoIterator<Item = u64>,
    ) {
        self.observe(backend, generation);
        if let Some(state) = self.backends.get_mut(&backend) {
            state.panes = panes.into_iter().collect();
        }
    }

    /// Return the stable presentation-local handle for one backend pane.
    pub(crate) fn view_pane(&mut self, pane: QualifiedPane) -> u64 {
        if let Some(view_pane) = self.view_panes.get(&pane) {
            return *view_pane;
        }
        let view_pane = self.next_view_pane.max(1);
        self.next_view_pane = view_pane.saturating_add(1);
        self.view_panes.insert(pane, view_pane);
        self.backend_panes.insert(view_pane, pane);
        view_pane
    }

    pub(crate) fn backend_pane(&self, view_pane: u64) -> Option<QualifiedPane> {
        self.backend_panes.get(&view_pane).copied()
    }

    pub(crate) fn all_view_routes(&self) -> Vec<(u64, BackendId, u64)> {
        self.view_panes
            .iter()
            .filter_map(|(qualified, view)| {
                self.backends
                    .get(&qualified.backend)
                    .is_some_and(|state| state.panes.contains(&qualified.pane))
                    .then_some((*view, qualified.backend, qualified.pane))
            })
            .collect()
    }

    #[cfg(test)]
    fn contains(&self, pane: QualifiedPane) -> bool {
        self.backends
            .get(&pane.backend)
            .is_some_and(|state| state.panes.contains(&pane.pane))
    }

    pub(crate) fn backend_count(&self) -> usize {
        self.backends.len()
    }

    #[cfg(test)]
    fn generation(&self, backend: BackendId) -> Option<u64> {
        self.backends
            .get(&backend)
            .and_then(|state| state.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendRegistry, QualifiedPane};
    use brood_protocol::BackendId;

    #[test]
    fn same_pane_number_is_isolated_by_backend() {
        let a = BackendId(10);
        let b = BackendId(20);
        let mut registry = BackendRegistry::default();
        registry.replace_panes(a, Some(1), [7]);
        registry.replace_panes(b, Some(2), [7]);

        assert!(registry.contains(QualifiedPane {
            backend: a,
            pane: 7
        }));
        assert!(registry.contains(QualifiedPane {
            backend: b,
            pane: 7
        }));
        assert_eq!(registry.backend_count(), 2);
        assert_eq!(registry.generation(a), Some(1));
        assert_eq!(registry.generation(b), Some(2));
    }

    #[test]
    fn replacing_one_backend_does_not_remove_another() {
        let a = BackendId(10);
        let b = BackendId(20);
        let mut registry = BackendRegistry::default();
        registry.replace_panes(a, None, [1, 2]);
        registry.replace_panes(b, None, [3]);
        registry.view_pane(QualifiedPane {
            backend: a,
            pane: 1,
        });
        registry.view_pane(QualifiedPane {
            backend: b,
            pane: 3,
        });
        registry.replace_panes(a, None, [4]);
        registry.view_pane(QualifiedPane {
            backend: a,
            pane: 4,
        });

        assert!(!registry.contains(QualifiedPane {
            backend: a,
            pane: 1
        }));
        assert!(registry.contains(QualifiedPane {
            backend: a,
            pane: 4
        }));
        assert!(registry.contains(QualifiedPane {
            backend: b,
            pane: 3
        }));
        assert_eq!(registry.all_view_routes().len(), 2);
    }

    #[test]
    fn view_handles_are_stable_and_reverse_resolvable() {
        let qualified = QualifiedPane {
            backend: BackendId(10),
            pane: 7,
        };
        let mut registry = BackendRegistry::default();
        let view = registry.view_pane(qualified);

        assert_eq!(registry.view_pane(qualified), view);
        assert_eq!(registry.backend_pane(view), Some(qualified));
    }
}
