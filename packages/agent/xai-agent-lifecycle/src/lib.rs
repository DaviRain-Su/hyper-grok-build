//! Host-agnostic agent lifecycle hooks shared by multiple agent hosts (e.g. xai-grok-shell).
//! Contributors receive data-only per-hook inputs at dispatch time; anything they act through is a
//! capability injected at install time, and they never own loop control.

pub mod local;
pub mod send;

pub use local::{
    LocalCommandContributor, LocalExtensionRegistry, LocalExtensionRegistryBuilder,
    LocalSessionLifecycleContributor, LocalTurnInputContributor, LocalTurnLifecycleContributor,
};
pub use send::{
    CommandAction, CommandContributor, CommandInvocation, CommandSpec, ExtensionRegistry,
    ExtensionRegistryBuilder, SessionIdleInput, SessionLifecycleContributor, TurnAbortInput,
    TurnAbortReason, TurnDoneInput, TurnErrorInput, TurnInputContext, TurnInputContributor,
    TurnInputFragment, TurnLifecycleContributor, TurnStartInput,
};
