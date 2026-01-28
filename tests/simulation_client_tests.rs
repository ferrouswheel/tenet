use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tenet::client::{ClientLogEvent, ClientLogSink};
use tenet::crypto::generate_keypair;
use tenet::protocol::{build_plaintext_payload, ContentId};
use tenet::relay::RelayConfig;
use tenet::simulation::{
    start_relay, MessageEncryption, PlannedSend, SimMessage, SimulationClient, SimulationHarness,
    SimulationTimingConfig,
};

fn relay_config() -> RelayConfig {
    RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

fn build_client_log_sink(events: &Arc<Mutex<Vec<ClientLogEvent>>>) -> Arc<dyn ClientLogSink> {
    let events = Arc::clone(events);
    Arc::new(move |event: ClientLogEvent| {
        events.lock().expect("lock client logs").push(event);
    })
}

fn insert_links(links: &mut HashSet<(String, String)>, pairs: &[(&str, &str)]) {
    for (from, to) in pairs {
        links.insert(((*from).to_string(), (*to).to_string()));
    }
}

#[tokio::test]
async fn simulation_client_direct_and_store_forward_paths_increment_metrics() {
    let relay_config = relay_config();
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let log_sink = build_client_log_sink(&events);
    let clients = vec![
        SimulationClient::new("node-a", vec![true, true], Some(log_sink.clone())),
        SimulationClient::new("node-b", vec![false, true], Some(log_sink.clone())),
        SimulationClient::new("node-c", vec![true, true], Some(log_sink.clone())),
    ];
    let mut direct_links = HashSet::new();
    insert_links(
        &mut direct_links,
        &[
            ("node-a", "node-b"),
            ("node-b", "node-a"),
            ("node-a", "node-c"),
            ("node-c", "node-a"),
            ("node-b", "node-c"),
            ("node-c", "node-b"),
        ],
    );
    let mut keypairs = HashMap::new();
    keypairs.insert("node-a".to_string(), generate_keypair());
    keypairs.insert("node-b".to_string(), generate_keypair());
    keypairs.insert("node-c".to_string(), generate_keypair());

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        keypairs,
        SimulationTimingConfig {
            base_seconds_per_step: 60.0,
            speed_factor: 1.0,
            base_real_time_per_step: Duration::ZERO,
        },
        HashMap::new(),
        None,
        None,
    );

    let payload = build_plaintext_payload("store-forward", b"message-1");
    let message_id = ContentId::from_value(&payload).expect("serialize payload");
    let direct_payload = build_plaintext_payload("direct", b"message-2");
    let direct_message_id = ContentId::from_value(&direct_payload).expect("serialize payload");
    let planned = vec![
        PlannedSend {
            step: 0,
            message: SimMessage {
                id: message_id.0.clone(),
                sender: "node-a".to_string(),
                recipient: "node-b".to_string(),
                body: "store-forward".to_string(),
                payload,
            },
        },
        PlannedSend {
            step: 1,
            message: SimMessage {
                id: direct_message_id.0.clone(),
                sender: "node-a".to_string(),
                recipient: "node-b".to_string(),
                body: "direct".to_string(),
                payload: direct_payload,
            },
        },
    ];

    harness.run(2, planned).await;
    shutdown_tx.send(()).ok();

    let inbox = harness.inbox("node-b");
    assert_eq!(inbox.len(), 2);
    assert!(inbox.iter().any(|message| message.id == message_id.0));
    assert!(inbox
        .iter()
        .any(|message| message.id == direct_message_id.0));

    let report = harness.metrics_report();
    assert_eq!(report.store_forwards_stored, 1);
    assert_eq!(report.store_forwards_forwarded, 1);
    assert_eq!(report.store_forwards_delivered, 1);
    let client_metrics = &report.aggregate_metrics.client_metrics;
    assert_eq!(client_metrics["node-a"].messages_sent, 2);
    assert_eq!(client_metrics["node-b"].direct_deliveries, 1);
    assert_eq!(client_metrics["node-b"].store_forwards_delivered, 1);
    assert_eq!(client_metrics["node-c"].store_forwards_stored, 1);
    assert_eq!(client_metrics["node-c"].store_forwards_forwarded, 1);

    let events = events.lock().expect("lock client logs");
    assert!(events.iter().any(|event| event.client_id == "node-c"
        && event.message.contains("stored a store-forward message")));
    assert!(events
        .iter()
        .any(|event| event.client_id == "node-c"
            && event.message.contains("forwarded stored message")));
}

#[tokio::test]
async fn simulation_client_online_handshake_updates_metrics() {
    let relay_config = relay_config();
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let log_sink = build_client_log_sink(&events);
    let clients = vec![
        SimulationClient::new("node-a", vec![false, true], Some(log_sink.clone())),
        SimulationClient::new("node-b", vec![true, true], Some(log_sink.clone())),
    ];
    let mut direct_links = HashSet::new();
    insert_links(
        &mut direct_links,
        &[("node-a", "node-b"), ("node-b", "node-a")],
    );

    let mut keypairs = HashMap::new();
    keypairs.insert("node-a".to_string(), generate_keypair());
    keypairs.insert("node-b".to_string(), generate_keypair());

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        keypairs,
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
    assert!(!events.lock().expect("lock client logs").is_empty());
}

#[tokio::test]
async fn client_log_sink_adapter_forwards_to_simulation_logger() {
    let relay_config = relay_config();
    let (base_url, shutdown_tx, _relay_control) = start_relay(relay_config.clone()).await;

    let logs = Arc::new(Mutex::new(Vec::new()));
    let log_sink = {
        let logs = Arc::clone(&logs);
        Arc::new(move |message: String| {
            logs.lock().expect("lock log sink").push(message);
        })
    };
    let clients = vec![
        SimulationClient::new("node-a", vec![true], None),
        SimulationClient::new("node-b", vec![true], None),
    ];
    let mut direct_links = HashSet::new();
    insert_links(
        &mut direct_links,
        &[("node-a", "node-b"), ("node-b", "node-a")],
    );

    let mut harness = SimulationHarness::new(
        base_url,
        clients,
        direct_links,
        true,
        relay_config.ttl.as_secs(),
        MessageEncryption::Plaintext,
        HashMap::new(),
        SimulationTimingConfig {
            base_seconds_per_step: 60.0,
            speed_factor: 1.0,
            base_real_time_per_step: Duration::ZERO,
        },
        HashMap::new(),
        Some(log_sink),
        None,
    );

    harness.run(1, Vec::new()).await;
    shutdown_tx.send(()).ok();

    let logs = logs.lock().expect("lock log sink");
    assert!(logs.iter().any(|entry| {
        entry.contains("[sim step=1]")
            && entry.contains("announced online")
            && (entry.contains("node-a") || entry.contains("node-b"))
    }));
}
