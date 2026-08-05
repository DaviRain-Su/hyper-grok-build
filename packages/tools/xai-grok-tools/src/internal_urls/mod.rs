//! Internal virtual URL schemes for tools (`agent://`, `history://`, `conflict://`).
//!
//! Single chokepoint used by `read_file` / `grep` when the path has a scheme
//! prefix. Distinct from workspace remote hub / MCP URLs.

mod conflict;
mod resolve;
mod schemes;

pub use conflict::{
    ConflictRegistry, ConflictRegistryResource, ConflictSide, RegisteredConflict, resolve_conflict_write,
};
pub use resolve::{ResolveContext, VirtualRead, apply_line_window, resolve_virtual_path};
pub use schemes::{InternalScheme, InternalUrl, parse_internal_url};
