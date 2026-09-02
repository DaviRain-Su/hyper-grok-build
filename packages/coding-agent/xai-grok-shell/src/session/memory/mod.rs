//! Memory system shim.
//!
//! The memory "core engine" now lives in the standalone `xai-grok-memory` crate.
//! This module re-exports that crate's public API under the historical `crate::session::memory::*` paths.
//!
//! Only `hooks` stays here: it is session glue (depends on `crate::sampling` and `crate::session::helpers::session_compact`).

pub mod hooks;

pub use xai_grok_memory::{
    EndpointScopedCredentials, MemoryBackendImpl, MemoryBackendParams, MemoryIndex, MemoryScope,
    MemorySearchSource, MemoryStorage, archive, backend, chunker, dream, dream_lock,
    embed_missing_chunks, embedding, index, init_sqlite_vec, mmr, noop_memory_observation_sink,
    query_expansion, schema, search, storage, text_utils, watcher,
};
