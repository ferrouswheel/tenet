use std::collections::HashSet;
use std::time::Duration;

use tenet_crypto::relay::RelayConfig;
use tenet_crypto::simulation::{
    build_simulation_inputs, FriendsPerNode, OnlineAvailability, PlannedSend, PostFrequency,
    SimMessage, SimulationConfig, SimulationHarness, start_relay,
};

#[tokio::test]
async fn simulation_harness_routes_relay_and_direct_with_dedup() {
    let (base_url, shutdown_tx) = start_relay(RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
    })
    .await;

    let config = SimulationConfig {
        node_ids: vec![
            "node-a".to_string(),
            "node-b".to_string(),
            "node-c".to_string(),
        ],
        steps: 6,
        friends_per_node: FriendsPerNode::Uniform { min: 2, max: 2 },
        post_frequency: PostFrequency::WeightedSchedule {
            weights: vec![1.0, 1.0, 2.0, 1.0, 0.5, 0.5],
            total_posts: 3,
        },
        availability: OnlineAvailability::Bernoulli { p_online: 1.0 },
        seed: 42,
    };

    let inputs = build_simulation_inputs(&config);
    let mut harness = SimulationHarness::new(base_url, inputs.nodes, inputs.direct_links, true);

    let mut planned = inputs.planned_sends;
    planned.push(PlannedSend {
        step: 0,
        message: SimMessage {
            id: "message-a".to_string(),
            sender: "node-a".to_string(),
            recipient: "node-b".to_string(),
            body: "relay me".to_string(),
        },
    });
    planned.push(PlannedSend {
        step: 2,
        message: SimMessage {
            id: "message-c".to_string(),
            sender: "node-c".to_string(),
            recipient: "node-b".to_string(),
            body: "direct and relay".to_string(),
        },
    });

    harness.run(config.steps, planned).await;
    shutdown_tx.send(()).ok();

    let inbox = harness.inbox("node-b");
    let mut ids: HashSet<String> = inbox.iter().map(|message| message.id.clone()).collect();

    assert_eq!(ids.len(), inbox.len());
    assert!(ids.remove("message-a"));
    assert!(ids.remove("message-c"));
}
