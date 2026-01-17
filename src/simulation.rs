use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use rand::distributions::Uniform;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};
use rand_distr::{Distribution, LogNormal, Normal};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

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
const SIMULATION_HOURS_PER_DAY: usize = 24;
const MESSAGE_KIND_SUMMARIES: [MessageKind; 5] = [
    MessageKind::Public,
    MessageKind::Meta,
    MessageKind::Direct,
    MessageKind::FriendGroup,
    MessageKind::StoreForPeer,
];

fn default_seconds_per_step() -> u64 {
    60
}

fn default_speed_factor() -> f64 {
    1.0
}

fn default_simulated_time() -> SimulatedTimeConfig {
    SimulatedTimeConfig {
        seconds_per_step: default_seconds_per_step(),
        default_speed_factor: default_speed_factor(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub node_ids: Vec<String>,
    pub steps: usize,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
    #[serde(default = "default_simulated_time")]
    pub simulated_time: SimulatedTimeConfig,
    pub friends_per_node: FriendsPerNode,
    pub clustering: Option<ClusteringConfig>,
    pub post_frequency: PostFrequency,
    #[serde(default)]
    pub availability: Option<OnlineAvailability>,
    #[serde(default)]
    pub cohorts: Vec<OnlineCohortDefinition>,
    pub message_size_distribution: MessageSizeDistribution,
    pub encryption: Option<MessageEncryption>,
    pub seed: u64,
}

impl SimulationConfig {
    pub fn effective_steps(&self) -> usize {
        if let Some(duration_seconds) = self.duration_seconds {
            let seconds_per_step = self.simulated_time.seconds_per_step.max(1) as f64;
            let steps = (duration_seconds as f64 / seconds_per_step).ceil() as usize;
            return steps.max(1);
        }
        self.steps.max(1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatedTimeConfig {
    #[serde(default = "default_seconds_per_step")]
    pub seconds_per_step: u64,
    #[serde(default = "default_speed_factor")]
    pub default_speed_factor: f64,
}

#[derive(Debug, Clone)]
pub struct SimulationTimingConfig {
    pub base_seconds_per_step: f64,
    pub speed_factor: f64,
    pub base_real_time_per_step: Duration,
}

impl SimulationTimingConfig {
    pub fn simulated_seconds_per_step(&self) -> f64 {
        self.base_seconds_per_step * self.speed_factor.max(0.0)
    }

    pub fn real_time_per_step(&self) -> Duration {
        if self.speed_factor <= 0.0 {
            return Duration::ZERO;
        }
        self.base_real_time_per_step
            .mul_f64(1.0 / self.speed_factor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OnlineCohortDefinition {
    AlwaysOnline {
        name: String,
        share: f64,
    },
    RarelyOnline {
        name: String,
        share: f64,
        online_probability: f64,
    },
    Diurnal {
        name: String,
        share: f64,
        online_probability: f64,
        #[serde(default)]
        timezone_offset_hours: i32,
        #[serde(default)]
        hourly_weights: Vec<f64>,
    },
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
        #[serde(default)]
        lambda_per_step: Option<f64>,
        #[serde(default)]
        lambda_per_hour: Option<f64>,
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
            log_sink: None,
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
pub struct SenderRecipientCount {
    pub sender: String,
    pub recipient: String,
    pub count: usize,
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
    pub per_sender_recipient_counts: Vec<SenderRecipientCount>,
    pub first_delivery_latency: LatencyHistogram,
    pub all_recipients_delivery_latency: LatencyHistogram,
    pub first_delivery_latency_seconds: LatencyHistogram,
    pub all_recipients_delivery_latency_seconds: LatencyHistogram,
    pub cohort_online_rates: HashMap<String, f64>,
    pub online_broadcasts_sent: usize,
    pub acks_received: usize,
    pub missed_message_requests_sent: usize,
    pub missed_message_deliveries: usize,
    pub store_forwards_stored: usize,
    pub store_forwards_forwarded: usize,
    pub store_forwards_delivered: usize,
    pub aggregate_metrics: SimulationAggregateMetrics,
}

#[derive(Debug, Clone, Serialize)]
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
    pub total_peers: usize,
    pub speed_factor: f64,
    pub sent_messages: usize,
    pub received_messages: usize,
    pub rolling_latency: RollingLatencySnapshot,
    pub aggregate_metrics: SimulationAggregateMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct CountSummary {
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub average: Option<f64>,
}

impl CountSummary {
    fn empty() -> Self {
        Self {
            min: None,
            max: None,
            average: None,
        }
    }

    fn from_counts<I: IntoIterator<Item = usize>>(counts: I) -> Self {
        let mut min: Option<usize> = None;
        let mut max: Option<usize> = None;
        let mut total = 0usize;
        let mut samples = 0usize;
        for count in counts {
            min = Some(min.map_or(count, |current| current.min(count)));
            max = Some(max.map_or(count, |current| current.max(count)));
            total = total.saturating_add(count);
            samples = samples.saturating_add(1);
        }
        let average = if samples > 0 {
            Some(total as f64 / samples as f64)
        } else {
            None
        };
        Self { min, max, average }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SizeSummary {
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub average: Option<f64>,
}

impl SizeSummary {
    fn empty() -> Self {
        Self {
            min: None,
            max: None,
            average: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulationAggregateMetrics {
    pub peer_feed_messages: CountSummary,
    pub sent_messages_by_kind: HashMap<MessageKind, CountSummary>,
    pub stored_unforwarded_by_peer: CountSummary,
    pub stored_forwarded_by_peer: CountSummary,
    pub message_size_by_kind: HashMap<MessageKind, SizeSummary>,
}

impl SimulationAggregateMetrics {
    pub fn empty() -> Self {
        Self {
            peer_feed_messages: CountSummary::empty(),
            sent_messages_by_kind: HashMap::new(),
            stored_unforwarded_by_peer: CountSummary::empty(),
            stored_forwarded_by_peer: CountSummary::empty(),
            message_size_by_kind: HashMap::new(),
        }
    }
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
    pub cohort_online_rates: HashMap<String, f64>,
    pub timing: SimulationTimingConfig,
}

#[derive(Debug, Clone)]
pub enum SimulationControlCommand {
    AddPeer { node: Node, keypair: StoredKeypair },
    AddFriendship { peer_a: String, peer_b: String },
    AdjustSpeedFactor { delta: f64 },
    SetSpeedFactor { speed_factor: f64 },
    SetPaused { paused: bool },
    Stop,
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
    timing: SimulationTimingConfig,
    log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
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
        timing: SimulationTimingConfig,
        cohort_online_rates: HashMap<String, f64>,
        log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
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
            metrics_tracker: MetricsTracker::new(
                timing.simulated_seconds_per_step(),
                cohort_online_rates,
            ),
            timing,
            log_sink,
        }
    }

    pub fn metrics(&self) -> SimulationMetrics {
        self.metrics.clone()
    }

    pub fn metrics_report(&self) -> SimulationMetricsReport {
        let aggregate_metrics = self.aggregate_metrics();
        self.metrics_tracker.report(aggregate_metrics)
    }

    fn aggregate_metrics(&self) -> SimulationAggregateMetrics {
        let peer_ids: Vec<String> = self.nodes.keys().cloned().collect();
        let peer_feed_messages = CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
            self.nodes
                .get(peer_id)
                .map(|node| node.inbox.len())
                .unwrap_or(0)
        }));
        let stored_unforwarded_by_peer =
            CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
                self.stored_forwards
                    .get(peer_id)
                    .map(|stored| stored.len())
                    .unwrap_or(0)
            }));
        let sent_messages_by_kind = self.metrics_tracker.sent_message_summary_by_kind(&peer_ids);
        let stored_forwarded_by_peer = self.metrics_tracker.stored_forwarded_summary(&peer_ids);
        let message_size_by_kind = self.metrics_tracker.message_size_summary_by_kind();
        SimulationAggregateMetrics {
            peer_feed_messages,
            sent_messages_by_kind,
            stored_unforwarded_by_peer,
            stored_forwarded_by_peer,
            message_size_by_kind,
        }
    }

    pub fn total_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn speed_factor(&self) -> f64 {
        self.timing.speed_factor
    }

    fn set_speed_factor(&mut self, speed_factor: f64) {
        self.timing.speed_factor = speed_factor.max(0.0);
        self.metrics_tracker
            .set_simulated_seconds_per_step(self.timing.simulated_seconds_per_step());
    }

    fn log_action(&self, step: usize, message: impl Into<String>) {
        if let Some(log_sink) = &self.log_sink {
            log_sink(format!("[sim step={step}] {}", message.into()));
        }
    }

    fn apply_control_command(
        &mut self,
        step: usize,
        command: SimulationControlCommand,
        previous_online: &mut HashMap<String, bool>,
    ) {
        match command {
            SimulationControlCommand::AddPeer { node, keypair } => {
                let node_id = node.id.clone();
                if self.nodes.contains_key(&node_id) {
                    return;
                }
                self.nodes.insert(node_id.clone(), node);
                self.keypairs.insert(node_id.clone(), keypair);
                self.last_seen_by_peer
                    .insert(node_id.clone(), HashMap::new());
                self.neighbors.entry(node_id.clone()).or_default();
                previous_online.entry(node_id.clone()).or_insert(false);
                self.log_action(step, format!("{node_id} added"));
            }
            SimulationControlCommand::AddFriendship { peer_a, peer_b } => {
                if !(self.nodes.contains_key(&peer_a) && self.nodes.contains_key(&peer_b)) {
                    return;
                }
                if self.direct_links.insert((peer_a.clone(), peer_b.clone())) {
                    self.neighbors
                        .entry(peer_a.clone())
                        .or_default()
                        .push(peer_b.clone());
                }
                if self.direct_links.insert((peer_b.clone(), peer_a.clone())) {
                    self.neighbors
                        .entry(peer_b.clone())
                        .or_default()
                        .push(peer_a.clone());
                }
                self.log_action(
                    step,
                    format!("friendship added between {peer_a} and {peer_b}"),
                );
            }
            SimulationControlCommand::AdjustSpeedFactor { delta } => {
                let next = self.timing.speed_factor + delta;
                self.set_speed_factor(next);
                self.log_action(
                    step,
                    format!("speed factor adjusted to {:.2}", self.timing.speed_factor),
                );
            }
            SimulationControlCommand::SetSpeedFactor { speed_factor } => {
                self.set_speed_factor(speed_factor);
                self.log_action(
                    step,
                    format!("speed factor set to {:.2}", self.timing.speed_factor),
                );
            }
            SimulationControlCommand::SetPaused { .. } | SimulationControlCommand::Stop => {}
        }
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

    async fn enqueue_online_broadcasts(&mut self, node_id: &str, step: usize) {
        let Some(neighbors) = self.neighbors.get(node_id) else {
            return;
        };
        let neighbors = neighbors.clone();
        for neighbor in neighbors {
            self.log_action(
                step + 1,
                format!("{node_id} announced online to {neighbor}"),
            );
            let meta = MetaMessage::Online {
                peer_id: node_id.to_string(),
                timestamp: step as u64,
            };
            self.send_meta_message(node_id, &neighbor, step, &meta)
                .await;
            self.pending_online_broadcasts.push(PendingOnlineBroadcast {
                sender_id: node_id.to_string(),
                recipient_id: neighbor.clone(),
                sent_step: step,
                expires_at: step.saturating_add(SIMULATION_ACK_WINDOW_STEPS),
            });
            self.metrics_tracker.record_online_broadcast();
        }
    }

    async fn process_pending_online_broadcasts(
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
                self.send_meta_message(&pending.recipient_id, &pending.sender_id, step, &ack)
                    .await;
                self.metrics_tracker.record_ack();
                self.log_action(
                    step + 1,
                    format!(
                        "{} acknowledged {} online",
                        pending.recipient_id, pending.sender_id
                    ),
                );
                delivered = delivered.saturating_add(
                    self.handle_message_request(&pending.sender_id, &pending.recipient_id, step)
                        .await,
                );
                continue;
            }
            remaining.push(pending);
        }
        self.pending_online_broadcasts = remaining;
        delivered
    }

    async fn handle_message_request(
        &mut self,
        requester: &str,
        responder: &str,
        step: usize,
    ) -> usize {
        let last_seen = self.last_seen_for(requester, responder);
        let request = MetaMessage::MessageRequest {
            peer_id: responder.to_string(),
            since_timestamp: last_seen,
        };
        self.send_meta_message(requester, responder, step, &request)
            .await;
        self.metrics_tracker.record_missed_message_request();
        self.log_action(
            step + 1,
            format!("{requester} requested missed messages from {responder}"),
        );
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
        if delivered > 0 {
            self.log_action(
                step + 1,
                format!("{responder} delivered {delivered} missed messages to {requester}"),
            );
        }
        delivered
    }

    async fn send_meta_message(
        &mut self,
        sender_id: &str,
        recipient_id: &str,
        step: usize,
        meta: &MetaMessage,
    ) {
        let Ok(payload) = build_meta_payload(meta) else {
            return;
        };
        let Ok(envelope) = build_envelope_from_payload(
            sender_id.to_string(),
            recipient_id.to_string(),
            None,
            None,
            step as u64,
            self.ttl_seconds,
            MessageKind::Meta,
            None,
            payload,
        ) else {
            return;
        };
        self.metrics_tracker
            .record_meta_send(sender_id.to_string(), &envelope);
        let _ = post_envelope(&self.relay_base_url, &envelope).await;
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
            self.log_action(
                step + 1,
                format!("{recipient_id} received missed message {}", message.id),
            );
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

    fn store_forward_message(
        &mut self,
        storage_peer_id: &str,
        step: usize,
        message: StoredForPeerMessage,
    ) {
        self.stored_forwards
            .entry(storage_peer_id.to_string())
            .or_default()
            .push(message);
        self.metrics.store_forwards_stored = self.metrics.store_forwards_stored.saturating_add(1);
        self.metrics_tracker.record_store_forward_stored();
        self.log_action(
            step + 1,
            format!("{storage_peer_id} stored a store-forward message"),
        );
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

    async fn forward_store_forwards(&mut self, step: usize, online_set: &HashSet<String>) -> usize {
        let mut forwarded = 0usize;
        let mut log_entries = Vec::new();
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
                        self.metrics_tracker
                            .record_store_forward_forwarded(&storage_peer_id);
                        log_entries.push(format!(
                            "{storage_peer_id} forwarded stored message to {}",
                            entry.store_for_id
                        ));
                        forwarded = forwarded.saturating_add(1);
                    } else {
                        remaining.push(entry);
                    }
                }
                *stored = remaining;
            }
        }
        for entry in log_entries {
            self.log_action(step + 1, entry);
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
            let offline: Vec<String> = previous_online
                .iter()
                .filter(|(id, was_online)| **was_online && !online_set.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();

            for node_id in &newly_online {
                self.log_action(step + 1, format!("{node_id} came online"));
            }
            for node_id in &offline {
                self.log_action(step + 1, format!("{node_id} went offline"));
            }

            let _ = self.forward_store_forwards(step, &online_set).await;

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
                            self.store_forward_message(node_id, step, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &newly_online {
                self.enqueue_online_broadcasts(node_id, step).await;
            }

            let _ = self
                .process_pending_online_broadcasts(step, &online_set)
                .await;

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
                            self.store_forward_message(node_id, step, message);
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
        let (_control_tx, control_rx) = mpsc::unbounded_channel();
        self.run_with_progress_and_controls(steps, plan, control_rx, |update| {
            on_step(update);
        })
        .await
    }

    pub async fn run_with_progress_and_controls<F>(
        &mut self,
        steps: usize,
        plan: Vec<PlannedSend>,
        mut control_rx: mpsc::UnboundedReceiver<SimulationControlCommand>,
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
        let mut paused = false;
        let mut stop_requested = false;

        let mut step = 0usize;
        while step < steps {
            while let Ok(command) = control_rx.try_recv() {
                match command {
                    SimulationControlCommand::SetPaused {
                        paused: should_pause,
                    } => {
                        paused = should_pause;
                        if paused {
                            self.log_action(step + 1, "simulation paused".to_string());
                        } else {
                            self.log_action(step + 1, "simulation resumed".to_string());
                        }
                    }
                    SimulationControlCommand::Stop => {
                        stop_requested = true;
                    }
                    other => self.apply_control_command(step + 1, other, &mut previous_online),
                }
            }
            if stop_requested {
                break;
            }
            if paused {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
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
            let offline: Vec<String> = previous_online
                .iter()
                .filter(|(id, was_online)| **was_online && !online_set.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();

            for node_id in &newly_online {
                self.log_action(step + 1, format!("{node_id} came online"));
            }
            for node_id in &offline {
                self.log_action(step + 1, format!("{node_id} went offline"));
            }

            let _ = self.forward_store_forwards(step, &online_set).await;

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
                            self.store_forward_message(node_id, step, message);
                        }
                        None => {}
                    }
                }
            }

            for node_id in &newly_online {
                self.enqueue_online_broadcasts(node_id, step).await;
            }

            let missed_deliveries = self
                .process_pending_online_broadcasts(step, &online_set)
                .await;
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
                            self.store_forward_message(node_id, step, message);
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
                total_peers: self.nodes.len(),
                speed_factor: self.timing.speed_factor,
                sent_messages,
                received_messages,
                rolling_latency: rolling_latency.snapshot(),
                aggregate_metrics: self.aggregate_metrics(),
            };
            on_step(update);
            let delay = self.timing.real_time_per_step();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            step += 1;
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
                    self.metrics_tracker
                        .record_send(message, step, &store_envelope);
                    let _ = post_envelope(&self.relay_base_url, &store_envelope).await;
                    return direct_latency;
                }
            }
        }

        self.metrics_tracker.record_send(message, step, &envelope);
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
struct SizeStats {
    total: u64,
    samples: usize,
    min: Option<u64>,
    max: Option<u64>,
}

impl SizeStats {
    fn record(&mut self, size: u64) {
        self.total = self.total.saturating_add(size);
        self.samples = self.samples.saturating_add(1);
        self.min = Some(self.min.map_or(size, |current| current.min(size)));
        self.max = Some(self.max.map_or(size, |current| current.max(size)));
    }

    fn report(&self) -> SizeSummary {
        let average = if self.samples > 0 {
            Some(self.total as f64 / self.samples as f64)
        } else {
            None
        };
        SizeSummary {
            min: self.min,
            max: self.max,
            average,
        }
    }
}

#[derive(Debug)]
struct MetricsTracker {
    per_sender_recipient_counts: HashMap<SenderRecipient, usize>,
    first_delivery_latency: LatencyStats,
    all_recipients_delivery_latency: LatencyStats,
    first_delivery_latency_seconds: LatencyStats,
    all_recipients_delivery_latency_seconds: LatencyStats,
    messages: HashMap<String, MessageTracking>,
    per_peer_sent_by_kind: HashMap<String, HashMap<MessageKind, usize>>,
    per_peer_store_forwarded: HashMap<String, usize>,
    message_sizes_by_kind: HashMap<MessageKind, SizeStats>,
    cohort_online_rates: HashMap<String, f64>,
    simulated_seconds_per_step: f64,
    online_broadcasts_sent: usize,
    acks_received: usize,
    missed_message_requests_sent: usize,
    missed_message_deliveries: usize,
    store_forwards_stored: usize,
    store_forwards_forwarded: usize,
    store_forwards_delivered: usize,
}

impl MetricsTracker {
    fn new(simulated_seconds_per_step: f64, cohort_online_rates: HashMap<String, f64>) -> Self {
        Self {
            per_sender_recipient_counts: HashMap::new(),
            first_delivery_latency: LatencyStats::default(),
            all_recipients_delivery_latency: LatencyStats::default(),
            first_delivery_latency_seconds: LatencyStats::default(),
            all_recipients_delivery_latency_seconds: LatencyStats::default(),
            messages: HashMap::new(),
            per_peer_sent_by_kind: HashMap::new(),
            per_peer_store_forwarded: HashMap::new(),
            message_sizes_by_kind: HashMap::new(),
            cohort_online_rates,
            simulated_seconds_per_step: simulated_seconds_per_step.max(0.0),
            online_broadcasts_sent: 0,
            acks_received: 0,
            missed_message_requests_sent: 0,
            missed_message_deliveries: 0,
            store_forwards_stored: 0,
            store_forwards_forwarded: 0,
            store_forwards_delivered: 0,
        }
    }

    fn set_simulated_seconds_per_step(&mut self, simulated_seconds_per_step: f64) {
        self.simulated_seconds_per_step = simulated_seconds_per_step.max(0.0);
    }

    fn record_send(&mut self, message: &SimMessage, step: usize, envelope: &Envelope) {
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
        let sender_entry = self
            .per_peer_sent_by_kind
            .entry(message.sender.clone())
            .or_default();
        let kind = envelope.header.message_kind.clone();
        *sender_entry.entry(kind.clone()).or_insert(0) += 1;
        self.message_sizes_by_kind
            .entry(kind)
            .or_default()
            .record(envelope.header.payload_size);
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
            let latency_seconds =
                ((latency as f64) * self.simulated_seconds_per_step).round() as usize;
            self.first_delivery_latency_seconds.record(latency_seconds);
            tracking.first_delivery_recorded = true;
        }
        if tracking.delivered_recipients.len() == tracking.recipients.len()
            && !tracking.all_delivery_recorded
        {
            self.all_recipients_delivery_latency.record(latency);
            let latency_seconds =
                ((latency as f64) * self.simulated_seconds_per_step).round() as usize;
            self.all_recipients_delivery_latency_seconds
                .record(latency_seconds);
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

    fn record_store_forward_forwarded(&mut self, storage_peer_id: &str) {
        self.store_forwards_forwarded = self.store_forwards_forwarded.saturating_add(1);
        *self
            .per_peer_store_forwarded
            .entry(storage_peer_id.to_string())
            .or_insert(0) += 1;
    }

    fn record_store_forward_delivery(&mut self) {
        self.store_forwards_delivered = self.store_forwards_delivered.saturating_add(1);
    }

    fn record_meta_send(&mut self, sender_id: String, envelope: &Envelope) {
        let sender_entry = self.per_peer_sent_by_kind.entry(sender_id).or_default();
        let kind = envelope.header.message_kind.clone();
        *sender_entry.entry(kind.clone()).or_insert(0) += 1;
        self.message_sizes_by_kind
            .entry(kind)
            .or_default()
            .record(envelope.header.payload_size);
    }

    fn sent_message_summary_by_kind(
        &self,
        peer_ids: &[String],
    ) -> HashMap<MessageKind, CountSummary> {
        let mut summaries = HashMap::new();
        for kind in MESSAGE_KIND_SUMMARIES.iter().cloned() {
            let kind_for_counts = kind.clone();
            let counts = peer_ids.iter().map(|peer_id| {
                self.per_peer_sent_by_kind
                    .get(peer_id)
                    .and_then(|counts| counts.get(&kind_for_counts))
                    .copied()
                    .unwrap_or(0)
            });
            summaries.insert(kind, CountSummary::from_counts(counts));
        }
        summaries
    }

    fn stored_forwarded_summary(&self, peer_ids: &[String]) -> CountSummary {
        CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
            self.per_peer_store_forwarded
                .get(peer_id)
                .copied()
                .unwrap_or(0)
        }))
    }

    fn message_size_summary_by_kind(&self) -> HashMap<MessageKind, SizeSummary> {
        MESSAGE_KIND_SUMMARIES
            .iter()
            .cloned()
            .map(|kind| {
                let summary = self
                    .message_sizes_by_kind
                    .get(&kind)
                    .map(SizeStats::report)
                    .unwrap_or_else(SizeSummary::empty);
                (kind, summary)
            })
            .collect()
    }

    fn report(&self, aggregate_metrics: SimulationAggregateMetrics) -> SimulationMetricsReport {
        let mut per_sender_recipient_counts: Vec<SenderRecipientCount> = self
            .per_sender_recipient_counts
            .iter()
            .map(|(key, count)| SenderRecipientCount {
                sender: key.sender.clone(),
                recipient: key.recipient.clone(),
                count: *count,
            })
            .collect();
        per_sender_recipient_counts.sort_by(|left, right| {
            left.sender
                .cmp(&right.sender)
                .then_with(|| left.recipient.cmp(&right.recipient))
        });
        SimulationMetricsReport {
            per_sender_recipient_counts,
            first_delivery_latency: self.first_delivery_latency.report(),
            all_recipients_delivery_latency: self.all_recipients_delivery_latency.report(),
            first_delivery_latency_seconds: self.first_delivery_latency_seconds.report(),
            all_recipients_delivery_latency_seconds: self
                .all_recipients_delivery_latency_seconds
                .report(),
            cohort_online_rates: self.cohort_online_rates.clone(),
            online_broadcasts_sent: self.online_broadcasts_sent,
            acks_received: self.acks_received,
            missed_message_requests_sent: self.missed_message_requests_sent,
            missed_message_deliveries: self.missed_message_deliveries,
            store_forwards_stored: self.store_forwards_stored,
            store_forwards_forwarded: self.store_forwards_forwarded,
            store_forwards_delivered: self.store_forwards_delivered,
            aggregate_metrics,
        }
    }
}

pub async fn run_simulation_scenario(
    scenario: SimulationScenarioConfig,
) -> Result<SimulationReport, String> {
    let relay_config = scenario.relay.clone();
    let (base_url, shutdown_tx) = start_relay(relay_config.clone().into_relay_config()).await;
    let inputs = build_simulation_inputs(&scenario.simulation);
    let steps = scenario.simulation.effective_steps();
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.nodes,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        inputs.timing,
        inputs.cohort_online_rates,
        None,
    );
    let metrics = harness.run(steps, inputs.planned_sends).await;
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
    let steps = scenario.simulation.effective_steps();
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.nodes,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        inputs.timing,
        inputs.cohort_online_rates,
        None,
    );
    let metrics = harness
        .run_with_progress(steps, inputs.planned_sends, |update| {
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
    let schedule_plan = build_online_schedules(config, &mut rng);
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
        let schedule = schedule_plan
            .schedules
            .get(node_id)
            .cloned()
            .unwrap_or_default();
        nodes.push(Node::new(node_id, schedule));
    }
    SimulationInputs {
        nodes,
        planned_sends,
        direct_links,
        keypairs,
        encryption,
        cohort_online_rates: schedule_plan.cohort_online_rates,
        timing: SimulationTimingConfig {
            base_seconds_per_step: config.simulated_time.seconds_per_step as f64,
            speed_factor: config.simulated_time.default_speed_factor.max(0.0),
            base_real_time_per_step: Duration::ZERO,
        },
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

fn simulated_seconds_per_step(config: &SimulationConfig) -> f64 {
    let seconds = config.simulated_time.seconds_per_step as f64;
    let factor = config.simulated_time.default_speed_factor.max(0.0);
    seconds * factor
}

fn select_cohort<'a>(
    cohorts: &'a [OnlineCohortDefinition],
    rng: &mut impl Rng,
) -> &'a OnlineCohortDefinition {
    let weights: Vec<f64> = cohorts.iter().map(cohort_share).collect();
    let index = sample_weighted_index(rng, &weights);
    cohorts
        .get(index)
        .unwrap_or_else(|| cohorts.first().expect("cohort list cannot be empty"))
}

