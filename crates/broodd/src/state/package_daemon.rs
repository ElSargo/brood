//! Ordinary daemon packages remain children of the configured daemon root.
use super::{
    BACKEND_INIT_ACTOR_ID, BACKEND_LIFECYCLE_CAPACITY, BTreeMap, Brood, ConfiguredPluginLifecycle,
    ConfiguredPluginResources, Duration, HostedPluginFilesystem, HostedReplacementPurpose,
    NativeToolAccess, Result, ensure, warn,
};
use brood_cli::package_runtime::contract::Realm;
use brood_wasm::ComponentChildEvent;

impl Brood {
    pub(super) fn daemon_child_owner(&self, actor: &str) -> Option<String> {
        self.backend_init
            .as_ref()?
            .children
            .iter()
            .find_map(|(id, hosted)| (hosted == actor).then(|| id.clone()))
    }

    pub(super) fn send_daemon_child_event(&self, event: ComponentChildEvent) {
        match &event {
            ComponentChildEvent::Started { id, generation } => {
                self.package_host
                    .started(Realm::Daemon, "daemon", id, *generation);
            }
            ComponentChildEvent::Failed { id, message } => {
                self.package_host
                    .outcome(Realm::Daemon, "daemon", id, "failed", Some(message));
            }
            ComponentChildEvent::Stopped { id } => {
                self.package_host
                    .outcome(Realm::Daemon, "daemon", id, "stopped", None);
            }
        }
        if let Some(brood_wasm::HostedWasmActor::Component(root)) =
            self.wasm_io_actors.get(BACKEND_INIT_ACTOR_ID)
            && let Err(error) = root.try_send_child_event(event)
        {
            warn!(%error, "daemon child lifecycle delivery pending");
        }
    }

    pub(super) fn handle_daemon_child_start(
        &mut self,
        id: String,
        path: &str,
        capabilities: &[String],
    ) {
        if let Err(error) = self.start_daemon_child(&id, path, capabilities) {
            self.starting_daemon_children.remove(&id);
            self.send_daemon_child_event(ComponentChildEvent::Failed {
                id,
                message: format!("{error:#}"),
            });
        }
    }

    fn start_daemon_child(
        &mut self,
        id: &str,
        reference: &str,
        capabilities: &[String],
    ) -> Result<()> {
        ensure!(self.backend_init.is_some(), "daemon init actor unavailable");
        let actor_id = format!("brood.daemon.{id}");
        ensure!(
            !self.wasm_io_actors.contains_key(&actor_id)
                && !self.starting_daemon_children.contains(id)
                && !self.stopping_daemon_children.contains_key(&actor_id),
            "daemon child is running, starting, or retiring"
        );
        // This public package path resolves only selected declarations. It cannot
        // promote an ordinary artifact to a realm root or invent host grants.
        let (path, declaration) =
            self.package_host
                .resolve(Realm::Daemon, "daemon", id, reference, capabilities)?;
        self.package_host.starting(
            Realm::Daemon,
            "daemon",
            &actor_id,
            BACKEND_INIT_ACTOR_ID,
            declaration,
        );
        ensure!(
            capabilities.iter().all(|capability| matches!(
                capability.as_str(),
                "filesystem:workspace-read"
                    | "filesystem:workspace-write"
                    | "pty"
                    | "network:loopback"
                    | "computer-use"
            ) || capability
                .strip_prefix("subprocess:")
                .is_some_and(|command| !command.trim().is_empty())),
            "unknown daemon child capability"
        );
        let filesystem =
            crate::config::workspace_filesystem_capability(capabilities).map(|writable| {
                HostedPluginFilesystem {
                    host_path: brood_cli::project::managed_worktree_root(),
                    writable,
                }
            });
        if let Some(filesystem) = &filesystem {
            std::fs::create_dir_all(&filesystem.host_path)?;
        }
        let lifecycle = ConfiguredPluginLifecycle {
            start_timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
            data: BTreeMap::from([("realm".into(), "daemon".into())]),
        };
        let runtime_io = crate::config::runtime_io_capabilities(capabilities)?;
        self.starting_daemon_children.insert(id.into());
        self.replace_wasm_component_actor(
            &actor_id,
            &path,
            filesystem.as_ref(),
            Some(&(lifecycle, brood_wasm::PluginStartReason::Initial)),
            ConfiguredPluginResources::default(),
            runtime_io,
            if capabilities
                .iter()
                .any(|capability| capability == "computer-use")
            {
                NativeToolAccess::ComputerUse
            } else {
                NativeToolAccess::None
            },
            false,
            false,
            None,
            true,
            HostedReplacementPurpose::DaemonChild { id: id.into() },
        )
    }

    pub(super) fn commit_daemon_child(&mut self, id: String, actor_id: &str, generation: u64) {
        self.starting_daemon_children.remove(&id);
        if let Some(root) = &mut self.backend_init {
            root.children.insert(id.clone(), actor_id.into());
        }
        self.send_daemon_child_event(ComponentChildEvent::Started {
            id: id.clone(),
            generation,
        });
        if self.stop_daemon_while_starting.remove(&id) {
            self.handle_daemon_child_stop(id);
        }
    }

    pub(super) fn handle_daemon_child_stop(&mut self, id: String) {
        if self.starting_daemon_children.contains(&id) {
            self.stop_daemon_while_starting.insert(id);
            return;
        }
        let actor_id = self
            .backend_init
            .as_ref()
            .and_then(|root| root.children.get(&id))
            .cloned();
        let Some(actor_id) = actor_id else {
            if self
                .stopping_daemon_children
                .values()
                .any(|child| child == &id)
            {
                return;
            }
            if self.package_host.owns(Realm::Daemon, "daemon", &id) {
                self.send_daemon_child_event(ComponentChildEvent::Stopped { id });
            } else {
                self.send_daemon_child_event(ComponentChildEvent::Failed {
                    id,
                    message: "daemon init may stop only its children".into(),
                });
            }
            return;
        };
        if self.backend_lifecycle_admission.active() >= BACKEND_LIFECYCLE_CAPACITY {
            self.send_daemon_child_event(ComponentChildEvent::Failed {
                id,
                message: "backend lifecycle lane full".into(),
            });
            return;
        }
        let actor = self.wasm_io_actors.remove(&actor_id);
        let lifecycle = self.configured_plugin_lifecycles.remove(&actor_id);
        if let Some(root) = &mut self.backend_init {
            root.children.remove(&id);
        }
        if let Some(actor) = actor {
            self.stopping_daemon_children
                .insert(actor_id.clone(), id.clone());
            self.package_host
                .outcome(Realm::Daemon, "daemon", &id, "stopping", None);
            if let Err(error) = self.spawn_actor_retirement(
                actor_id.clone(),
                actor,
                lifecycle,
                brood_wasm::PluginStopReason::Shutdown,
                None,
            ) {
                self.stopping_daemon_children.remove(&actor_id);
                self.send_daemon_child_event(ComponentChildEvent::Failed {
                    id,
                    message: format!("{error:#}"),
                });
            }
        } else {
            self.send_daemon_child_event(ComponentChildEvent::Stopped { id });
        }
    }
}
