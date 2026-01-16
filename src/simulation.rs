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
    build_encrypted_payload, build_envelope_from_payload, build_plaintext_payload,
    decrypt_encrypted_payload, ContentId, Envelope, Payload,
};
use crate::relay::{app, RelayConfig, RelayState};

const SIMULATION_PAYLOAD_AAD: &[u8] = b"tenet-simulation";
const SIMULATION_HPKE_INFO: &[u8] = b"tenet-simulation-hpke";

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

#[derive(Debug)]
pub struct SimulationInputs {
    pub nodes: Vec<Node>,
    pub planned_sends: Vec<PlannedSend>,
    pub direct_links: HashSet<(String, String)>,
    pub keypairs: HashMap<String, StoredKeypair>,
    pub encryption: MessageEncryption,
}

pub struct SimulationHarness {
    relay_base_url: String,
    nodes: HashMap<String, Node>,
    direct_links: HashSet<(String, String)>,
    direct_enabled: bool,
    ttl_seconds: u64,
    encryption: MessageEncryption,
    keypairs: HashMap<String, StoredKeypair>,
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
        Self {
            relay_base_url,
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            direct_links,
            direct_enabled,
            ttl_seconds,
            encryption,
            keypairs,
            metrics: SimulationMetrics {
                planned_messages,
                sent_messages: 0,
                direct_deliveries: 0,
                inbox_deliveries: 0,
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

    pub async fn run(&mut self, steps: usize, plan: Vec<PlannedSend>) -> SimulationMetrics {
        self.metrics.planned_messages = plan.len();
        let mut sends_by_step: HashMap<usize, Vec<SimMessage>> = HashMap::new();
        for item in plan {
            sends_by_step
                .entry(item.step)
                .or_default()
                .push(item.message);
        }

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

            for node_id in online_ids {
                let envelopes = fetch_inbox(&self.relay_base_url, &node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    let message_id = envelope.header.message_id.0.clone();
                    let message = self.decode_envelope(&envelope, &message_id, &node_id);
                    if let Some(message) = message {
                        if let Some(node) = self.nodes.get_mut(&node_id) {
                            if node.receive(message) {
                                self.metrics.inbox_deliveries += 1;
                                self.metrics_tracker
                                    .record_delivery(&message_id, &node_id, step);
                            }
                        }
                    }
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

        for step in 0..steps {
            let mut sent_messages = 0;
            let mut received_messages = 0;
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

            for node_id in &online_ids {
                let envelopes = fetch_inbox(&self.relay_base_url, node_id)
                    .await
                    .unwrap_or_default();
                for envelope in envelopes {
                    let message_id = envelope.header.message_id.0.clone();
                    let message = self.decode_envelope(&envelope, &message_id, node_id);
                    if let Some(message) = message {
                        if let Some(node) = self.nodes.get_mut(node_id) {
                            if node.receive(message) {
                                self.metrics.inbox_deliveries += 1;
                                if let Some(latency) =
                                    self.metrics_tracker
                                        .record_delivery(&message_id, node_id, step)
                                {
                                    rolling_latency.record(latency);
                                }
                                received_messages += 1;
                            }
                        }
                    }
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
            timestamp,
            self.ttl_seconds,
            message.payload.clone(),
        )
        .ok()?;
        let mut direct_latency = None;
        if self.direct_enabled
            && self
                .direct_links
                .contains(&(message.sender.clone(), message.recipient.clone()))
        {
            let recipient_online = self
                .nodes
                .get(&message.recipient)
                .is_some_and(|node| node.online_at(step));
            if recipient_online {
                let decoded = self.decode_envelope(&envelope, &message.id, &message.recipient);
                if let Some(decoded) = decoded {
                    if let Some(node) = self.nodes.get_mut(&message.recipient) {
                        if node.receive(decoded) {
                            direct_latency = self.metrics_tracker.record_delivery(
                                &message.id,
                                &message.recipient,
                                step,
                            );
                        }
                    }
                }
            }
        }

        let _ = post_envelope(&self.relay_base_url, &envelope).await;

        direct_latency
    }

    fn decode_envelope(
        &self,
        envelope: &Envelope,
        message_id: &str,
        recipient_id: &str,
    ) -> Option<SimMessage> {
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

    fn report(&self) -> SimulationMetricsReport {
        SimulationMetricsReport {
            per_sender_recipient_counts: self.per_sender_recipient_counts.clone(),
            first_delivery_latency: self.first_delivery_latency.report(),
            all_recipients_delivery_latency: self.all_recipients_delivery_latency.report(),
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
    if matches!(encryption, MessageEncryption::Encrypted) {
        for node_id in &config.node_ids {
            keypairs.insert(node_id.clone(), generate_keypair_with_rng(&mut crypto_rng));
        }
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