fn cohort_share(cohort: &OnlineCohortDefinition) -> f64 {
    match cohort {
        OnlineCohortDefinition::AlwaysOnline { share, .. }
        | OnlineCohortDefinition::RarelyOnline { share, .. }
        | OnlineCohortDefinition::Diurnal { share, .. } => *share,
    }
}

fn cohort_name(cohort: &OnlineCohortDefinition) -> &str {
    match cohort {
        OnlineCohortDefinition::AlwaysOnline { name, .. }
        | OnlineCohortDefinition::RarelyOnline { name, .. }
        | OnlineCohortDefinition::Diurnal { name, .. } => name,
    }
}

fn build_availability_schedule(config: &SimulationConfig, rng: &mut impl Rng) -> Vec<bool> {
    let steps = config.effective_steps();
    let Some(availability) = &config.availability else {
        return vec![true; steps];
    };
    match availability {
        OnlineAvailability::Bernoulli { p_online } => {
            (0..steps).map(|_| rng.gen::<f64>() < *p_online).collect()
        }
        OnlineAvailability::Markov {
            p_online_given_online,
            p_online_given_offline,
            start_online_prob,
        } => {
            let mut schedule = Vec::with_capacity(steps);
            let mut online = rng.gen::<f64>() < *start_online_prob;
            for _ in 0..steps {
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
    }
}

fn build_cohort_schedule(
    cohort: &OnlineCohortDefinition,
    steps: usize,
    simulated_seconds_per_step: f64,
    rng: &mut impl Rng,
) -> Vec<bool> {
    match cohort {
        OnlineCohortDefinition::AlwaysOnline { .. } => vec![true; steps],
        OnlineCohortDefinition::RarelyOnline {
            online_probability, ..
        } => (0..steps)
            .map(|_| rng.gen::<f64>() < online_probability.clamp(0.0, 1.0))
            .collect(),
        OnlineCohortDefinition::Diurnal {
            online_probability,
            timezone_offset_hours,
            hourly_weights,
            ..
        } => {
            let weights = normalize_hourly_weights(hourly_weights);
            let max_weight = weights
                .iter()
                .copied()
                .fold(0.0_f64, |max, weight| max.max(weight));
            let max_weight = if max_weight > 0.0 { max_weight } else { 1.0 };
            let base_probability = online_probability.clamp(0.0, 1.0);
            (0..steps)
                .map(|step| {
                    let hour = ((step as f64 * simulated_seconds_per_step) / 3600.0).floor() as i64;
                    let hour = (hour + *timezone_offset_hours as i64)
                        .rem_euclid(SIMULATION_HOURS_PER_DAY as i64)
                        as usize;
                    let weight_factor = weights[hour] / max_weight;
                    rng.gen::<f64>() < (base_probability * weight_factor).clamp(0.0, 1.0)
                })
                .collect()
        }
    }
}

fn normalize_hourly_weights(weights: &[f64]) -> Vec<f64> {
    if weights.len() == SIMULATION_HOURS_PER_DAY {
        return weights.to_vec();
    }
    vec![
        0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95, 0.9, 0.8, 0.75, 0.8, 0.9,
        1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25,
    ]
}

pub struct OnlineSchedulePlan {
    pub schedules: HashMap<String, Vec<bool>>,
    pub cohort_online_rates: HashMap<String, f64>,
}

pub fn build_online_schedules(config: &SimulationConfig, rng: &mut impl Rng) -> OnlineSchedulePlan {
    let mut schedules = HashMap::new();
    let mut cohort_online_counts: HashMap<String, usize> = HashMap::new();
    let mut cohort_total_counts: HashMap<String, usize> = HashMap::new();
    let simulated_seconds_per_step = simulated_seconds_per_step(config);
    let steps = config.effective_steps();
    let cohorts = if config.cohorts.is_empty() {
        None
    } else {
        Some(config.cohorts.as_slice())
    };

    for node_id in &config.node_ids {
        let (cohort_name, schedule) = if let Some(cohorts) = cohorts {
            let cohort = select_cohort(cohorts, rng);
            let name = cohort_name(cohort).to_string();
            let schedule = build_cohort_schedule(cohort, steps, simulated_seconds_per_step, rng);
            (name, schedule)
        } else {
            let name = "legacy".to_string();
            let schedule = build_availability_schedule(config, rng);
            (name, schedule)
        };

        let online_count = schedule.iter().filter(|online| **online).count();
        *cohort_online_counts.entry(cohort_name.clone()).or_insert(0) += online_count;
        *cohort_total_counts.entry(cohort_name.clone()).or_insert(0) += schedule.len();
        schedules.insert(node_id.clone(), schedule);
    }

    let cohort_online_rates = cohort_total_counts
        .into_iter()
        .map(|(cohort, total)| {
            let online = cohort_online_counts.get(&cohort).copied().unwrap_or(0);
            let rate = if total > 0 {
                online as f64 / total as f64
            } else {
                0.0
            };
            (cohort, rate)
        })
        .collect();

    OnlineSchedulePlan {
        schedules,
        cohort_online_rates,
    }
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
    let steps = config.effective_steps();
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
            PostFrequency::Poisson {
                lambda_per_step,
                lambda_per_hour,
            } => {
                let simulated_seconds_per_step = simulated_seconds_per_step(config).max(1.0);
                let per_step = if let Some(lambda_per_hour) = lambda_per_hour {
                    lambda_per_hour * (simulated_seconds_per_step / 3600.0)
                } else {
                    lambda_per_step.unwrap_or(0.0)
                };
                for step in 0..steps {
                    let count = sample_poisson(rng, per_step);
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
                let simulated_seconds_per_step = simulated_seconds_per_step(config).max(1.0);
                let use_hourly_weights = weights.len() == SIMULATION_HOURS_PER_DAY;
                let weights = if weights.len() == steps || use_hourly_weights {
                    weights.clone()
                } else {
                    vec![1.0; steps.max(1)]
                };
                for _ in 0..*total_posts {
                    let step = if use_hourly_weights {
                        let hour = sample_weighted_index(rng, &weights);
                        let offset_seconds = rng.gen_range(0.0..3600.0);
                        let simulated_seconds = (hour as f64 * 3600.0) + offset_seconds;
                        let step =
                            (simulated_seconds / simulated_seconds_per_step).floor() as usize;
                        step.min(steps.saturating_sub(1))
                    } else {
                        sample_weighted_index(rng, &weights)
                    };
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
