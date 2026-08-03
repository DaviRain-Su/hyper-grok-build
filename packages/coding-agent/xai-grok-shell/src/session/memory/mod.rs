//! Memory system shim.
//!
//! The memory "core engine" now lives in the standalone `xai-grok-memory`
//! crate. This module re-exports that crate's public surface under the
//! historical `crate::session::memory::*` paths so the ~30 reverse-dependency
//! call sites in this crate keep compiling unchanged.
//!
//! Only `hooks` stays here: it is session glue (depends on
//! `crate::sampling` and `crate::session::helpers::session_compact`) and is
//! not part of the relocatable core engine.

pub mod hooks;

pub use xai_grok_memory::{
    EndpointScopedCredentials, MemoryBackendImpl, MemoryBackendParams, MemoryIndex, MemoryScope,
    MemoryStorage, archive, backend, chunker, dream, dream_lock, embed_missing_chunks, embedding,
    index, init_sqlite_vec, mmr, query_expansion, schema, search, storage, text_utils, watcher,
};
