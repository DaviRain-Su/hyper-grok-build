use super::*;

struct OpaqueGrant(u64);

#[test]
fn marker_constructors_set_principal_and_preserve_grant() {
    assert_eq!(
        DeliveryEnvelope::from_human(Operation::Queue, (), (), OpaqueGrant(11)).principal(),
        Principal::Human
    );
    let agent = DeliveryEnvelope::from_agent(Operation::Queue, (), (), OpaqueGrant(17));
    assert_eq!(agent.principal(), Principal::Agent);
    assert_eq!(agent.into_parts().3.0, 17);
}

#[test]
fn queue_operation_set_authorizes_queue_and_rejects_steer() {
    assert!(authorize_operation(OperationSet::QUEUE, Operation::Queue).is_ok());
    assert!(authorize_operation(OperationSet::QUEUE, Operation::Steer).is_err());
}
