//! SQLite-backed persistent dashboard workspace: membership, layout ranks,
//! and grouping.
//!
//! # Ownership contract
//!
//! This crate owns every byte of workspace-store SQL. No other code may
//! touch the database for this feature: consumers get the typed API and
//! nothing else. Every statement names its columns; decode indexes refer to
//! that explicit projection, never table order. Additive schema evolution
//! therefore cannot clobber columns a binary does not know.
//!
//! # Invariants
//!
//! - Every [`SessionId`] in the program is a validated single path
//!   component; ids joined into filesystem paths cannot traverse.
//! - `origin` is written once at insert; no API can rewrite it.
//! - Membership is capped at [`WORKSPACE_CAPACITY`]; inserting at capacity
//!   evicts the least-recently-changed unpinned members in the same
//!   transaction, and refuses with a typed error when every member is
//!   pinned.
//! - Unknown enum text written by newer binaries round-trips byte-identical.
//!
//! # Consumer contracts
//!
//! - Own **one** [`WorkspaceStore`] per process and route every read and
//!   write through it: `data_version`'s self-vs-foreign discrimination is
//!   per-connection, so a second handle in the same process would look like
//!   a foreign writer.
//! - [`WorkspaceStore::open`] creates the directory and file; callers that
//!   must not create the store (e.g. while the feature is disabled) must
//!   not call it.
//! - A corrupt or newer-schema file is never deleted, truncated, or
//!   quarantined; the typed errors ([`StoreError::Unusable`],
//!   [`StoreError::NewerSchema`]) leave recovery to the user.

use std::path::{Path, PathBuf};

mod error;
mod owner_only;
mod schema;
mod store;
mod store_open;
#[cfg(test)]
mod test_support;
mod types;

pub use error::{Result, StoreError};
pub use schema::USER_VERSION;
pub use store::WorkspaceStore;
pub use types::{
    Grouping, InsertOutcome, MAX_CWD_BYTES, MAX_ENUM_BYTES, MAX_MODEL_BYTES, MAX_SESSION_ID_BYTES,
    MAX_SUMMARY_BYTES, MAX_TITLE_BYTES, Member, MemberKey, MemberKind, MemberMetadata,
    MemberOrigin, NewMember, RANK_GAP, RankAssignment, RekeyOutcome, RemoveOutcome, SchemaState,
    SessionId, UnknownGrouping, UnknownMemberKind, UnknownMemberOrigin, WORKSPACE_CAPACITY,
    WorkspaceSnapshot,
};

/// The canonical store path under a grok home. The caller resolves the grok
/// home (nothing in this crate reads `GROK_HOME`); tests pass a temp dir.
/// The dedicated subdirectory is created 0700 by [`WorkspaceStore::open`]
/// and contains every sibling file the store ever creates.
pub fn default_db_path(grok_home: &Path) -> PathBuf {
    grok_home.join("dashboard").join("workspace.db")
}
