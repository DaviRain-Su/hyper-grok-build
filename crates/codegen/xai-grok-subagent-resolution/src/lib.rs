//! Subagent configuration resolution crate.
//!
//! Extracts the pure-logic "resolution" phase of subagent spawning from
//! `xai-grok-shell` into a reusable library. Given a spawn request and a
//! resolution context (roles, personas, parent state), this crate resolves:
//!
//! - Effective runtime config (model, persona, capability mode, isolation)
//!   via precedence: explicit override > role > persona > parent.
//! - Persona instruction loading (inline `instructions` + `instructions_file`).
//! - Role prompt file loading.
//! - Resume identity validation (type/persona match checks; model is soft-ignored).
//!
//! This crate has no dependency on session, coordinator, or transport types.
//! Designed to be consumed by local hosts (e.g. `xai-grok-shell`) and any
//! future remote spawn path that only needs pure resolution logic.
//!
//! ## Planned composition API
//!
//! Future work may add a higher-level composition helper once shell call sites
//! are refactored onto this crate:
//!
//! - `resolve_subagent_spec()` composition function
//! - `SubagentSpec`, `ResolveSubagentRequest`, `ResolutionContext` boundary types
//! - Optional deps for `AgentDefinition` lookup and worktree creation
//! - Model override resolution chain (global > per-type > role > parent)
//! - Capability mode filtering (delegates to `SubagentCapabilityMode::filter_tool_config()`)

pub mod config;
pub mod context;
pub mod overrides;
pub mod resume;
pub mod types;

pub use config::{PersonaIOField, SubagentPersona, SubagentRole};
pub use overrides::{intersect_capability_modes, resolve_effective_overrides};
pub use resume::{ResumeValidationError, validate_resume_identity};
pub use types::{ContextSource, EffectiveRuntimeConfig, ResolutionError, ResumeSourceData};
