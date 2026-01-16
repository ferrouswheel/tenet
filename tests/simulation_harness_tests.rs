use std::collections::HashSet;
use std::time::Duration;

use tenet::protocol::{build_plaintext_payload, ContentId};
use tenet::relay::RelayConfig;
use tenet::simulation::{
    build_simulation_inputs, start_relay, FriendsPerNode, MessageEncryption,
    MessageSizeDistribution, OnlineAvailability, PlannedSend, PostFrequency, SimMessage,
    SimulationConfig, SimulationHarness,
};

#[tokio::test]
async fn simulation_harness_routes_relay_and_direct_with_dedup() {
    let relay_config = RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
    };
    let (base_url, shutdown_tx) = start_relay(relay_config.clone()).await;

    let config = SimulationConfig {
        node_ids: vec![
            "node-a".to_string(),
            "node-b".to_string(),
            "node-c".to_string(),
        ],
        steps: 6,
        friends_per_node: FriendsPerNode::Uniform { min: 2, max: 2 },
        clustering: None,
        post_frequency: PostFrequency::WeightedSchedule {
            weights: vec![1.0, 1.0, 2.0, 1.0, 0.5, 0.5],
            total_posts: 3,
        },
        availability: OnlineAvailability::Bernoulli { p_online: 1.0 },
        message_size_distribution: MessageSizeDistribution::Uniform { min: 8, max: 32 },
        encryption: Some(MessageEncryption::Plaintext),
        seed: 42,
    };

    let inputs = build_simulation_inputs(&config);
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.nodes,
        inputs.direct_links,
        true,
        relay_config.ttl.as_secs(),
        inputs.encryption,
        inputs.keypairs,
    );

    let mut planned = inputs.planned_sends;
    let payload = build_plaintext_payload("relay me", b"message-a");
    let message_id = ContentId::from_value(&payload).expect("serialize plaintext payload");
    let message_a_id = message_id.0.clone();
    planned.push(PlannedSend {
        step: 0,
        message: SimMessage {
            id: message_id.0,
            sender: "node-a".to_string(),
            recipient: "node-b".to_string(),
            body: "relay me".to_string(),
            payload,
        },
    });
    let payload = build_plaintext_payload("direct and relay", b"message-c");
    let message_id = ContentId::from_value(&payload).expect("serialize plaintext payload");
    let message_c_id = message_id.0.clone();
    planned.push(PlannedSend {
        step: 2,
        message: SimMessage {
            id: message_id.0,
            sender: "node-c".to_string(),
            recipient: "node-b".to_string(),
            body: "direct and relay".to_string(),
            payload,
        },
    });

    harness.run(config.steps, planned).await;
    shutdown_tx.send(()).ok();

    let inbox = harness.inbox("node-b");
    let mut ids: HashSet<String> = inbox.iter().map(|message| message.id.clone()).collect();

    assert_eq!(ids.len(), inbox.len());
    assert!(ids.remove(&message_a_id));
    assert!(ids.remove(&message_c_id));
}
