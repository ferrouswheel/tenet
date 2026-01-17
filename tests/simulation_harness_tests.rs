use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tenet::crypto::generate_keypair;
use tenet::protocol::{build_plaintext_payload, ContentId};
use tenet::relay::RelayConfig;
use tenet::simulation::{
    build_simulation_inputs, start_relay, FriendsPerNode, MessageEncryption,
    MessageSizeDistribution, OnlineAvailability, PlannedSend, PostFrequency, SimMessage,
    SimulatedTimeConfig, SimulationClient, SimulationConfig, SimulationControlCommand,
    SimulationHarness, SimulationTimingConfig,
};
use tokio::sync::mpsc;

#[tokio::test]
async fn simulation_harness_routes_relay_and_direct_with_dedup() {
    let relay_config = RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let config = SimulationConfig {
        node_ids: vec![
            "node-a".to_string(),
            "node-b".to_string(),
            "node-c".to_string(),
        ],
        steps: 6,
        duration_seconds: None,
        simulated_time: SimulatedTimeConfig {
            seconds_per_step: 60,
            default_speed_factor: 1.0,
        },
        friends_per_node: FriendsPerNode::Uniform { min: 2, max: 2 },
        clustering: None,
        post_frequency: PostFrequency::WeightedSchedule {
            weights: vec![1.0, 1.0, 2.0, 1.0, 0.5, 0.5],
            total_posts: 3,
        },
        availability: Some(OnlineAvailability::Bernoulli { p_online: 1.0 }),
        cohorts: Vec::new(),
        message_size_distribution: MessageSizeDistribution::Uniform { min: 8, max: 32 },
        encryption: Some(MessageEncryption::Plaintext),
        seed: 42,
    };

    let inputs = build_simulation_inputs(&config);
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.clients,
        inputs.direct_links,
        true,
        relay_config.ttl.as_secs(),
        inputs.encryption,
        inputs.keypairs,
        inputs.timing,
        inputs.cohort_online_rates,
        None,
        None,
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

#[tokio::test]
async fn simulation_harness_tracks_online_handshake_metrics() {
    let relay_config = RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let clients = vec![
        tenet::simulation::SimulationClient::new("node-a", vec![false, true]),
        tenet::simulation::SimulationClient::new("node-b", vec![true, true]),
    ];
    let mut direct_links = HashSet::new();
    direct_links.insert(("node-a".to_string(), "node-b".to_string()));
    direct_links.insert(("node-b".to_string(), "node-a".to_string()));

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        Default::default(),
        SimulationTimingConfig {
            base_seconds_per_step: 60.0,
            speed_factor: 1.0,
            base_real_time_per_step: Duration::ZERO,
        },
        HashMap::new(),
        None,
        None,
    );

    harness.run(2, Vec::new()).await;
    shutdown_tx.send(()).ok();

    let report = harness.metrics_report();
    assert_eq!(report.online_broadcasts_sent, 2);
    assert_eq!(report.acks_received, 2);
    assert_eq!(report.missed_message_requests_sent, 2);
    assert_eq!(report.missed_message_deliveries, 0);
}

#[tokio::test]
async fn simulation_harness_delivers_missed_messages_after_handshake() {
    let relay_config = RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 0,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let clients = vec![
        tenet::simulation::SimulationClient::new("node-a", vec![false, true, true]),
        tenet::simulation::SimulationClient::new("node-b", vec![true, true, true]),
    ];
    let mut direct_links = HashSet::new();
    direct_links.insert(("node-a".to_string(), "node-b".to_string()));
    direct_links.insert(("node-b".to_string(), "node-a".to_string()));

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        Default::default(),
        SimulationTimingConfig {
            base_seconds_per_step: 60.0,
            speed_factor: 1.0,
            base_real_time_per_step: Duration::ZERO,
        },
        HashMap::new(),
        None,
        None,
    );

    let payload = build_plaintext_payload("missed", b"missed-message");
    let message_id = ContentId::from_value(&payload).expect("serialize payload");
    let planned = vec![PlannedSend {
        step: 0,
        message: SimMessage {
            id: message_id.0.clone(),
            sender: "node-b".to_string(),
            recipient: "node-a".to_string(),
            body: "missed".to_string(),
            payload,
        },
    }];

    harness.run(3, planned).await;
    shutdown_tx.send(()).ok();

    let inbox = harness.inbox("node-a");
    assert!(inbox.iter().any(|message| message.id == message_id.0));

    let report = harness.metrics_report();
    assert_eq!(report.missed_message_deliveries, 1);
    assert_eq!(report.missed_message_requests_sent, 2);
    assert_eq!(report.acks_received, 2);
}

#[tokio::test]
async fn simulation_harness_applies_dynamic_updates() {
    let relay_config = RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let clients = vec![
        SimulationClient::new("node-a", vec![true; 4]),
        SimulationClient::new("node-b", vec![true; 4]),
    ];
    let mut direct_links = HashSet::new();
    direct_links.insert(("node-a".to_string(), "node-b".to_string()));
    direct_links.insert(("node-b".to_string(), "node-a".to_string()));

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        Default::default(),
        SimulationTimingConfig {
            base_seconds_per_step: 60.0,
            speed_factor: 1.0,
            base_real_time_per_step: Duration::ZERO,
        },
        HashMap::new(),
        None,
        None,
    );

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let node_c = SimulationClient::new("node-c", vec![true; 4]);
    let _ = control_tx.send(SimulationControlCommand::AddPeer {
        client: node_c,
        keypair: generate_keypair(),
    });
    let _ = control_tx.send(SimulationControlCommand::AddFriendship {
        peer_a: "node-a".to_string(),
        peer_b: "node-c".to_string(),
    });
    let _ = control_tx.send(SimulationControlCommand::AdjustSpeedFactor { delta: 0.5 });
    drop(control_tx);

    let mut last_update = None;
    harness
        .run_with_progress_and_controls(4, Vec::new(), control_rx, |update| {
            last_update = Some(update);
        })
        .await;
    shutdown_tx.send(()).ok();

    let update = last_update.expect("expected at least one update");
    assert_eq!(harness.total_nodes(), 3);
    assert!(update.speed_factor > 1.0);
}
