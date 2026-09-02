//! Source-typed message delivery values and operation authorization.

mod envelope;
mod lifecycle;

pub use envelope::{
    AgentSource, AuthorizedOperation, DeliveryEnvelope, DeliveryIdentity, HumanSource, Operation,
    OperationSet, Principal, UnsupportedOperation, authorize_operation,
};
pub use lifecycle::{
    DeliveryMessage, MessageDeliveryLifecycle, OwnedDelivery, TerminalCause, TerminalTarget,
    TerminalTransition, TurnBinding,
};
