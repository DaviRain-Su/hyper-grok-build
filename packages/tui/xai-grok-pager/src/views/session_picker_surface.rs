/// Which surface a picker fetch was issued for.
/// Results route back to the requesting host's storage only; a live picker on another host never absorbs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerHost {
    /// Welcome-screen picker (`session_picker_*` fields on `AppView`).
    Welcome,
    /// `/resume` modal on the active agent (`ActiveModal::SessionPicker`).
    AgentModal,
    /// Dashboard picker (`AppView::dashboard_session_picker`).
    Dashboard,
}

/// State for one session-picker incarnation.
/// Host-agnostic: everything a picker accumulates between open and dismiss, nothing about how a host renders it or maps its keys.
#[derive(Debug)]
pub struct SessionPickerSurface {
    /// Incarnation identity; results apply only when it matches.
    pub generation: u64,
    pub state: crate::views::picker::PickerState,
    pub entries: Option<Vec<crate::app::app_view::SessionPickerEntry>>,
    pub loading: bool,
    pub lanes: crate::views::session_picker::SessionPickerLanes,
    pub content_results: Option<Vec<xai_grok_shell::extensions::session_search::SearchSessionHit>>,
    pub content_loading: bool,
    /// Per-surface counters; the dashboard host does not share the welcome picker's `session_picker_list_seq` / `session_picker_deep_search_seq`.
    pub list_seq: u64,
    pub deep_search_seq: u64,
    /// Invalidates in-flight card-detail reads when this surface's rows or filters change.
    pub detail_seq: u64,
    pub entries_query: Option<String>,
    pub source_filter: crate::views::session_picker::SourceFilter,
    pub pending_delete: Option<crate::views::session_picker::PendingDelete>,
}

impl SessionPickerSurface {
    #[must_use]
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            state: crate::views::picker::PickerState::default(),
            entries: None,
            loading: false,
            lanes: Default::default(),
            content_results: None,
            content_loading: false,
            list_seq: 0,
            deep_search_seq: 0,
            detail_seq: 0,
            entries_query: None,
            source_filter: Default::default(),
            pending_delete: None,
        }
    }
}
