use tokio_agent_extension_api::{
    ExtensionAction, ExtensionId, HostRequest, HostResponse, SessionEvent, TimerId,
};

#[test]
fn all_newtype_protocol_variants_round_trip() {
    let requests = [
        HostRequest::SessionEvent(tokio_agent_extension_api::Sequenced {
            sequence: 1,
            extension: ExtensionId::new("example.test.runtime"),
            generation: 2,
            value: SessionEvent::TimerFired {
                id: TimerId::new("timer"),
            },
        }),
        HostRequest::Shutdown,
    ];
    for request in requests {
        let json = serde_json::to_string(&request).unwrap();
        let decoded: HostRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    let response = HostResponse::Actions(vec![tokio_agent_extension_api::Sequenced {
        sequence: 3,
        extension: ExtensionId::new("example.test.runtime"),
        generation: 2,
        value: ExtensionAction::CancelTimer(TimerId::new("timer")),
    }]);
    let json = serde_json::to_string(&response).unwrap();
    assert_eq!(
        serde_json::from_str::<HostResponse>(&json).unwrap(),
        response
    );
}
