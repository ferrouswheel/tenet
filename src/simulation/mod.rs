use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Router;
use tokio::sync::{mpsc, oneshot};

use crate::client::{Client, ClientContext, ClientLogEvent, ClientLogSink, ClientMetrics};
use crate::crypto::{generate_keypair, StoredKeypair};
use crate::protocol::{Envelope, MessageKind};
use crate::relay::{app, RelayConfig, RelayControl, RelayState};

pub mod config;
pub mod metrics;
pub mod random;
pub mod scenario;

pub use crate::client::SimulationClient;
pub use config::{
    ClusteringConfig, FriendsPerNode, MessageEncryption, MessageSizeDistribution,
    OnlineAvailability, OnlineCohortDefinition, PostFrequency, RelayConfigToml,
    SimulatedTimeConfig, SimulationConfig, SimulationScenarioConfig, SimulationTimingConfig,
};
pub use metrics::{
    CountSummary, LatencyHistogram, MetricsTracker, RollingLatencySnapshot, RollingLatencyTracker,
    SenderRecipient, SenderRecipientCount, SimulationAggregateMetrics, SimulationMetrics,
    SimulationMetricsReport, SimulationReport, SimulationStepUpdate, SizeSummary,
};
pub use random::{
    generate_message_body, sample_message_size, sample_poisson, sample_weighted_index, sample_zipf,
};
pub use scenario::{
    build_friend_graph, build_online_schedules, build_planned_sends, build_simulation_inputs,
    run_simulation_scenario, run_simulation_scenario_with_progress, HistoricalMessage,
    OnlineSchedulePlan, PlannedSend, SimMessage, SimulationInputs,
};

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
