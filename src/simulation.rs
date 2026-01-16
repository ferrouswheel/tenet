use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use rand::distributions::Uniform;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};
use rand_distr::{Distribution, LogNormal, Normal};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::crypto::{generate_keypair_with_rng, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    build_plaintext_payload, decrypt_encrypted_payload, ContentId, Envelope, MessageKind,
    MetaMessage, Payload,
};
use crate::relay::{app, RelayConfig, RelayState};

const SIMULATION_PAYLOAD_AAD: &[u8] = b"tenet-simulation";
const SIMULATION_HPKE_INFO: &[u8] = b"tenet-simulation-hpke";
const SIMULATION_ACK_WINDOW_STEPS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub node_ids: Vec<String>,
    pub steps: usize,
    pub friends_per_node: FriendsPerNode,
    pub clustering: Option<ClusteringConfig>,
    pub post_frequency: PostFrequency,
    pub availability: OnlineAvailability,
    pub message_size_distribution: MessageSizeDistribution,
    pub encryption: Option<MessageEncryption>,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageEncryption {
    Plaintext,
    Encrypted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FriendsPerNode {
    Uniform { min: usize, max: usize },
    Poisson { lambda: f64 },
    Zipf { max: usize, exponent: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusteringConfig {
    pub cluster_sizes: Vec<usize>,
    pub inter_cluster_friend_probability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PostFrequency {
    Poisson {
        lambda_per_step: f64,
    },
    WeightedSchedule {
        weights: Vec<f64>,
        total_posts: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OnlineAvailability {
    Bernoulli {
        p_online: f64,
    },
    Markov {
        p_online_given_online: f64,
        p_online_given_offline: f64,
        start_online_prob: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageSizeDistribution {
    Uniform {
        min: usize,
        max: usize,
    },
    Normal {
        mean: f64,
        std_dev: f64,
        min: usize,
        max: usize,
    },
    LogNormal {
        mean: f64,
        std_dev: f64,
        min: usize,
        max: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfigToml {
    pub ttl_seconds: u64,
    pub max_messages: usize,
    pub max_bytes: usize,
    pub retry_backoff_seconds: Vec<u64>,
    pub peer_log_window_seconds: Option<u64>,
    pub peer_log_interval_seconds: Option<u64>,
}

impl RelayConfigToml {
    pub fn into_relay_config(self) -> RelayConfig {
        RelayConfig {
            ttl: Duration::from_secs(self.ttl_seconds),
            max_messages: self.max_messages,
            max_bytes: self.max_bytes,
            retry_backoff: self
                .retry_backoff_seconds
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            peer_log_window: Duration::from_secs(self.peer_log_window_seconds.unwrap_or(60)),
            peer_log_interval: Duration::from_secs(self.peer_log_interval_seconds.unwrap_or(30)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationScenarioConfig {
    pub simulation: SimulationConfig,
    pub relay: RelayConfigToml,
    pub direct_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulationMetrics {
    pub planned_messages: usize,
    pub sent_messages: usize,
    pub direct_deliveries: usize,
    pub inbox_deliveries: usize,
    pub store_forwards_stored: usize,
    pub store_forwards_forwarded: usize,
    pub store_forwards_delivered: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SenderRecipient {
    pub sender: String,
    pub recipient: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyHistogram {
    pub counts: HashMap<usize, usize>,
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub average: Option<f64>,
    pub samples: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulationMetricsReport {
    pub per_sender_recipient_counts: HashMap<SenderRecipient, usize>,
    pub first_delivery_latency: LatencyHistogram,
    pub all_recipients_delivery_latency: LatencyHistogram,
    pub online_broadcasts_sent: usize,
    pub acks_received: usize,
    pub missed_message_requests_sent: usize,
    pub missed_message_deliveries: usize,
    pub store_forwards_stored: usize,
    pub store_forwards_forwarded: usize,
    pub store_forwards_delivered: usize,
}

#[derive(Debug, Clone)]
pub struct SimulationReport {
    pub metrics: SimulationMetrics,
    pub report: SimulationMetricsReport,
}

#[derive(Debug, Clone)]
pub struct RollingLatencySnapshot {
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub average: Option<f64>,
    pub samples: usize,
    pub window: usize,
}

#[derive(Debug, Clone)]
pub struct SimulationStepUpdate {
    pub step: usize,
    pub total_steps: usize,
    pub online_nodes: usize,
    pub sent_messages: usize,
    pub received_messages: usize,
    pub rolling_latency: RollingLatencySnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimMessage {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub payload: Payload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreForPayload {
    envelope: Envelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedSend {
    pub step: usize,
    pub message: SimMessage,
}

#[derive(Debug, Clone)]
pub struct Node {
    id: String,
    schedule: Vec<bool>,
    inbox: Vec<SimMessage>,
    seen: HashSet<String>,
}

impl Node {
    pub fn new(id: &str, schedule: Vec<bool>) -> Self {
        Self {
            id: id.to_string(),
            schedule,
            inbox: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn online_at(&self, step: usize) -> bool {
        self.schedule.get(step).copied().unwrap_or(false)
    }

    fn receive(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.inbox.push(message);
            return true;
        }
        false
    }
}

#[derive(Debug, Clone)]
struct HistoricalMessage {
    send_step: usize,
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct StoredForPeerMessage {
    store_for_id: String,
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct PendingOnlineBroadcast {
    sender_id: String,
    recipient_id: String,
    sent_step: usize,
    expires_at: usize,
}

#[derive(Debug)]
pub struct SimulationInputs {
    pub nodes: Vec<Node>,
    pub planned_sends: Vec<PlannedSend>,
    pub direct_links: HashSet<(String, String)>,
    pub keypairs: HashMap<String, StoredKeypair>,
    pub encryption: MessageEncryption,
}

enum IncomingEnvelopeAction {
    DirectMessage(SimMessage),
    StoredForPeer(StoredForPeerMessage),
}

pub struct SimulationHarness {
    relay_base_url: String,
    nodes: HashMap<String, Node>,
    direct_links: HashSet<(String, String)>,
    neighbors: HashMap<String, Vec<String>>,
    direct_enabled: bool,
    ttl_seconds: u64,
    encryption: MessageEncryption,
    keypairs: HashMap<String, StoredKeypair>,
    last_seen_by_peer: HashMap<String, HashMap<String, u64>>,
    message_history: HashMap<(String, String), Vec<HistoricalMessage>>,
    message_send_steps: HashMap<String, usize>,
    pending_online_broadcasts: Vec<PendingOnlineBroadcast>,
    stored_forwards: HashMap<String, Vec<StoredForPeerMessage>>,
    pending_forwarded_messages: HashSet<String>,
    metrics: SimulationMetrics,
    metrics_tracker: MetricsTracker,
}

impl SimulationHarness {
    pub fn new(
        relay_base_url: String,
        nodes: Vec<Node>,
        direct_links: HashSet<(String, String)>,
        direct_enabled: bool,
        ttl_seconds: u64,
        encryption: MessageEncryption,
        keypairs: HashMap<String, StoredKeypair>,
    ) -> Self {
        let planned_messages = 0;
        let mut neighbors: HashMap<String, Vec<String>> = HashMap::new();
        for (sender, recipient) in &direct_links {
            neighbors
                .entry(sender.clone())
                .or_default()
                .push(recipient.clone());
        }
        let mut last_seen_by_peer = HashMap::new();
        for node in &nodes {
            last_seen_by_peer.insert(node.id.clone(), HashMap::new());
        }
        Self {
            relay_base_url,
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            direct_links,
            neighbors,
            direct_enabled,
            ttl_seconds,
            encryption,
            keypairs,
            last_seen_by_peer,
            message_history: HashMap::new(),
            message_send_steps: HashMap::new(),
            pending_online_broadcasts: Vec::new(),
            stored_forwards: HashMap::new(),
            pending_forwarded_messages: HashSet::new(),
            metrics: SimulationMetrics {
                planned_messages,
                sent_messages: 0,
                direct_deliveries: 0,
                inbox_deliveries: 0,
                store_forwards_stored: 0,
                store_forwards_forwarded: 0,
                store_forwards_delivered: 0,
            },
            metrics_tracker: MetricsTracker::default(),
        }
    }

    pub fn metrics(&self) -> SimulationMetrics {
        self.metrics.clone()
    }

    pub fn metrics_report(&self) -> SimulationMetricsReport {
        self.metrics_tracker.report()
    }

    fn record_message_send(&mut self, message: &SimMessage, step: usize, envelope: &Envelope) {
        self.message_send_steps.insert(message.id.clone(), step);
        self.message_history
            .entry((message.sender.clone(), message.recipient.clone()))
            .or_default()
            .push(HistoricalMessage {
                send_step: step,
                envelope: envelope.clone(),
            });
    }

    fn update_last_seen(&mut self, recipient_id: &str, sender_id: &str, message_id: &str) {
        let Some(send_step) = self.message_send_steps.get(message_id) else {
            return;
        };
        let entry = self
            .last_seen_by_peer
            .entry(recipient_id.to_string())
            .or_default()
            .entry(sender_id.to_string())
            .or_insert(0);
        *entry = (*entry).max(*send_step as u64);
    }

    fn last_seen_for(&self, recipient_id: &str, sender_id: &str) -> u64 {
        self.last_seen_by_peer
            .get(recipient_id)
            .and_then(|map| map.get(sender_id))
            .copied()
            .unwrap_or(0)
    }

    fn enqueue_online_broadcasts(&mut self, node_id: &str, step: usize) {
        let Some(neighbors) = self.neighbors.get(node_id) else {
            return;
        };
        for neighbor in neighbors {
            let meta = MetaMessage::Online {
                peer_id: node_id.to_string(),
                timestamp: step as u64,
            };
            let _ = build_meta_payload(&meta);
            self.pending_online_broadcasts.push(PendingOnlineBroadcast {
                sender_id: node_id.to_string(),
                recipient_id: neighbor.clone(),
                sent_step: step,
                expires_at: step.saturating_add(SIMULATION_ACK_WINDOW_STEPS),
            });
            self.metrics_tracker.record_online_broadcast();
        }
    }

    fn process_pending_online_broadcasts(
        &mut self,
        step: usize,
        online_nodes: &HashSet<String>,
    ) -> usize {
        let pending = std::mem::take(&mut self.pending_online_broadcasts);
        let mut remaining = Vec::with_capacity(pending.len());
        let mut delivered: usize = 0;
        for pending in pending {
            if step > pending.expires_at {
                continue;
            }
            if online_nodes.contains(&pending.recipient_id) {
                let ack = MetaMessage::Ack {
                    peer_id: pending.recipient_id.clone(),
                    online_timestamp: pending.sent_step as u64,
                };
                let _ = build_meta_payload(&ack);
                self.metrics_tracker.record_ack();
                delivered = delivered.saturating_add(self.handle_message_request(
                    &pending.sender_id,
                    &pending.recipient_id,
                    step,
                ));
                continue;
            }
            remaining.push(pending);
        }
        self.pending_online_broadcasts = remaining;
        delivered
    }

    fn handle_message_request(&mut self, requester: &str, responder: &str, step: usize) -> usize {
        let last_seen = self.last_seen_for(requester, responder);
        let request = MetaMessage::MessageRequest {
            peer_id: responder.to_string(),
            since_timestamp: last_seen,
        };
        let _ = build_meta_payload(&request);
        self.metrics_tracker.record_missed_message_request();
        let Some(history) = self
            .message_history
            .get(&(responder.to_string(), requester.to_string()))
        else {
            return 0;
        };
        let entries: Vec<HistoricalMessage> = history
            .iter()
            .filter(|entry| entry.send_step as u64 >= last_seen)
            .cloned()
            .collect();
        let mut delivered = 0;
        for entry in entries {
            if self.deliver_missed_message(&entry.envelope, requester, step) {
                delivered += 1;
            }
        }
        delivered
    }

    fn deliver_missed_message(
        &mut self,
        envelope: &Envelope,
        recipient_id: &str,
        step: usize,
    ) -> bool {
        let message_id = envelope.header.message_id.0.clone();
        let message = self.decode_direct_envelope(envelope, &message_id, recipient_id);
        let Some(message) = message else {
            return false;
        };
        let Some(node) = self.nodes.get_mut(recipient_id) else {
            return false;
        };
        if node.receive(message.clone()) {
            self.metrics_tracker.record_missed_message_delivery();
            self.record_delivery_metrics(&message, recipient_id, step);
            return true;
        }
        false
    }

    fn record_delivery_metrics(
        &mut self,
        message: &SimMessage,
        recipient_id: &str,
        step: usize,
    ) -> Option<usize> {
        if self.pending_forwarded_messages.remove(&message.id) {
            self.metrics.store_forwards_delivered =
                self.metrics.store_forwards_delivered.saturating_add(1);
            self.metrics_tracker.record_store_forward_delivery();
        }
        let latency = self
            .metrics_tracker
            .record_delivery(&message.id, recipient_id, step);
        self.update_last_seen(recipient_id, &message.sender, &message.id);
        latency
    }

    fn store_forward_message(&mut self, storage_peer_id: &str, message: StoredForPeerMessage) {
        self.stored_forwards
            .entry(storage_peer_id.to_string())
            .or_default()
            .push(message);
        self.metrics.store_forwards_stored = self.metrics.store_forwards_stored.saturating_add(1);
        self.metrics_tracker.record_store_forward_stored();
    }

    fn select_storage_peer(&self, sender: &str, recipient: &str) -> Option<String> {
        let sender_neighbors = self.neighbors.get(sender)?;
        let recipient_neighbors = self.neighbors.get(recipient)?;
        let sender_set: HashSet<&String> = sender_neighbors.iter().collect();
        let mut mutual: Vec<String> = recipient_neighbors
            .iter()
            .filter(|candidate| sender_set.contains(candidate))
            .cloned()
            .collect();
        mutual.sort();
        mutual
            .into_iter()
            .find(|candidate| candidate != sender && candidate != recipient)
    }

    async fn forward_store_forwards(&mut self, online_set: &HashSet<String>) -> usize {
        let mut forwarded = 0usize;
        let storage_peers: Vec<String> = self.stored_forwards.keys().cloned().collect();
        for storage_peer_id in storage_peers {
            if !online_set.contains(&storage_peer_id) {
                continue;
            }
            if let Some(stored) = self.stored_forwards.get_mut(&storage_peer_id) {
                let mut remaining = Vec::with_capacity(stored.len());
                for entry in stored.drain(..) {
                    if online_set.contains(&entry.store_for_id) {
                        let message_id = entry.envelope.header.message_id.0.clone();
                        let _ = post_envelope(&self.relay_base_url, &entry.envelope).await;
                        self.pending_forwarded_messages.insert(message_id);
                        self.metrics.store_forwards_forwarded =
                            self.metrics.store_forwards_forwarded.saturating_add(1);
                        self.metrics_tracker.record_store_forward_forwarded();
                        forwarded = forwarded.saturating_add(1);
                    } else {
                        remaining.push(entry);
                    }
                }
                *stored = remaining;
            }
        }
        forwarded
    }

    pub async fn run(&mut self, steps: usize, plan: Vec<PlannedSend>) -> SimulationMetrics {
        self.metrics.planned_messages = plan.len();
        let mut sends_by_step: HashMap<usize, Vec<SimMessage>> = HashMap::new();
        for item in plan {
            sends_by_step
                .entry(item.step)
                .or_default()
                .push(item.message);
        }

        let mut previous_online: HashMap<String, bool> =
            self.nodes.keys().map(|id| (id.clone(), false)).collect();

        for step in 0..steps {
            if let Some(messages) = sends_by_step.get(&step) {
                for message in messages {
                    if self
                        .nodes
                        .get(&message.sender)
                        .is_some_and(|node| node.online_at(step))
                    {
                        self.metrics.sent_messages += 1;
                        self.metrics_tracker.record_send(message, step);
                        let delivered_direct = self.route_message(step, message).await;
                        if delivered_direct.is_some() {
                            self.metrics.direct_deliveries += 1;
                        }
                    }
                }
            }

            let online_ids: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.online_at(step))
                .map(|(id, _)| id.clone())
                .collect();
            let online_set: HashSet<String> = online_ids.iter().cloned().collect();
            let newly_online: Vec<String> = online_ids
                .iter()
                .filter(|id| !previous_online.get(*id).copied().unwrap_or(false))
                .cloned()
                .collect();

            let _ = self.forward_store_forwards(&online_set).await;

            for node_id in &newly_online {
                let envelopes = fetch_inbox(&self.relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    match self.decode_envelope_action(&envelope, node_id) {
                        Some(IncomingEnvelopeAction::DirectMessage(message)) => {
                            if let Some(node) = self.nodes.get_mut(node_id) {
                                if node.receive(message.clone()) {
                                    self.metrics.inbox_deliveries += 1;
                                    self.record_delivery_metrics(&message, node_id, step);
                                }
                            }
                        }
                        Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                            self.store_forward_message(node_id, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &newly_online {
                self.enqueue_online_broadcasts(node_id, step);
            }

            let _ = self.process_pending_online_broadcasts(step, &online_set);

            for node_id in &online_ids {
                if newly_online.contains(node_id) {
                    continue;
                }
                let envelopes = fetch_inbox(&self.relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    match self.decode_envelope_action(&envelope, node_id) {
                        Some(IncomingEnvelopeAction::DirectMessage(message)) => {
                            if let Some(node) = self.nodes.get_mut(node_id) {
                                if node.receive(message.clone()) {
                                    self.metrics.inbox_deliveries += 1;
                                    self.record_delivery_metrics(&message, node_id, step);
                                }
                            }
                        }
                        Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                            self.store_forward_message(node_id, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &online_ids {
                previous_online.insert(node_id.clone(), true);
            }
            for (node_id, was_online) in previous_online.iter_mut() {
                if !online_set.contains(node_id) {
                    *was_online = false;
                }
            }
        }

        self.metrics()
    }

    pub async fn run_with_progress<F>(
        &mut self,
        steps: usize,
        plan: Vec<PlannedSend>,
        mut on_step: F,
    ) -> SimulationMetrics
    where
        F: FnMut(SimulationStepUpdate),
    {
        self.metrics.planned_messages = plan.len();
        let mut sends_by_step: HashMap<usize, Vec<SimMessage>> = HashMap::new();
        for item in plan {
            sends_by_step
                .entry(item.step)
                .or_default()
                .push(item.message);
        }

        let mut rolling_latency = RollingLatencyTracker::new(100);
        let mut previous_online: HashMap<String, bool> =
            self.nodes.keys().map(|id| (id.clone(), false)).collect();

        for step in 0..steps {
            let mut sent_messages = 0usize;
            let mut received_messages = 0usize;
            if let Some(messages) = sends_by_step.get(&step) {
                for message in messages {
                    if self
                        .nodes
                        .get(&message.sender)
                        .is_some_and(|node| node.online_at(step))
                    {
                        sent_messages += 1;
                        self.metrics.sent_messages += 1;
                        self.metrics_tracker.record_send(message, step);
                        if let Some(latency) = self.route_message(step, message).await {
                            self.metrics.direct_deliveries += 1;
                            received_messages += 1;
                            rolling_latency.record(latency);
                        }
                    }
                }
            }

            let online_ids: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.online_at(step))
                .map(|(id, _)| id.clone())
                .collect();
            let online_set: HashSet<String> = online_ids.iter().cloned().collect();
            let newly_online: Vec<String> = online_ids
                .iter()
                .filter(|id| !previous_online.get(*id).copied().unwrap_or(false))
                .cloned()
                .collect();

            let _ = self.forward_store_forwards(&online_set).await;

            for node_id in &newly_online {
                let envelopes = fetch_inbox(&self.relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    match self.decode_envelope_action(&envelope, node_id) {
                        Some(IncomingEnvelopeAction::DirectMessage(message)) => {
                            if let Some(node) = self.nodes.get_mut(node_id) {
                                if node.receive(message.clone()) {
                                    self.metrics.inbox_deliveries += 1;
                                    if let Some(latency) =
                                        self.record_delivery_metrics(&message, node_id, step)
                                    {
                                        rolling_latency.record(latency);
                                    }
                                    received_messages += 1;
                                }
                            }
                        }
                        Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                            self.store_forward_message(node_id, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &newly_online {
                self.enqueue_online_broadcasts(node_id, step);
            }

            let missed_deliveries = self.process_pending_online_broadcasts(step, &online_set);
            received_messages = received_messages.saturating_add(missed_deliveries);

            for node_id in &online_ids {
                if newly_online.contains(node_id) {
                    continue;
                }
                let envelopes = fetch_inbox(&self.relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    match self.decode_envelope_action(&envelope, node_id) {
                        Some(IncomingEnvelopeAction::DirectMessage(message)) => {
                            if let Some(node) = self.nodes.get_mut(node_id) {
                                if node.receive(message.clone()) {
                                    self.metrics.inbox_deliveries += 1;
                                    if let Some(latency) =
                                        self.record_delivery_metrics(&message, node_id, step)
                                    {
                                        rolling_latency.record(latency);
                                    }
                                    received_messages += 1;
                                }
                            }
                        }
                        Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                            self.store_forward_message(node_id, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &online_ids {
                previous_online.insert(node_id.clone(), true);
            }
            for (node_id, was_online) in previous_online.iter_mut() {
                if !online_set.contains(node_id) {
                    *was_online = false;
                }
            }

            let update = SimulationStepUpdate {
                step: step + 1,
                total_steps: steps,
                online_nodes: online_ids.len(),
                sent_messages,
                received_messages,
                rolling_latency: rolling_latency.snapshot(),
            };
            on_step(update);
        }

        self.metrics()
    }

    pub fn inbox(&self, node_id: &str) -> Vec<SimMessage> {
        self.nodes
            .get(node_id)
            .map(|node| node.inbox.clone())
            .unwrap_or_default()
    }

    async fn route_message(&mut self, step: usize, message: &SimMessage) -> Option<usize> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let envelope = build_envelope_from_payload(
            message.sender.clone(),
            message.recipient.clone(),
            None,
            None,
            timestamp,
            self.ttl_seconds,
            MessageKind::Direct,
            None,
            message.payload.clone(),
        )
        .ok()?;
        self.record_message_send(message, step, &envelope);
        let mut direct_latency = None;
        let recipient_online = self
            .nodes
            .get(&message.recipient)
            .is_some_and(|node| node.online_at(step));
        if self.direct_enabled
            && self
                .direct_links
                .contains(&(message.sender.clone(), message.recipient.clone()))
        {
            if recipient_online {
                let decoded =
                    self.decode_direct_envelope(&envelope, &message.id, &message.recipient);
                if let Some(decoded) = decoded {
                    if let Some(node) = self.nodes.get_mut(&message.recipient) {
                        if node.receive(decoded.clone()) {
                            direct_latency =
                                self.record_delivery_metrics(&decoded, &message.recipient, step);
                        }
                    }
                }
            }
        }

        if !recipient_online {
            if let Some(storage_peer_id) =
                self.select_storage_peer(&message.sender, &message.recipient)
            {
                if let Some(store_envelope) = self.build_store_for_envelope(
                    &message.sender,
                    &storage_peer_id,
                    &message.recipient,
                    timestamp,
                    &envelope,
                ) {
                    let _ = post_envelope(&self.relay_base_url, &store_envelope).await;
                    return direct_latency;
                }
            }
        }

        let _ = post_envelope(&self.relay_base_url, &envelope).await;

        direct_latency
    }

    fn decode_envelope_action(
        &self,
        envelope: &Envelope,
        recipient_id: &str,
    ) -> Option<IncomingEnvelopeAction> {
        if envelope.header.verify_signature(envelope.version).is_err() {
            return None;
        }
        match envelope.header.message_kind {
            MessageKind::Direct => self
                .decode_direct_envelope(envelope, &envelope.header.message_id.0, recipient_id)
                .map(IncomingEnvelopeAction::DirectMessage),
            MessageKind::StoreForPeer => self
                .decode_store_for_envelope(envelope, recipient_id)
                .map(IncomingEnvelopeAction::StoredForPeer),
            _ => None,
        }
    }

    fn decode_direct_envelope(
        &self,
        envelope: &Envelope,
        message_id: &str,
        recipient_id: &str,
    ) -> Option<SimMessage> {
        if envelope.header.message_kind != MessageKind::Direct {
            return None;
        }
        let body = match self.encryption {
            MessageEncryption::Plaintext => envelope.payload.body.clone(),
            MessageEncryption::Encrypted => {
                let keypair = self.keypairs.get(recipient_id)?;
                let plaintext = decrypt_encrypted_payload(
                    &envelope.payload,
                    &keypair.private_key_hex,
                    SIMULATION_PAYLOAD_AAD,
                    SIMULATION_HPKE_INFO,
                )
                .ok()?;
                String::from_utf8(plaintext).ok()?
            }
        };
        Some(SimMessage {
            id: message_id.to_string(),
            sender: envelope.header.sender_id.clone(),
            recipient: envelope.header.recipient_id.clone(),
            body,
            payload: envelope.payload.clone(),
        })
    }

    fn decode_store_for_envelope(
        &self,
        envelope: &Envelope,
        storage_peer_id: &str,
    ) -> Option<StoredForPeerMessage> {
        if envelope.header.message_kind != MessageKind::StoreForPeer {
            return None;
        }
        let store_for_id = envelope.header.store_for.clone()?;
        let storage_peer = envelope.header.storage_peer_id.clone()?;
        if storage_peer != storage_peer_id {
            return None;
        }
        let keypair = self.keypairs.get(storage_peer_id)?;
        let plaintext = decrypt_encrypted_payload(
            &envelope.payload,
            &keypair.private_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
        )
        .ok()?;
        let payload: StoreForPayload = serde_json::from_slice(&plaintext).ok()?;
        Some(StoredForPeerMessage {
            store_for_id,
            envelope: payload.envelope,
        })
    }

    fn build_store_for_envelope(
        &self,
        sender_id: &str,
        storage_peer_id: &str,
        store_for_id: &str,
        timestamp: u64,
        inner_envelope: &Envelope,
    ) -> Option<Envelope> {
        let keypair = self.keypairs.get(storage_peer_id)?;
        let payload_bytes = serde_json::to_vec(&StoreForPayload {
            envelope: inner_envelope.clone(),
        })
        .ok()?;
        let mut rng = rand::thread_rng();
        let mut content_key = [0u8; CONTENT_KEY_SIZE];
        let mut nonce = [0u8; NONCE_SIZE];
        rng.fill_bytes(&mut content_key);
        rng.fill_bytes(&mut nonce);
        let outer_payload = build_encrypted_payload(
            &payload_bytes,
            &keypair.public_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        )
        .ok()?;
        build_envelope_from_payload(
            sender_id.to_string(),
            storage_peer_id.to_string(),
            Some(store_for_id.to_string()),
            Some(storage_peer_id.to_string()),
            timestamp,
            self.ttl_seconds,
            MessageKind::StoreForPeer,
            None,
            outer_payload,
        )
        .ok()
    }
}

#[derive(Debug, Clone)]
struct MessageTracking {
    send_step: usize,
    sender: String,
    recipients: Vec<String>,
    delivered_recipients: HashSet<String>,
    first_delivery_recorded: bool,
    all_delivery_recorded: bool,
}

#[derive(Debug, Default)]
struct LatencyStats {
    counts: HashMap<usize, usize>,
    total: usize,
    samples: usize,
    min: Option<usize>,
    max: Option<usize>,
}

impl LatencyStats {
    fn record(&mut self, latency: usize) {
        *self.counts.entry(latency).or_insert(0) += 1;
        self.total = self.total.saturating_add(latency);
        self.samples = self.samples.saturating_add(1);
        self.min = Some(self.min.map_or(latency, |min| min.min(latency)));
        self.max = Some(self.max.map_or(latency, |max| max.max(latency)));
    }

    fn report(&self) -> LatencyHistogram {
        let average = if self.samples > 0 {
            Some(self.total as f64 / self.samples as f64)
        } else {
            None
        };
        LatencyHistogram {
            counts: self.counts.clone(),
            min: self.min,
            max: self.max,
            average,
            samples: self.samples,
        }
    }
}

#[derive(Debug, Default)]
struct MetricsTracker {
    per_sender_recipient_counts: HashMap<SenderRecipient, usize>,
    first_delivery_latency: LatencyStats,
    all_recipients_delivery_latency: LatencyStats,
    messages: HashMap<String, MessageTracking>,
    online_broadcasts_sent: usize,
    acks_received: usize,
    missed_message_requests_sent: usize,
    missed_message_deliveries: usize,
    store_forwards_stored: usize,
    store_forwards_forwarded: usize,
    store_forwards_delivered: usize,
}

impl MetricsTracker {
    fn record_send(&mut self, message: &SimMessage, step: usize) {
        let key = SenderRecipient {
            sender: message.sender.clone(),
            recipient: message.recipient.clone(),
        };
        *self.per_sender_recipient_counts.entry(key).or_insert(0) += 1;
        self.messages
            .entry(message.id.clone())
            .or_insert_with(|| MessageTracking {
                send_step: step,
                sender: message.sender.clone(),
                recipients: vec![message.recipient.clone()],
                delivered_recipients: HashSet::new(),
                first_delivery_recorded: false,
                all_delivery_recorded: false,
            });
    }

    fn record_delivery(
        &mut self,
        message_id: &str,
        recipient_id: &str,
        step: usize,
    ) -> Option<usize> {
        let Some(tracking) = self.messages.get_mut(message_id) else {
            return None;
        };
        if !tracking
            .recipients
            .iter()
            .any(|recipient| recipient == recipient_id)
        {
            return None;
        }
        if !tracking
            .delivered_recipients
            .insert(recipient_id.to_string())
        {
            return None;
        }
        let latency = step.saturating_sub(tracking.send_step);
        if !tracking.first_delivery_recorded {
            self.first_delivery_latency.record(latency);
            tracking.first_delivery_recorded = true;
        }
        if tracking.delivered_recipients.len() == tracking.recipients.len()
            && !tracking.all_delivery_recorded
        {
            self.all_recipients_delivery_latency.record(latency);
            tracking.all_delivery_recorded = true;
        }
        Some(latency)
    }

    fn record_online_broadcast(&mut self) {
        self.online_broadcasts_sent = self.online_broadcasts_sent.saturating_add(1);
    }

    fn record_ack(&mut self) {
        self.acks_received = self.acks_received.saturating_add(1);
    }

    fn record_missed_message_request(&mut self) {
        self.missed_message_requests_sent = self.missed_message_requests_sent.saturating_add(1);
    }

    fn record_missed_message_delivery(&mut self) {
        self.missed_message_deliveries = self.missed_message_deliveries.saturating_add(1);
    }

    fn record_store_forward_stored(&mut self) {
        self.store_forwards_stored = self.store_forwards_stored.saturating_add(1);
    }

    fn record_store_forward_forwarded(&mut self) {
        self.store_forwards_forwarded = self.store_forwards_forwarded.saturating_add(1);
    }

    fn record_store_forward_delivery(&mut self) {
        self.store_forwards_delivered = self.store_forwards_delivered.saturating_add(1);
    }

    fn report(&self) -> SimulationMetricsReport {
        SimulationMetricsReport {
            per_sender_recipient_counts: self.per_sender_recipient_counts.clone(),
            first_delivery_latency: self.first_delivery_latency.report(),
            all_recipients_delivery_latency: self.all_recipients_delivery_latency.report(),
            online_broadcasts_sent: self.online_broadcasts_sent,
            acks_received: self.acks_received,
            missed_message_requests_sent: self.missed_message_requests_sent,
            missed_message_deliveries: self.missed_message_deliveries,
            store_forwards_stored: self.store_forwards_stored,
            store_forwards_forwarded: self.store_forwards_forwarded,
            store_forwards_delivered: self.store_forwards_delivered,
        }
    }
}

pub async fn run_simulation_scenario(
    scenario: SimulationScenarioConfig,
) -> Result<SimulationReport, String> {
    let relay_config = scenario.relay.clone();
    let (base_url, shutdown_tx) = start_relay(relay_config.clone().into_relay_config()).await;
    let inputs = build_simulation_inputs(&scenario.simulation);
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.nodes,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
    );
    let metrics = harness
        .run(scenario.simulation.steps, inputs.planned_sends)
        .await;
    let report = harness.metrics_report();
    shutdown_tx.send(()).ok();
    Ok(SimulationReport { metrics, report })
}

pub async fn run_simulation_scenario_with_progress<F>(
    scenario: SimulationScenarioConfig,
    mut on_step: F,
) -> Result<SimulationReport, String>
where
    F: FnMut(SimulationStepUpdate),
{
    let relay_config = scenario.relay.clone();
    let (base_url, shutdown_tx) = start_relay(relay_config.clone().into_relay_config()).await;
    let inputs = build_simulation_inputs(&scenario.simulation);
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.nodes,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
    );
    let metrics = harness
        .run_with_progress(scenario.simulation.steps, inputs.planned_sends, |update| {
            on_step(update);
        })
        .await;
    let report = harness.metrics_report();
    shutdown_tx.send(()).ok();
    Ok(SimulationReport { metrics, report })
}

#[derive(Debug)]
struct RollingLatencyTracker {
    window: usize,
    samples: VecDeque<usize>,
}

impl RollingLatencyTracker {
    fn new(window: usize) -> Self {
        Self {
            window,
            samples: VecDeque::with_capacity(window),
        }
    }

    fn record(&mut self, latency: usize) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(latency);
    }

    fn snapshot(&self) -> RollingLatencySnapshot {
        let samples = self.samples.len();
        let (min, max, average) = if samples == 0 {
            (None, None, None)
        } else {
            let mut min = usize::MAX;
            let mut max = 0usize;
            let mut total = 0usize;
            for latency in &self.samples {
                min = min.min(*latency);
                max = max.max(*latency);
                total = total.saturating_add(*latency);
            }
            (Some(min), Some(max), Some(total as f64 / samples as f64))
        };
        RollingLatencySnapshot {
            min,
            max,
            average,
            samples,
            window: self.window,
        }
    }
}

pub fn build_simulation_inputs(config: &SimulationConfig) -> SimulationInputs {
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let encryption = config
        .encryption
        .clone()
        .unwrap_or(MessageEncryption::Plaintext);
    let mut crypto_rng = ChaCha20Rng::seed_from_u64(config.seed ^ 0x5A17_5EED);
    let graph = build_friend_graph(config, &mut rng);
    let schedules = build_online_schedules(config, &mut rng);
    let mut keypairs = HashMap::new();
    for node_id in &config.node_ids {
        keypairs.insert(node_id.clone(), generate_keypair_with_rng(&mut crypto_rng));
    }
    let planned_sends = build_planned_sends(
        config,
        &mut rng,
        &graph,
        &encryption,
        &keypairs,
        if matches!(encryption, MessageEncryption::Encrypted) {
            Some(&mut crypto_rng as &mut dyn RngCore)
        } else {
            None
        },
    );
    let mut direct_links = HashSet::new();
    let mut nodes = Vec::new();
    for node_id in &config.node_ids {
        if let Some(friends) = graph.get(node_id) {
            for friend in friends {
                direct_links.insert((node_id.clone(), friend.clone()));
            }
        }
        let schedule = schedules.get(node_id).cloned().unwrap_or_default();
        nodes.push(Node::new(node_id, schedule));
    }
    SimulationInputs {
        nodes,
        planned_sends,
        direct_links,
        keypairs,
        encryption,
    }
}

pub fn build_friend_graph(
    config: &SimulationConfig,
    rng: &mut impl Rng,
) -> HashMap<String, Vec<String>> {
    if let Some(clustering) = &config.clustering {
        return build_clustered_friend_graph(config, clustering, rng);
    }
    let mut graph = HashMap::new();
    let uniform = Uniform::new_inclusive(0usize, config.node_ids.len().saturating_sub(1));
    for node_id in &config.node_ids {
        let friends_target =
            sample_friends_target(config, rng, config.node_ids.len().saturating_sub(1));

        let mut friends = HashSet::new();
        while friends.len() < friends_target
            && friends.len() < config.node_ids.len().saturating_sub(1)
        {
            let idx = rng.sample(uniform);
            let candidate = &config.node_ids[idx];
            if candidate != node_id {
                friends.insert(candidate.clone());
            }
        }
        graph.insert(node_id.clone(), friends.into_iter().collect());
    }
    graph
}

fn build_clustered_friend_graph(
    config: &SimulationConfig,
    clustering: &ClusteringConfig,
    rng: &mut impl Rng,
) -> HashMap<String, Vec<String>> {
    let mut clusters = Vec::new();
    let mut start = 0usize;
    let total = config.node_ids.len();
    for size in &clustering.cluster_sizes {
        if start >= total {
            break;
        }
        let end = (start + *size).min(total);
        clusters.push(config.node_ids[start..end].to_vec());
        start = end;
    }
    if start < total {
        clusters.push(config.node_ids[start..].to_vec());
    }
    if clusters.is_empty() {
        clusters.push(config.node_ids.clone());
    }

    let mut graph: HashMap<String, HashSet<String>> = HashMap::new();
    for cluster in &clusters {
        for node_id in cluster {
            let max_friends = cluster.len().saturating_sub(1);
            let friends_target = sample_friends_target(config, rng, max_friends);
            let mut friends = HashSet::new();
            while friends.len() < friends_target && friends.len() < max_friends {
                let idx = rng.gen_range(0..cluster.len());
                let candidate = &cluster[idx];
                if candidate != node_id {
                    friends.insert(candidate.clone());
                }
            }
            graph.insert(node_id.clone(), friends);
        }
    }

    let inter_prob = clustering
        .inter_cluster_friend_probability
        .max(0.0)
        .min(1.0);
    if inter_prob > 0.0 {
        let mut cluster_lookup = HashMap::new();
        for (index, cluster) in clusters.iter().enumerate() {
            for node_id in cluster {
                cluster_lookup.insert(node_id, index);
            }
        }
        for node_id in &config.node_ids {
            let node_cluster = cluster_lookup.get(node_id);
            for candidate in &config.node_ids {
                if candidate == node_id {
                    continue;
                }
                let candidate_cluster = cluster_lookup.get(candidate);
                if node_cluster == candidate_cluster {
                    continue;
                }
                if rng.gen::<f64>() < inter_prob {
                    graph
                        .entry(node_id.clone())
                        .or_insert_with(HashSet::new)
                        .insert(candidate.clone());
                }
            }
        }
    }

    graph
        .into_iter()
        .map(|(node_id, friends)| (node_id, friends.into_iter().collect()))
        .collect()
}

fn sample_friends_target(
    config: &SimulationConfig,
    rng: &mut impl Rng,
    max_friends: usize,
) -> usize {
    match &config.friends_per_node {
        FriendsPerNode::Uniform { min, max } => rng.gen_range(*min..=(*max).min(max_friends)),
        FriendsPerNode::Poisson { lambda } => sample_poisson(rng, *lambda).min(max_friends),
        FriendsPerNode::Zipf { max, exponent } => {
            sample_zipf(rng, (*max).min(max_friends), *exponent)
        }
    }
}

pub fn build_online_schedules(
    config: &SimulationConfig,
    rng: &mut impl Rng,
) -> HashMap<String, Vec<bool>> {
    let mut schedules = HashMap::new();
    for node_id in &config.node_ids {
        let schedule = match &config.availability {
            OnlineAvailability::Bernoulli { p_online } => (0..config.steps)
                .map(|_| rng.gen::<f64>() < *p_online)
                .collect(),
            OnlineAvailability::Markov {
                p_online_given_online,
                p_online_given_offline,
                start_online_prob,
            } => {
                let mut schedule = Vec::with_capacity(config.steps);
                let mut online = rng.gen::<f64>() < *start_online_prob;
                for _ in 0..config.steps {
                    schedule.push(online);
                    let next_prob = if online {
                        *p_online_given_online
                    } else {
                        *p_online_given_offline
                    };
                    online = rng.gen::<f64>() < next_prob;
                }
                schedule
            }
        };
        schedules.insert(node_id.clone(), schedule);
    }
    schedules
}

pub fn build_planned_sends(
    config: &SimulationConfig,
    rng: &mut impl Rng,
    graph: &HashMap<String, Vec<String>>,
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    mut crypto_rng: Option<&mut dyn RngCore>,
) -> Vec<PlannedSend> {
    let mut planned = Vec::new();
    let mut counter = 0usize;
    for node_id in &config.node_ids {
        let friends = graph
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        if friends.is_empty() {
            continue;
        }
        match &config.post_frequency {
            PostFrequency::Poisson { lambda_per_step } => {
                for step in 0..config.steps {
                    let count = sample_poisson(rng, *lambda_per_step);
                    for _ in 0..count {
                        let recipient = friends[rng.gen_range(0..friends.len())].clone();
                        let body = generate_message_body(rng, &config.message_size_distribution);
                        let payload = build_message_payload(
                            encryption,
                            keypairs,
                            &mut crypto_rng,
                            &body,
                            &recipient,
                            node_id,
                            counter,
                        );
                        let message_id =
                            ContentId::from_value(&payload).expect("serialize simulation payload");
                        let message = SimMessage {
                            id: message_id.0.clone(),
                            sender: node_id.clone(),
                            recipient,
                            body,
                            payload,
                        };
                        counter += 1;
                        planned.push(PlannedSend { step, message });
                    }
                }
            }
            PostFrequency::WeightedSchedule {
                weights,
                total_posts,
            } => {
                let weights = if weights.len() == config.steps {
                    weights.clone()
                } else {
                    vec![1.0; config.steps]
                };
                for _ in 0..*total_posts {
                    let step = sample_weighted_index(rng, &weights);
                    let recipient = friends[rng.gen_range(0..friends.len())].clone();
                    let body = generate_message_body(rng, &config.message_size_distribution);
                    let payload = build_message_payload(
                        encryption,
                        keypairs,
                        &mut crypto_rng,
                        &body,
                        &recipient,
                        node_id,
                        counter,
                    );
                    let message_id =
                        ContentId::from_value(&payload).expect("serialize simulation payload");
                    let message = SimMessage {
                        id: message_id.0.clone(),
                        sender: node_id.clone(),
                        recipient,
                        body,
                        payload,
                    };
                    counter += 1;
                    planned.push(PlannedSend { step, message });
                }
            }
        }
    }
    planned
}

fn build_message_payload(
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    crypto_rng: &mut Option<&mut dyn RngCore>,
    body: &str,
    recipient: &str,
    sender: &str,
    counter: usize,
) -> Payload {
    match encryption {
        MessageEncryption::Plaintext => {
            let salt = format!("{sender}-{counter}");
            build_plaintext_payload(body.to_string(), salt.as_bytes())
        }
        MessageEncryption::Encrypted => {
            let keypair = keypairs.get(recipient).expect("missing recipient keypair");
            let rng = crypto_rng.as_deref_mut().expect("missing crypto rng");
            let mut content_key = [0u8; CONTENT_KEY_SIZE];
            let mut nonce = [0u8; NONCE_SIZE];
            let mut sender_seed = [0u8; 32];
            rng.fill_bytes(&mut content_key);
            rng.fill_bytes(&mut nonce);
            rng.fill_bytes(&mut sender_seed);
            build_encrypted_payload(
                body.as_bytes(),
                &keypair.public_key_hex,
                SIMULATION_PAYLOAD_AAD,
                SIMULATION_HPKE_INFO,
                &content_key,
                &nonce,
                Some(sender_seed),
            )
            .expect("encrypt payload")
        }
    }
}

pub fn sample_poisson(rng: &mut impl Rng, lambda: f64) -> usize {
    if lambda <= 0.0 {
        return 0;
    }
    let l = (-lambda).exp();
    let mut k = 0usize;
    let mut p = 1.0;
    while p > l {
        k += 1;
        p *= rng.gen::<f64>();
    }
    k.saturating_sub(1)
}

pub fn sample_zipf(rng: &mut impl Rng, max: usize, exponent: f64) -> usize {
    let max = max.max(1);
    let exponent = exponent.max(0.0);
    let mut weights = Vec::with_capacity(max);
    for k in 1..=max {
        let weight = 1.0 / (k as f64).powf(exponent);
        weights.push(weight);
    }
    let index = sample_weighted_index(rng, &weights);
    index + 1
}

pub fn sample_weighted_index(rng: &mut impl Rng, weights: &[f64]) -> usize {
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return rng.gen_range(0..weights.len().max(1));
    }
    let mut target = rng.gen_range(0.0..total);
    for (idx, weight) in weights.iter().enumerate() {
        if *weight <= 0.0 {
            continue;
        }
        if target < *weight {
            return idx;
        }
        target -= *weight;
    }
    weights.len().saturating_sub(1)
}

pub fn generate_message_body(rng: &mut impl Rng, distribution: &MessageSizeDistribution) -> String {
    const LOREM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
    let target_size = sample_message_size(rng, distribution);
    if target_size == 0 {
        return String::new();
    }
    let mut body = String::with_capacity(target_size);
    while body.len() < target_size {
        body.push_str(LOREM);
    }
    body.truncate(target_size);
    body
}

pub fn sample_message_size(rng: &mut impl Rng, distribution: &MessageSizeDistribution) -> usize {
    match distribution {
        MessageSizeDistribution::Uniform { min, max } => {
            let (min, max) = normalize_bounds(*min, *max);
            rng.gen_range(min..=max)
        }
        MessageSizeDistribution::Normal {
            mean,
            std_dev,
            min,
            max,
        } => {
            let (min, max) = normalize_bounds(*min, *max);
            if *std_dev <= 0.0 {
                return min;
            }
            let normal = Normal::new(*mean, *std_dev)
                .or_else(|_| Normal::new(0.0, 1.0))
                .expect("default normal distribution");
            let sample = normal.sample(rng);
            clamp_sample(sample, min, max)
        }
        MessageSizeDistribution::LogNormal {
            mean,
            std_dev,
            min,
            max,
        } => {
            let (min, max) = normalize_bounds(*min, *max);
            if *std_dev <= 0.0 {
                return min;
            }
            let log_normal = LogNormal::new(*mean, *std_dev)
                .or_else(|_| LogNormal::new(0.0, 1.0))
                .expect("default log-normal distribution");
            let sample = log_normal.sample(rng);
            clamp_sample(sample, min, max)
        }
    }
}

fn clamp_sample(sample: f64, min: usize, max: usize) -> usize {
    if !sample.is_finite() {
        return min;
    }
    let clamped = sample.max(min as f64).min(max as f64);
    clamped.round() as usize
}

fn normalize_bounds(min: usize, max: usize) -> (usize, usize) {
    if min <= max {
        (min, max)
    } else {
        (max, min)
    }
}

pub async fn start_relay(config: RelayConfig) -> (String, oneshot::Sender<()>) {
    let state = RelayState::new(config);
    state.start_peer_log_task();
    let app: Router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    });
    tokio::spawn(async move {
        let _ = server.await;
    });

    (format!("http://{}", addr), shutdown_tx)
}

async fn post_envelope(base_url: &str, envelope: &Envelope) -> Result<(), String> {
    let body = serde_json::to_string(envelope).map_err(|err| err.to_string())?;
    tokio::task::spawn_blocking({
        let base_url = base_url.to_string();
        let body = body.clone();
        move || {
            let response = ureq::post(&format!("{}/envelopes", base_url))
                .set("Content-Type", "application/json")
                .send_string(&body)
                .map_err(|err| err.to_string())?;
            if response.status() >= 400 {
                return Err(format!("relay rejected envelope: {}", response.status()));
            }
            Ok(())
        }
    })
    .await
    .map_err(|err| err.to_string())?
}

async fn fetch_inbox(base_url: &str, recipient_id: &str) -> Result<Vec<Envelope>, String> {
    tokio::task::spawn_blocking({
        let base_url = base_url.to_string();
        let recipient_id = recipient_id.to_string();
        move || {
            let response = ureq::get(&format!("{}/inbox/{}", base_url, recipient_id))
                .call()
                .map_err(|err| err.to_string())?;
            let body = response.into_string().map_err(|err| err.to_string())?;
            serde_json::from_str(&body).map_err(|err| err.to_string())
        }
    })
    .await
    .map_err(|err| err.to_string())?
}
