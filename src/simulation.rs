use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Router;
use rand::distributions::Uniform;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};
use rand_distr::{Distribution, LogNormal, Normal};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::crypto::{
    generate_keypair, generate_keypair_with_rng, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE,
};
use crate::protocol::{
    build_encrypted_payload, build_plaintext_payload, ContentId, Envelope, MessageKind, Payload,
};
use crate::relay::{app, RelayConfig, RelayControl, RelayState};

pub use crate::client::SimulationClient;
use crate::client::{Client, ClientContext, ClientLogEvent, ClientLogSink, ClientMetrics};

pub const SIMULATION_PAYLOAD_AAD: &[u8] = b"tenet-simulation";
pub const SIMULATION_HPKE_INFO: &[u8] = b"tenet-simulation-hpke";
pub const SIMULATION_ACK_WINDOW_STEPS: usize = 10;
pub const SIMULATION_HOURS_PER_DAY: usize = 24;
pub const MESSAGE_KIND_SUMMARIES: [MessageKind; 5] = [
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
            pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
    pub client_metrics: HashMap<String, ClientMetrics>,
}

impl SimulationAggregateMetrics {
    pub fn empty() -> Self {
        Self {
            peer_feed_messages: CountSummary::empty(),
            sent_messages_by_kind: HashMap::new(),
            stored_unforwarded_by_peer: CountSummary::empty(),
            stored_forwarded_by_peer: CountSummary::empty(),
            message_size_by_kind: HashMap::new(),
            client_metrics: HashMap::new(),
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
pub struct PlannedSend {
    pub step: usize,
    pub message: SimMessage,
}

#[derive(Debug, Clone)]
pub struct HistoricalMessage {
    pub send_step: usize,
    pub envelope: Envelope,
}

#[derive(Debug)]
pub struct SimulationInputs {
    pub clients: Vec<SimulationClient>,
    pub planned_sends: Vec<PlannedSend>,
    pub direct_links: HashSet<(String, String)>,
    pub keypairs: HashMap<String, StoredKeypair>,
    pub encryption: MessageEncryption,
    pub cohort_online_rates: HashMap<String, f64>,
    pub timing: SimulationTimingConfig,
}

#[derive(Debug, Clone)]
pub enum SimulationControlCommand {
    AddPeer {
        client: SimulationClient,
        keypair: StoredKeypair,
    },
    AddFriendship {
        peer_a: String,
        peer_b: String,
    },
    AdjustSpeedFactor {
        delta: f64,
    },
    SetSpeedFactor {
        speed_factor: f64,
    },
    SetPaused {
        paused: bool,
    },
    Stop,
}

pub struct SimulationHarness {
    relay_base_url: String,
    clients: HashMap<String, Box<dyn Client>>,
    direct_links: HashSet<(String, String)>,
    neighbors: HashMap<String, Vec<String>>,
    direct_enabled: bool,
    ttl_seconds: u64,
    encryption: MessageEncryption,
    keypairs: HashMap<String, StoredKeypair>,
    message_history: HashMap<(String, String), Vec<HistoricalMessage>>,
    message_send_steps: HashMap<String, usize>,
    pending_forwarded_messages: HashSet<String>,
    metrics: SimulationMetrics,
    metrics_tracker: MetricsTracker,
    timing: SimulationTimingConfig,
    log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    client_log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
    relay_control: Option<RelayControl>,
}

impl SimulationHarness {
    pub fn build_client(node_id: &str, schedule: Vec<bool>) -> SimulationClient {
        SimulationClient::new(node_id, schedule, None)
    }

    pub fn build_peer(node_id: &str, schedule: Vec<bool>) -> (SimulationClient, StoredKeypair) {
        (Self::build_client(node_id, schedule), generate_keypair())
    }

    pub fn new(
        relay_base_url: String,
        clients: Vec<SimulationClient>,
        direct_links: HashSet<(String, String)>,
        direct_enabled: bool,
        ttl_seconds: u64,
        encryption: MessageEncryption,
        keypairs: HashMap<String, StoredKeypair>,
        timing: SimulationTimingConfig,
        cohort_online_rates: HashMap<String, f64>,
        log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
        relay_control: Option<RelayControl>,
    ) -> Self {
        let planned_messages = 0;
        let mut neighbors: HashMap<String, Vec<String>> = HashMap::new();
        for (sender, recipient) in &direct_links {
            neighbors
                .entry(sender.clone())
                .or_default()
                .push(recipient.clone());
        }
        let client_log_sink = log_sink.as_ref().map(|log_sink| {
            let log_sink = std::sync::Arc::clone(log_sink);
            std::sync::Arc::new(move |event: ClientLogEvent| {
                log_sink(format!(
                    "[sim step={}] {} {}",
                    event.step, event.client_id, event.message
                ));
            }) as std::sync::Arc<dyn ClientLogSink>
        });
        Self {
            relay_base_url,
            clients: clients
                .into_iter()
                .map(|mut client| {
                    if let Some(log_sink) = client_log_sink.clone() {
                        client.set_log_sink(Some(log_sink));
                    }
                    (client.id().to_string(), Box::new(client) as Box<dyn Client>)
                })
                .collect(),
            direct_links,
            neighbors,
            direct_enabled,
            ttl_seconds,
            encryption,
            keypairs,
            message_history: HashMap::new(),
            message_send_steps: HashMap::new(),
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
            client_log_sink,
            relay_control,
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
        let peer_ids: Vec<String> = self.clients.keys().cloned().collect();
        let peer_feed_messages = CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
            self.clients
                .get(peer_id)
                .map(|client| client.inbox().len())
                .unwrap_or(0)
        }));
        let stored_unforwarded_by_peer =
            CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
                self.clients
                    .get(peer_id)
                    .map(|client| client.stored_forward_count())
                    .unwrap_or(0)
            }));
        let sent_messages_by_kind = self.metrics_tracker.sent_message_summary_by_kind(&peer_ids);
        let stored_forwarded_by_peer = self.metrics_tracker.stored_forwarded_summary(&peer_ids);
        let message_size_by_kind = self.metrics_tracker.message_size_summary_by_kind();
        let client_metrics = self.collect_client_metrics();
        SimulationAggregateMetrics {
            peer_feed_messages,
            sent_messages_by_kind,
            stored_unforwarded_by_peer,
            stored_forwarded_by_peer,
            message_size_by_kind,
            client_metrics,
        }
    }

    pub fn total_nodes(&self) -> usize {
        self.clients.len()
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

    fn collect_client_metrics(&self) -> HashMap<String, ClientMetrics> {
        self.clients
            .iter()
            .map(|(id, client)| (id.clone(), client.metrics()))
            .collect()
    }

    fn apply_control_command(
        &mut self,
        step: usize,
        command: SimulationControlCommand,
        previous_online: &mut HashMap<String, bool>,
    ) {
        match command {
            SimulationControlCommand::AddPeer { client, keypair } => {
                let mut client = client;
                let client_id = client.id().to_string();
                if self.clients.contains_key(&client_id) {
                    return;
                }
                if let Some(log_sink) = self.client_log_sink.clone() {
                    client.set_log_sink(Some(log_sink));
                }
                self.clients
                    .insert(client_id.clone(), Box::new(client) as Box<dyn Client>);
                self.keypairs.insert(client_id.clone(), keypair);
                self.neighbors.entry(client_id.clone()).or_default();
                previous_online.entry(client_id.clone()).or_insert(false);
                self.log_action(step, format!("{client_id} added"));
            }
            SimulationControlCommand::AddFriendship { peer_a, peer_b } => {
                if !(self.clients.contains_key(&peer_a) && self.clients.contains_key(&peer_b)) {
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
            self.clients.keys().map(|id| (id.clone(), false)).collect();
        let direct_enabled = self.direct_enabled;
        let ttl_seconds = self.ttl_seconds;
        let encryption = self.encryption.clone();
        let direct_links = &self.direct_links;
        let neighbors = &self.neighbors;
        let keypairs = &self.keypairs;
        let relay_base_url = self.relay_base_url.clone();
        let log_sink = self.log_sink.clone();
        let (
            clients,
            message_history,
            message_send_steps,
            pending_forwarded_messages,
            metrics,
            metrics_tracker,
        ) = (
            &mut self.clients,
            &mut self.message_history,
            &mut self.message_send_steps,
            &mut self.pending_forwarded_messages,
            &mut self.metrics,
            &mut self.metrics_tracker,
        );

        for step in 0..steps {
            let online_ids: Vec<String> = clients
                .iter()
                .filter(|(_, client)| client.is_online(step))
                .map(|(id, _)| id.clone())
                .collect();
            let online_set: HashSet<String> = online_ids.iter().cloned().collect();
            let mut pending_direct_deliveries = Vec::new();
            let mut outbound_envelopes = Vec::new();
            if let Some(messages) = sends_by_step.get(&step) {
                for message in messages {
                    if !online_set.contains(&message.sender) {
                        continue;
                    }
                    metrics.sent_messages = metrics.sent_messages.saturating_add(1);
                    if let Some(client) = clients.get_mut(&message.sender) {
                        let mut context = ClientContext {
                            direct_enabled,
                            ttl_seconds,
                            encryption: encryption.clone(),
                            direct_links,
                            neighbors,
                            keypairs,
                            message_history: &mut *message_history,
                            message_send_steps: &mut *message_send_steps,
                            pending_forwarded_messages: &mut *pending_forwarded_messages,
                            metrics: &mut *metrics,
                            metrics_tracker: &mut *metrics_tracker,
                        };
                        let outcome = client.enqueue_sends(
                            step,
                            std::slice::from_ref(message),
                            &online_set,
                            &mut context,
                        );
                        outbound_envelopes.extend(outcome.envelopes);
                        pending_direct_deliveries.extend(outcome.direct_deliveries);
                    }
                }
            }
            for envelope in outbound_envelopes {
                let _ = post_envelope(&relay_base_url, &envelope).await;
            }
            for message in pending_direct_deliveries {
                if let Some(client) = clients.get_mut(&message.recipient) {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    let _ = client.handle_direct_delivery(step, message, &mut context, None);
                }
            }

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
                if let Some(log_sink) = &log_sink {
                    log_sink(format!("[sim step={}] {} came online", step + 1, node_id));
                }
            }
            for node_id in &offline {
                if let Some(log_sink) = &log_sink {
                    log_sink(format!("[sim step={}] {} went offline", step + 1, node_id));
                }
            }

            let mut forward_envelopes = Vec::new();
            for client in clients.values_mut() {
                let mut context = ClientContext {
                    direct_enabled,
                    ttl_seconds,
                    encryption: encryption.clone(),
                    direct_links,
                    neighbors,
                    keypairs,
                    message_history: &mut *message_history,
                    message_send_steps: &mut *message_send_steps,
                    pending_forwarded_messages: &mut *pending_forwarded_messages,
                    metrics: &mut *metrics,
                    metrics_tracker: &mut *metrics_tracker,
                };
                forward_envelopes.extend(client.forward_store_forwards(
                    step,
                    &online_set,
                    &mut context,
                ));
            }
            for envelope in forward_envelopes {
                let _ = post_envelope(&relay_base_url, &envelope).await;
            }

            for node_id in &newly_online {
                let envelopes = fetch_inbox(&relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                if let Some(client) = clients.get_mut(node_id) {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    client.handle_inbox(step, envelopes, &mut context, None);
                }
            }

            let mut online_envelopes = Vec::new();
            for node_id in &newly_online {
                if let Some(client) = clients.get_mut(node_id) {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    online_envelopes.extend(client.announce_online(step, &mut context));
                }
            }
            for envelope in online_envelopes {
                let _ = post_envelope(&relay_base_url, &envelope).await;
            }

            let mut broadcast_envelopes = Vec::new();
            for client in clients.values_mut() {
                let mut context = ClientContext {
                    direct_enabled,
                    ttl_seconds,
                    encryption: encryption.clone(),
                    direct_links,
                    neighbors,
                    keypairs,
                    message_history: &mut *message_history,
                    message_send_steps: &mut *message_send_steps,
                    pending_forwarded_messages: &mut *pending_forwarded_messages,
                    metrics: &mut *metrics,
                    metrics_tracker: &mut *metrics_tracker,
                };
                let outcome =
                    client.process_pending_online_broadcasts(step, &online_set, &mut context);
                broadcast_envelopes.extend(outcome.envelopes);
            }
            for envelope in broadcast_envelopes {
                let _ = post_envelope(&relay_base_url, &envelope).await;
            }

            for node_id in &online_ids {
                if newly_online.contains(node_id) {
                    continue;
                }
                let envelopes = fetch_inbox(&relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                if let Some(client) = clients.get_mut(node_id) {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    client.handle_inbox(step, envelopes, &mut context, None);
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

        metrics.clone()
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
            self.clients.keys().map(|id| (id.clone(), false)).collect();
        let mut paused = false;
        let mut stop_requested = false;
        let direct_enabled = self.direct_enabled;
        let ttl_seconds = self.ttl_seconds;
        let encryption = self.encryption.clone();
        let relay_base_url = self.relay_base_url.clone();
        let log_sink = self.log_sink.clone();

        let mut step = 0usize;
        while step < steps {
            while let Ok(command) = control_rx.try_recv() {
                match command {
                    SimulationControlCommand::SetPaused {
                        paused: should_pause,
                    } => {
                        paused = should_pause;
                        if let Some(relay_control) = &self.relay_control {
                            relay_control.set_paused(paused);
                        }
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
            let (sent_messages, received_messages, online_nodes) = {
                let direct_links = &self.direct_links;
                let neighbors = &self.neighbors;
                let keypairs = &self.keypairs;
                let (
                    clients,
                    message_history,
                    message_send_steps,
                    pending_forwarded_messages,
                    metrics,
                    metrics_tracker,
                ) = (
                    &mut self.clients,
                    &mut self.message_history,
                    &mut self.message_send_steps,
                    &mut self.pending_forwarded_messages,
                    &mut self.metrics,
                    &mut self.metrics_tracker,
                );
                let mut sent_messages = 0usize;
                let mut received_messages = 0usize;
                let online_ids: Vec<String> = clients
                    .iter()
                    .filter(|(_, client)| client.is_online(step))
                    .map(|(id, _)| id.clone())
                    .collect();
                let online_set: HashSet<String> = online_ids.iter().cloned().collect();
                let mut pending_direct_deliveries = Vec::new();
                let mut outbound_envelopes = Vec::new();
                if let Some(messages) = sends_by_step.get(&step) {
                    for message in messages {
                        if !online_set.contains(&message.sender) {
                            continue;
                        }
                        sent_messages = sent_messages.saturating_add(1);
                        metrics.sent_messages = metrics.sent_messages.saturating_add(1);
                        if let Some(client) = clients.get_mut(&message.sender) {
                            let mut context = ClientContext {
                                direct_enabled,
                                ttl_seconds,
                                encryption: encryption.clone(),
                                direct_links,
                                neighbors,
                                keypairs,
                                message_history: &mut *message_history,
                                message_send_steps: &mut *message_send_steps,
                                pending_forwarded_messages: &mut *pending_forwarded_messages,
                                metrics: &mut *metrics,
                                metrics_tracker: &mut *metrics_tracker,
                            };
                            let outcome = client.enqueue_sends(
                                step,
                                std::slice::from_ref(message),
                                &online_set,
                                &mut context,
                            );
                            outbound_envelopes.extend(outcome.envelopes);
                            pending_direct_deliveries.extend(outcome.direct_deliveries);
                        }
                    }
                }
                for envelope in outbound_envelopes {
                    let _ = post_envelope(&relay_base_url, &envelope).await;
                }
                for message in pending_direct_deliveries {
                    if let Some(client) = clients.get_mut(&message.recipient) {
                        let mut context = ClientContext {
                            direct_enabled,
                            ttl_seconds,
                            encryption: encryption.clone(),
                            direct_links,
                            neighbors,
                            keypairs,
                            message_history: &mut *message_history,
                            message_send_steps: &mut *message_send_steps,
                            pending_forwarded_messages: &mut *pending_forwarded_messages,
                            metrics: &mut *metrics,
                            metrics_tracker: &mut *metrics_tracker,
                        };
                        if client
                            .handle_direct_delivery(
                                step,
                                message,
                                &mut context,
                                Some(&mut rolling_latency),
                            )
                            .is_some()
                        {
                            received_messages = received_messages.saturating_add(1);
                        }
                    }
                }
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
                    if let Some(log_sink) = &log_sink {
                        log_sink(format!("[sim step={}] {} came online", step + 1, node_id));
                    }
                }
                for node_id in &offline {
                    if let Some(log_sink) = &log_sink {
                        log_sink(format!("[sim step={}] {} went offline", step + 1, node_id));
                    }
                }

                let mut forward_envelopes = Vec::new();
                for client in clients.values_mut() {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    forward_envelopes.extend(client.forward_store_forwards(
                        step,
                        &online_set,
                        &mut context,
                    ));
                }
                for envelope in forward_envelopes {
                    let _ = post_envelope(&relay_base_url, &envelope).await;
                }

                for node_id in &newly_online {
                    let envelopes = fetch_inbox(&relay_base_url, node_id)
                        .await
                        .unwrap_or_default();
                    if let Some(client) = clients.get_mut(node_id) {
                        let mut context = ClientContext {
                            direct_enabled,
                            ttl_seconds,
                            encryption: encryption.clone(),
                            direct_links,
                            neighbors,
                            keypairs,
                            message_history: &mut *message_history,
                            message_send_steps: &mut *message_send_steps,
                            pending_forwarded_messages: &mut *pending_forwarded_messages,
                            metrics: &mut *metrics,
                            metrics_tracker: &mut *metrics_tracker,
                        };
                        let newly_received = client.handle_inbox(
                            step,
                            envelopes,
                            &mut context,
                            Some(&mut rolling_latency),
                        );
                        received_messages = received_messages.saturating_add(newly_received);
                    }
                }

                let mut online_envelopes = Vec::new();
                for node_id in &newly_online {
                    if let Some(client) = clients.get_mut(node_id) {
                        let mut context = ClientContext {
                            direct_enabled,
                            ttl_seconds,
                            encryption: encryption.clone(),
                            direct_links,
                            neighbors,
                            keypairs,
                            message_history: &mut *message_history,
                            message_send_steps: &mut *message_send_steps,
                            pending_forwarded_messages: &mut *pending_forwarded_messages,
                            metrics: &mut *metrics,
                            metrics_tracker: &mut *metrics_tracker,
                        };
                        online_envelopes.extend(client.announce_online(step, &mut context));
                    }
                }
                for envelope in online_envelopes {
                    let _ = post_envelope(&relay_base_url, &envelope).await;
                }

                let mut broadcast_envelopes = Vec::new();
                for client in clients.values_mut() {
                    let mut context = ClientContext {
                        direct_enabled,
                        ttl_seconds,
                        encryption: encryption.clone(),
                        direct_links,
                        neighbors,
                        keypairs,
                        message_history: &mut *message_history,
                        message_send_steps: &mut *message_send_steps,
                        pending_forwarded_messages: &mut *pending_forwarded_messages,
                        metrics: &mut *metrics,
                        metrics_tracker: &mut *metrics_tracker,
                    };
                    let outcome =
                        client.process_pending_online_broadcasts(step, &online_set, &mut context);
                    received_messages = received_messages.saturating_add(outcome.delivered_missed);
                    broadcast_envelopes.extend(outcome.envelopes);
                }
                for envelope in broadcast_envelopes {
                    let _ = post_envelope(&relay_base_url, &envelope).await;
                }

                for node_id in &online_ids {
                    if newly_online.contains(node_id) {
                        continue;
                    }
                    let envelopes = fetch_inbox(&relay_base_url, node_id)
                        .await
                        .unwrap_or_default();
                    if let Some(client) = clients.get_mut(node_id) {
                        let mut context = ClientContext {
                            direct_enabled,
                            ttl_seconds,
                            encryption: encryption.clone(),
                            direct_links,
                            neighbors,
                            keypairs,
                            message_history: &mut *message_history,
                            message_send_steps: &mut *message_send_steps,
                            pending_forwarded_messages: &mut *pending_forwarded_messages,
                            metrics: &mut *metrics,
                            metrics_tracker: &mut *metrics_tracker,
                        };
                        let newly_received = client.handle_inbox(
                            step,
                            envelopes,
                            &mut context,
                            Some(&mut rolling_latency),
                        );
                        received_messages = received_messages.saturating_add(newly_received);
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

                (sent_messages, received_messages, online_ids.len())
            };

            let aggregate_metrics = self.aggregate_metrics();
            let update = SimulationStepUpdate {
                step: step + 1,
                total_steps: steps,
                online_nodes,
                total_peers: self.clients.len(),
                speed_factor: self.timing.speed_factor,
                sent_messages,
                received_messages,
                rolling_latency: rolling_latency.snapshot(),
                aggregate_metrics,
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
        self.clients
            .get(node_id)
            .map(|client| client.inbox())
            .unwrap_or_default()
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
pub struct MetricsTracker {
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

    pub fn record_send(&mut self, message: &SimMessage, step: usize, envelope: &Envelope) {
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

    pub fn record_delivery(
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

    pub fn record_online_broadcast(&mut self) {
        self.online_broadcasts_sent = self.online_broadcasts_sent.saturating_add(1);
    }

    pub fn record_ack(&mut self) {
        self.acks_received = self.acks_received.saturating_add(1);
    }

    pub fn record_missed_message_request(&mut self) {
        self.missed_message_requests_sent = self.missed_message_requests_sent.saturating_add(1);
    }

    pub fn record_missed_message_delivery(&mut self) {
        self.missed_message_deliveries = self.missed_message_deliveries.saturating_add(1);
    }

    pub fn record_store_forward_stored(&mut self) {
        self.store_forwards_stored = self.store_forwards_stored.saturating_add(1);
    }

    pub fn record_store_forward_forwarded(&mut self, storage_peer_id: &str) {
        self.store_forwards_forwarded = self.store_forwards_forwarded.saturating_add(1);
        *self
            .per_peer_store_forwarded
            .entry(storage_peer_id.to_string())
            .or_insert(0) += 1;
    }

    pub fn record_store_forward_delivery(&mut self) {
        self.store_forwards_delivered = self.store_forwards_delivered.saturating_add(1);
    }

    pub fn record_meta_send(&mut self, sender_id: String, envelope: &Envelope) {
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
    let (base_url, shutdown_tx, _relay_control) =
        start_relay(relay_config.clone().into_relay_config()).await;
    let inputs = build_simulation_inputs(&scenario.simulation);
    let steps = scenario.simulation.effective_steps();
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.clients,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        inputs.timing,
        inputs.cohort_online_rates,
        None,
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
    let (base_url, shutdown_tx, _relay_control) =
        start_relay(relay_config.clone().into_relay_config()).await;
    let inputs = build_simulation_inputs(&scenario.simulation);
    let steps = scenario.simulation.effective_steps();
    let mut harness = SimulationHarness::new(
        base_url,
        inputs.clients,
        inputs.direct_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        inputs.timing,
        inputs.cohort_online_rates,
        None,
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
pub struct RollingLatencyTracker {
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

    pub fn record(&mut self, latency: usize) {
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
    let mut clients = Vec::new();
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
        clients.push(SimulationHarness::build_client(node_id, schedule));
    }
    SimulationInputs {
        clients,
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

pub async fn start_relay(config: RelayConfig) -> (String, oneshot::Sender<()>, RelayControl) {
    let relay_control = RelayControl::new(config.pause_flag.clone());
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

    (format!("http://{}", addr), shutdown_tx, relay_control)
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
