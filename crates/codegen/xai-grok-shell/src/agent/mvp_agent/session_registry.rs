//! Per-session resources and the registry that owns them. Distinct from
//! `agent::session_registry_client`, which talks to the remote registry.
use super::*;
/// The map stays private so every caller goes through a named operation.
#[derive(Clone, Default)]
pub(super) struct SessionRegistry {
    sessions: Rc<RefCell<HashMap<acp::SessionId, SessionResources>>>,
}
/// The per-session state this registry owns: retained, resident, thread, live,
/// unavailable model, and bridge. Load guards, rewind snapshots, local
/// workspaces, and the handle map are owned elsewhere, so a new field belongs
/// here only if `release` should drop it with the rest.
#[derive(Default)]
struct SessionResources {
    retained: Option<RetainedResources>,
    /// Cleared at idle-unload; survives a reload rebuild.
    resident: Option<ResidentResources>,
    thread: Option<SessionThread>,
    live: Option<SessionLiveState>,
    unavailable_model: Option<acp::ModelId>,
}
#[derive(Default)]
pub(super) struct SessionCounts {
    pub(super) retained_resources: usize,
    pub(super) resident_resources: usize,
    pub(super) session_threads: usize,
    pub(super) session_live_state: usize,
    pub(super) model_unavailable_sessions: usize,
    pub(super) dispatch_locks: usize,
    pub(super) session_turn_numbers: usize,
    pub(super) permission_event_receivers: usize,
    pub(super) session_index_claims: usize,
    pub(super) require_gateway_sessions: usize,
}
impl SessionRegistry {
    /// Releases everything a closing session leaves behind, in one drop.
    ///
    /// A running actor thread stays: dropping its handle would detach it, and
    /// nothing would track the memory it holds. The sweep reclaims it later.
    pub(super) fn release(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        let Some(mut released) = entries.remove(id) else {
            return;
        };
        let running = released.thread.take().filter(|t| !t.is_finished());
        drop(released);
        if running.is_some() {
            entries.insert(
                id.clone(),
                SessionResources {
                    thread: running,
                    retained: None,
                    resident: None,
                    live: None,
                    unavailable_model: None,
                },
            );
        }
    }
    pub(super) fn set_thread(&self, id: &acp::SessionId, thread: SessionThread) {
        let displaced = self.edit(id, |e| e.thread.replace(thread));
        if displaced.is_some_and(|t| !t.is_finished()) {
            tracing::warn!(session_id = %id.0, "session thread displaced while still running");
        }
    }
    /// Drops the tracked thread. Returns nothing on purpose: handing a
    /// `SessionThread` to a caller lets the last handle die in a local, which
    /// detaches the thread with no record left for the sweep.
    pub(super) fn clear_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| e.thread = None);
    }
    /// `None` when no thread is tracked for the session.
    #[cfg(test)]
    pub(super) fn has_thread(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| e.thread.is_some()).unwrap_or(false)
    }
    pub(super) fn thread_is_finished(&self, id: &acp::SessionId) -> Option<bool> {
        self.with(id, |e| e.thread.as_ref().map(SessionThread::is_finished))
            .flatten()
    }
    pub(super) fn finished_threads(&self) -> Vec<acp::SessionId> {
        self.sessions
            .borrow()
            .iter()
            .filter(|(_, e)| e.thread.as_ref().is_some_and(SessionThread::is_finished))
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(super) fn clear_exited_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            e.thread = None;
            e.live = None;
        });
    }
    pub(super) fn set_live(&self, id: &acp::SessionId, state: SessionLiveState) {
        self.edit(id, |e| e.live = Some(state));
    }
    pub(super) fn live(&self, id: &acp::SessionId) -> Option<SessionLiveState> {
        self.with(id, |e| e.live).flatten()
    }
    pub(super) fn clear_resident(&self, id: &acp::SessionId) {
        self.clear(id, |e| e.resident = None);
    }
    pub(super) fn set_unavailable_model(&self, id: &acp::SessionId, model: acp::ModelId) {
        self.edit(id, |e| e.unavailable_model = Some(model));
    }
    pub(super) fn unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        self.with(id, |e| e.unavailable_model.clone()).flatten()
    }
    pub(super) fn take_unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        let model = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|e| e.unavailable_model.take());
        self.drop_if_empty(id);
        model
    }
    pub(super) fn turn_number(&self, id: &acp::SessionId) -> Option<u64> {
        self.with(id, |e| e.retained.as_ref()?.turn_number)
            .flatten()
    }
    pub(super) fn set_turn_number(&self, id: &acp::SessionId, next: u64) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().turn_number = Some(next);
        });
    }
    pub(super) fn dispatch_lock(&self, id: &acp::SessionId) -> Rc<tokio::sync::Mutex<()>> {
        self.edit(id, |e| {
            e.retained
                .get_or_insert_default()
                .dispatch_lock
                .get_or_insert_with(Default::default)
                .clone()
        })
    }
    pub(super) fn set_permission_receiver(
        &self,
        id: &acp::SessionId,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionEvent>,
    ) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().permission_event_receiver = Some(rx);
        });
    }
    pub(super) fn drain_permission_events(&self, id: &acp::SessionId) -> Vec<PermissionEvent> {
        let mut events = Vec::new();
        let mut entries = self.sessions.borrow_mut();
        if let Some(rx) = entries
            .get_mut(id)
            .and_then(|e| e.retained.as_mut())
            .and_then(|r| r.permission_event_receiver.as_mut())
        {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events
    }
    pub(super) fn set_codebase_index(
        &self,
        id: &acp::SessionId,
        index: std::sync::Arc<xai_codebase_graph::IndexManagerHandle>,
    ) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().codebase_index = Some(index);
        });
    }

    /// Mark a session as requiring a gateway connection to drive its turn loop
    /// (vs. a self-contained local session). Tracked via the
    /// `require_gateway_sessions` count surfaced to the leader's auto-update
    /// checker.
    #[cfg(test)]
    pub(super) fn mark_require_gateway(&self, id: &acp::SessionId) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().require_gateway = true;
        });
    }
    /// Destructured so a new field has to be counted, or go unmeasured.
    pub(super) fn counts(&self) -> SessionCounts {
        let mut counts = SessionCounts::default();
        for entry in self.sessions.borrow().values() {
            let SessionResources {
                retained,
                resident,
                thread,
                live,
                unavailable_model,
            } = entry;
            counts.retained_resources += usize::from(retained.is_some());
            counts.resident_resources += usize::from(resident.is_some());
            counts.session_threads += usize::from(thread.is_some());
            counts.session_live_state += usize::from(live.is_some());
            counts.model_unavailable_sessions += usize::from(unavailable_model.is_some());
            if let Some(retained) = retained {
                counts.dispatch_locks += usize::from(retained.dispatch_lock.is_some());
                counts.session_turn_numbers += usize::from(retained.turn_number.is_some());
                counts.permission_event_receivers +=
                    usize::from(retained.permission_event_receiver.is_some());
            }
            if let Some(resident) = resident {
                counts.session_index_claims += usize::from(resident.codebase_index.is_some());
                counts.require_gateway_sessions += usize::from(resident.require_gateway);
            }
        }
        counts
    }
    fn with<R>(&self, id: &acp::SessionId, f: impl FnOnce(&SessionResources) -> R) -> Option<R> {
        self.sessions.borrow().get(id).map(f)
    }
    fn edit<R>(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources) -> R) -> R {
        f(self.sessions.borrow_mut().entry(id.clone()).or_default())
    }
    fn clear(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources)) {
        {
            let mut entries = self.sessions.borrow_mut();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            f(entry);
        }
        self.drop_if_empty(id);
    }
    fn drop_if_empty(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        if entries.get(id).is_some_and(SessionResources::is_empty) {
            entries.remove(id);
        }
    }
}
impl SessionResources {
    fn is_empty(&self) -> bool {
        let Self {
            retained,
            resident,
            thread,
            live,
            unavailable_model,
        } = self;
        let chat_vacant = true;
        retained.is_none()
            && resident.is_none()
            && thread.is_none()
            && live.is_none()
            && unavailable_model.is_none()
            && chat_vacant
    }
}
