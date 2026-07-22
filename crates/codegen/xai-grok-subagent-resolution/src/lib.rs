//! Subagent configuration resolution crate.
//!
//! Extracts the pure-logic "resolution" phase of subagent spawning from
//! `xai-grok-shell` into a reusable library. Given a spawn request and a
//! resolution context (roles, personas, parent state), this crate resolves:
//!
//! - Effective runtime config (model, effort, persona, capability mode,
//!   isolation) via explicit > role > persona > definition > parent layering;
//!   capability layers are intersected as security ceilings.
//! - Persona instruction loading (inline `instructions` + `instructions_file`).
//! - Role prompt file loading.
//! - Resume identity validation (type/persona match checks; model is soft-ignored).
//!
//! This crate has no dependency on session, coordinator, or transport types.
//! Designed to be consumed by local hosts (e.g. `xai-grok-shell`) and any
//! future remote spawn path that only needs pure resolution logic.
//!
//! [`resolve_subagent_spec`] is the pure composition boundary for spawn-time,
//! role, persona, and agent-definition runtime defaults. Catalog credentials,
//! filesystem/worktree operations, and session construction remain in the host
//! because they require shell-owned state and I/O.

pub mod config;
pub mod context;
pub mod overrides;
pub mod resume;
pub mod types;

pub use config::{PersonaIOField, SubagentPersona, SubagentRole};
pub use overrides::{
    intersect_capability_modes, resolve_effective_overrides, resolve_subagent_spec,
};
pub use resume::{ResumeValidationError, validate_resume_identity};
pub use types::{
    ContextSource, DefinitionRuntimeDefaults, EffectiveRuntimeConfig, ResolutionError,
    ResumeSourceData,
};
