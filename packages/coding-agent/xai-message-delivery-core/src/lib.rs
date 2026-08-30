//! Source-typed message delivery values and operation authorization.

mod envelope;

pub use envelope::{
    AgentSource, AuthorizedOperation, DeliveryEnvelope, DeliveryIdentity, HumanSource, Operation,
    OperationSet, Principal, UnsupportedOperation, authorize_operation,
};
