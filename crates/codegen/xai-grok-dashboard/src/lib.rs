//! Local, read-only web observability for Grok Build / Hyper sessions.

pub mod api;
pub mod app;
pub mod server;
pub mod store;

pub use server::{DashboardServerConfig, build_router, build_router_with_options, serve};
pub use store::DashboardStore;

pub mod prelude {
    pub use crate::server::DashboardServerConfig;
    pub use crate::store::{
        ChatMessage, DashboardStore, EventKind, LogEntry, ResourceMetrics, ServerMetrics,
        SessionCharts, SessionDetail, SessionMeta, SessionSignals, TimelineEvent,
    };
}
