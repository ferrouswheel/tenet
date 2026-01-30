use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Router;
use rand::{Rng, SeedableRng};
use tokio::sync::{mpsc, oneshot};

use crate::client::{Client, ClientContext, ClientLogEvent, ClientLogSink, ClientMetrics};
use crate::crypto::{generate_keypair, StoredKeypair};
use crate::protocol::{Envelope, MessageKind};
use crate::relay::{app, RelayConfig, RelayControl, RelayState};

pub mod config;
pub mod event;
pub mod metrics;
pub mod random;
pub mod scenario;

pub use crate::client::SimulationClient;
pub use config::{
    ClusteringConfig, FriendsPerNode, GroupMembershipsPerNode, GroupSizeDistribution, GroupsConfig,
    LatencyDistribution, MessageEncryption, MessageSizeDistribution, MessageType,
    MessageTypeWeights, NetworkConditions, OnlineAvailability, OnlineCohortDefinition,
    PostFrequency, ReactionConfig, RelayConfigToml, SimulatedTimeConfig, SimulationConfig,
    SimulationScenarioConfig, SimulationTimingConfig, TimeControlConfig, TimeDistribution,
};
pub use event::{
    Event, EventLog, EventOutcome, EventQueue, ProcessedEvent, ScheduledEvent, SimulationClock,
    TimeControlMode,
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
    build_friend_graph, build_online_events, build_online_schedules, build_planned_sends,
    build_simulation_inputs, generate_message_events, generate_online_schedule_events,
    run_event_based_scenario, run_event_based_scenario_with_tui, HistoricalMessage,
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
    SetTimeControlMode {
        mode: TimeControlMode,
    },
    JumpToTime {
        time: f64,
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
            SimulationControlCommand::SetTimeControlMode { .. } => {
                // Event-based time control, not applicable to step-based simulation
                self.log_action(
                    step,
                    "SetTimeControlMode ignored (event-based only)".to_string(),
                );
            }
            SimulationControlCommand::JumpToTime { .. } => {
                // Event-based time control, not applicable to step-based simulation
                self.log_action(step, "JumpToTime ignored (event-based only)".to_string());
            }
        }
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

/// Event-based simulation harness
#[allow(dead_code)]
pub struct EventBasedHarness {
    pub clock: SimulationClock,
    pub event_queue: EventQueue,
    pub event_log: EventLog,
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
    network_conditions: NetworkConditions,
    reaction_config: Option<ReactionConfig>,
    log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    client_log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
    relay_control: Option<RelayControl>,
    rng: rand::rngs::StdRng,
}

impl EventBasedHarness {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        clock: SimulationClock,
        relay_base_url: String,
        clients: Vec<SimulationClient>,
        direct_links: HashSet<(String, String)>,
        direct_enabled: bool,
        ttl_seconds: u64,
        encryption: MessageEncryption,
        keypairs: HashMap<String, StoredKeypair>,
        network_conditions: NetworkConditions,
        cohort_online_rates: HashMap<String, f64>,
        reaction_config: Option<ReactionConfig>,
        log_sink: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
        relay_control: Option<RelayControl>,
        seed: u64,
    ) -> Self {
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
                    "[sim t={:.2}] {} {}",
                    event.step as f64, event.client_id, event.message
                ));
            }) as std::sync::Arc<dyn ClientLogSink>
        });

        let planned_messages = 0;
        Self {
            clock,
            event_queue: EventQueue::new(),
            event_log: EventLog::new(),
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
            metrics_tracker: MetricsTracker::new(1.0, cohort_online_rates),
            network_conditions,
            reaction_config,
            log_sink,
            client_log_sink,
            relay_control,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
        }
    }

    fn log_event(&self, message: impl Into<String>) {
        if let Some(log_sink) = &self.log_sink {
            log_sink(format!(
                "[sim t={:.2}] {}",
                self.clock.simulated_time,
                message.into()
            ));
        }
    }

    pub fn schedule_event(&mut self, time: f64, event: Event) {
        self.event_queue.push(time, event);
    }

    async fn handle_message_send(
        &mut self,
        sender_id: String,
        message: SimMessage,
    ) -> EventOutcome {
        // Check if sender is online - for now we assume they are since the event was scheduled
        let online_set: HashSet<String> = self.clients.keys().cloned().collect();

        if !online_set.contains(&sender_id) {
            return EventOutcome::MessageSent {
                envelope_id: message.id.clone(),
                posted_to_relay: false,
            };
        }

        self.metrics.sent_messages = self.metrics.sent_messages.saturating_add(1);

        let mut context = ClientContext {
            direct_enabled: self.direct_enabled,
            ttl_seconds: self.ttl_seconds,
            encryption: self.encryption.clone(),
            direct_links: &self.direct_links,
            neighbors: &self.neighbors,
            keypairs: &self.keypairs,
            message_history: &mut self.message_history,
            message_send_steps: &mut self.message_send_steps,
            pending_forwarded_messages: &mut self.pending_forwarded_messages,
            metrics: &mut self.metrics,
            metrics_tracker: &mut self.metrics_tracker,
        };

        let step = self.clock.simulated_time as usize;

        if let Some(client) = self.clients.get_mut(&sender_id) {
            let outcome = client.enqueue_sends(
                step,
                std::slice::from_ref(&message),
                &online_set,
                &mut context,
            );

            // Post envelopes to relay
            for envelope in &outcome.envelopes {
                let _ = post_envelope(&self.relay_base_url, envelope).await;
            }

            // Schedule direct deliveries with latency
            for direct_message in outcome.direct_deliveries {
                let latency = self.network_conditions.direct_latency.sample(&mut self.rng);
                let deliver_time = self.clock.simulated_time + latency;

                // Build envelope for direct delivery
                if let Some(envelope) = outcome.envelopes.first() {
                    self.event_queue.push(
                        deliver_time,
                        Event::MessageDeliver {
                            recipient_id: direct_message.recipient.clone(),
                            envelope: envelope.clone(),
                            send_time: self.clock.simulated_time,
                        },
                    );
                }
            }

            if let Some(envelope) = outcome.envelopes.first() {
                return EventOutcome::MessageSent {
                    envelope_id: envelope.header.message_id.0.clone(),
                    posted_to_relay: !outcome.envelopes.is_empty(),
                };
            }
        }

        EventOutcome::MessageSent {
            envelope_id: message.id,
            posted_to_relay: false,
        }
    }

    async fn handle_message_deliver(
        &mut self,
        recipient_id: String,
        envelope: Envelope,
        _send_time: f64,
    ) -> EventOutcome {
        let online_set: HashSet<String> = self.clients.keys().cloned().collect();

        if !online_set.contains(&recipient_id) {
            return EventOutcome::MessageDelivered { accepted: false };
        }

        let mut context = ClientContext {
            direct_enabled: self.direct_enabled,
            ttl_seconds: self.ttl_seconds,
            encryption: self.encryption.clone(),
            direct_links: &self.direct_links,
            neighbors: &self.neighbors,
            keypairs: &self.keypairs,
            message_history: &mut self.message_history,
            message_send_steps: &mut self.message_send_steps,
            pending_forwarded_messages: &mut self.pending_forwarded_messages,
            metrics: &mut self.metrics,
            metrics_tracker: &mut self.metrics_tracker,
        };

        let step = self.clock.simulated_time as usize;
        let sender_id = envelope.header.sender_id.clone();

        if let Some(client) = self.clients.get_mut(&recipient_id) {
            // Create a SimMessage from envelope for direct delivery
            let sim_message = SimMessage {
                id: envelope.header.message_id.0.clone(),
                sender: envelope.header.sender_id.clone(),
                recipient: recipient_id.clone(),
                body: envelope.payload.body.clone(),
                payload: envelope.payload.clone(),
                message_kind: envelope.header.message_kind,
                group_id: envelope.header.group_id.clone(),
            };

            let result = client.handle_direct_delivery(step, sim_message, &mut context, None);

            // Check if we should generate a reaction (reply) event
            if result.is_some() {
                if let Some(reaction_config) = &self.reaction_config {
                    let should_reply = self.rng.gen::<f64>() < reaction_config.reply_probability;
                    if should_reply {
                        // Schedule a reply event after a delay
                        let delay = reaction_config
                            .reply_delay_distribution
                            .sample(&mut self.rng);
                        let reply_time = self.clock.simulated_time + delay;

                        // Note: In a full implementation, we would create a reply message here
                        // For now, we just log that a reaction would occur
                        self.log_event(format!(
                            "Reaction: {} will reply to {} after {:.2}s delay (at t={:.2})",
                            recipient_id, sender_id, delay, reply_time
                        ));

                        // TODO: Actually generate and schedule a reply message event
                        // This would require access to the message generation logic
                        // which is currently in scenario.rs
                    }
                }
            }

            EventOutcome::MessageDelivered {
                accepted: result.is_some(),
            }
        } else {
            EventOutcome::MessageDelivered { accepted: false }
        }
    }

    async fn handle_online_transition(
        &mut self,
        client_id: String,
        going_online: bool,
    ) -> EventOutcome {
        self.log_event(format!(
            "Client {} going {}",
            client_id,
            if going_online { "online" } else { "offline" }
        ));

        // Update client's online state
        if let Some(client) = self.clients.get_mut(&client_id) {
            if let Some(sim_client) = client.as_any_mut().downcast_mut::<SimulationClient>() {
                sim_client.set_online(going_online);
            }
        }

        if going_online {
            // When going online, schedule inbox poll
            let poll_time = self.clock.simulated_time + 0.1; // Poll shortly after coming online
            self.event_queue.push(
                poll_time,
                Event::InboxPoll {
                    client_id: client_id.clone(),
                },
            );

            // Schedule online announcement to peers
            let announce_time = self.clock.simulated_time + 0.05;
            self.event_queue.push(
                announce_time,
                Event::OnlineAnnounce {
                    client_id: client_id.clone(),
                },
            );

            // Trigger store-and-forward delivery
            self.trigger_store_and_forward(&client_id).await;
        }

        EventOutcome::OnlineTransitioned {
            new_state: going_online,
        }
    }

    async fn handle_inbox_poll(&mut self, client_id: String) -> EventOutcome {
        // Check if client is online
        let step = self.clock.simulated_time as usize;
        let is_online = self
            .clients
            .get(&client_id)
            .map(|c| c.is_online(step))
            .unwrap_or(false);

        if !is_online {
            return EventOutcome::InboxPolled {
                messages_fetched: 0,
            };
        }

        // Fetch messages from relay
        let envelopes = fetch_inbox(&self.relay_base_url, &client_id)
            .await
            .unwrap_or_default();
        let messages_fetched = envelopes.len();

        if messages_fetched > 0 {
            self.log_event(format!(
                "Client {} polled inbox: {} messages",
                client_id, messages_fetched
            ));
        }

        // Handle inbox messages
        if let Some(client) = self.clients.get_mut(&client_id) {
            let mut context = ClientContext {
                direct_enabled: self.direct_enabled,
                ttl_seconds: self.ttl_seconds,
                encryption: self.encryption.clone(),
                direct_links: &self.direct_links,
                neighbors: &self.neighbors,
                keypairs: &self.keypairs,
                message_history: &mut self.message_history,
                message_send_steps: &mut self.message_send_steps,
                pending_forwarded_messages: &mut self.pending_forwarded_messages,
                metrics: &mut self.metrics,
                metrics_tracker: &mut self.metrics_tracker,
            };

            client.handle_inbox(step, envelopes, &mut context, None);
        }

        // Schedule next poll if still online (default: 60 seconds)
        if is_online {
            let next_poll_time = self.clock.simulated_time + 60.0;
            self.event_queue.push(
                next_poll_time,
                Event::InboxPoll {
                    client_id: client_id.clone(),
                },
            );
        }

        EventOutcome::InboxPolled { messages_fetched }
    }

    async fn handle_online_announce(&mut self, client_id: String) -> EventOutcome {
        self.log_event(format!("Client {} announcing online status", client_id));

        // In the event-based simulation, we don't need to do anything specific here
        // as the online state is already updated. This event is mainly for logging
        // and potential future extensions.

        EventOutcome::OnlineAnnounced
    }

    async fn trigger_store_and_forward(&mut self, client_id: &str) {
        // Build online_set with only this client (the one that just came online)
        let mut online_set = HashSet::new();
        online_set.insert(client_id.to_string());

        let step = self.clock.simulated_time as usize;

        // Collect envelopes to forward from all clients
        let mut forward_envelopes = Vec::new();

        // Get all client IDs upfront to avoid borrow checker issues
        let client_ids: Vec<String> = self.clients.keys().cloned().collect();

        for peer_id in client_ids {
            if let Some(client) = self.clients.get_mut(&peer_id) {
                let mut context = ClientContext {
                    direct_enabled: self.direct_enabled,
                    ttl_seconds: self.ttl_seconds,
                    encryption: self.encryption.clone(),
                    direct_links: &self.direct_links,
                    neighbors: &self.neighbors,
                    keypairs: &self.keypairs,
                    message_history: &mut self.message_history,
                    message_send_steps: &mut self.message_send_steps,
                    pending_forwarded_messages: &mut self.pending_forwarded_messages,
                    metrics: &mut self.metrics,
                    metrics_tracker: &mut self.metrics_tracker,
                };

                // Trigger store-and-forward delivery
                forward_envelopes.extend(client.forward_store_forwards(
                    step,
                    &online_set,
                    &mut context,
                ));
            }
        }

        // Post forwarded messages to relay
        for envelope in forward_envelopes {
            let _ = post_envelope(&self.relay_base_url, &envelope).await;
        }
    }

    pub async fn run(&mut self, duration_seconds: f64) -> SimulationReport {
        self.log_event(format!(
            "Starting event-based simulation for {:.2} seconds",
            duration_seconds
        ));

        while let Some(next_time) = self.event_queue.peek_time() {
            // Check if we've exceeded simulation duration
            if next_time > duration_seconds {
                break;
            }

            // Update clock based on mode
            match self.clock.mode {
                TimeControlMode::FastForward => {
                    self.clock.jump_to(next_time);
                }
                TimeControlMode::RealTime { speed_factor } => {
                    // Wait for real time to catch up
                    while self.clock.simulated_time < next_time {
                        self.clock.update();
                        let wait_time = (next_time - self.clock.simulated_time) / speed_factor;
                        if wait_time > 0.001 {
                            tokio::time::sleep(Duration::from_secs_f64(wait_time.min(0.1))).await;
                        } else {
                            break;
                        }
                    }
                    self.clock.jump_to(next_time);
                }
                TimeControlMode::Paused => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }

            // Process all events at current time
            while let Some(scheduled) = self.event_queue.pop() {
                if scheduled.time > self.clock.simulated_time {
                    // Put it back and break
                    self.event_queue.push(scheduled.time, scheduled.event);
                    break;
                }

                let wall_time = std::time::Instant::now();
                let outcome = match scheduled.event {
                    Event::MessageSend { sender_id, message } => {
                        self.handle_message_send(sender_id, message).await
                    }
                    Event::MessageDeliver {
                        recipient_id,
                        envelope,
                        send_time,
                    } => {
                        self.handle_message_deliver(recipient_id, envelope, send_time)
                            .await
                    }
                    Event::OnlineTransition {
                        client_id,
                        going_online,
                    } => self.handle_online_transition(client_id, going_online).await,
                    Event::InboxPoll { client_id } => self.handle_inbox_poll(client_id).await,
                    Event::OnlineAnnounce { client_id } => {
                        self.handle_online_announce(client_id).await
                    }
                    Event::CustomAction { .. } => {
                        EventOutcome::CustomActionExecuted { success: true }
                    }
                };

                self.event_log
                    .record(scheduled.event_id, scheduled.time, wall_time, outcome);
            }
        }

        self.log_event("Event-based simulation completed");
        self.generate_report()
    }

    fn generate_report(&self) -> SimulationReport {
        let aggregate_metrics = self.aggregate_metrics();
        let metrics_report = self.metrics_tracker.report(aggregate_metrics);

        SimulationReport {
            metrics: self.metrics.clone(),
            report: metrics_report,
        }
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

    fn collect_client_metrics(&self) -> HashMap<String, ClientMetrics> {
        self.clients
            .iter()
            .map(|(id, client)| (id.clone(), client.metrics()))
            .collect()
    }

    /// Run event-based simulation with progress callbacks and control commands
    pub async fn run_with_progress_and_controls<F>(
        &mut self,
        duration_seconds: f64,
        mut control_rx: mpsc::UnboundedReceiver<SimulationControlCommand>,
        mut on_progress: F,
    ) -> SimulationReport
    where
        F: FnMut(SimulationStepUpdate),
    {
        self.log_event(format!(
            "Starting event-based simulation for {:.2} seconds",
            duration_seconds
        ));

        let mut sent_messages_this_interval = 0usize;
        let mut received_messages_this_interval = 0usize;
        let mut last_report_time = 0.0;
        let report_interval = 1.0; // Report every simulated second
        let mut rolling_latency = RollingLatencyTracker::new(100);
        let mut stop_requested = false;

        loop {
            // Process control commands
            while let Ok(command) = control_rx.try_recv() {
                match command {
                    SimulationControlCommand::SetTimeControlMode { mode } => {
                        self.clock.set_mode(mode);
                        self.log_event(format!("Time control mode changed to {:?}", mode));
                    }
                    SimulationControlCommand::JumpToTime { time } => {
                        self.clock.jump_to(time);
                        self.log_event(format!("Jumped to time {:.2}s", time));
                    }
                    SimulationControlCommand::SetPaused { paused } => {
                        let mode = if paused {
                            TimeControlMode::Paused
                        } else {
                            TimeControlMode::RealTime { speed_factor: 1.0 }
                        };
                        self.clock.set_mode(mode);
                        if let Some(relay_control) = &self.relay_control {
                            relay_control.set_paused(paused);
                        }
                        self.log_event(if paused {
                            "Simulation paused".to_string()
                        } else {
                            "Simulation resumed".to_string()
                        });
                    }
                    SimulationControlCommand::AdjustSpeedFactor { delta } => {
                        if let TimeControlMode::RealTime { speed_factor } = self.clock.mode {
                            let new_speed = (speed_factor + delta).max(0.1);
                            self.clock.set_mode(TimeControlMode::RealTime {
                                speed_factor: new_speed,
                            });
                            self.log_event(format!("Speed factor adjusted to {:.2}x", new_speed));
                        }
                    }
                    SimulationControlCommand::SetSpeedFactor { speed_factor } => {
                        self.clock
                            .set_mode(TimeControlMode::RealTime { speed_factor });
                        self.log_event(format!("Speed factor set to {:.2}x", speed_factor));
                    }
                    SimulationControlCommand::Stop => {
                        self.log_event("Stop requested".to_string());
                        stop_requested = true;
                    }
                    _ => {
                        // Other commands not relevant for event-based simulation
                    }
                }
            }

            // Check if stop was requested
            if stop_requested {
                break;
            }

            // Check if we've exceeded simulation duration
            let next_time = match self.event_queue.peek_time() {
                Some(time) if time <= duration_seconds => time,
                _ => break, // No more events or past duration
            };

            // Update clock based on mode
            match self.clock.mode {
                TimeControlMode::FastForward => {
                    self.clock.jump_to(next_time);
                }
                TimeControlMode::RealTime { speed_factor } => {
                    // Wait for real time to catch up
                    while self.clock.simulated_time < next_time {
                        self.clock.update();
                        let wait_time = (next_time - self.clock.simulated_time) / speed_factor;
                        if wait_time > 0.001 {
                            tokio::time::sleep(Duration::from_secs_f64(wait_time.min(0.1))).await;
                        } else {
                            break;
                        }
                    }
                    self.clock.jump_to(next_time);
                }
                TimeControlMode::Paused => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }

            // Process all events at current time
            while let Some(scheduled) = self.event_queue.pop() {
                if scheduled.time > self.clock.simulated_time {
                    // Put it back and break
                    self.event_queue.push(scheduled.time, scheduled.event);
                    break;
                }

                let wall_time = std::time::Instant::now();
                let event = scheduled.event.clone();
                let outcome = match event {
                    Event::MessageSend { sender_id, message } => {
                        sent_messages_this_interval += 1;
                        self.handle_message_send(sender_id, message).await
                    }
                    Event::MessageDeliver {
                        recipient_id,
                        envelope,
                        send_time,
                    } => {
                        received_messages_this_interval += 1;
                        let latency_steps =
                            ((self.clock.simulated_time - send_time) / 1.0) as usize;
                        rolling_latency.record(latency_steps);
                        self.handle_message_deliver(recipient_id, envelope, send_time)
                            .await
                    }
                    Event::OnlineTransition {
                        client_id,
                        going_online,
                    } => self.handle_online_transition(client_id, going_online).await,
                    Event::InboxPoll { client_id } => self.handle_inbox_poll(client_id).await,
                    Event::OnlineAnnounce { client_id } => {
                        self.handle_online_announce(client_id).await
                    }
                    Event::CustomAction { .. } => {
                        EventOutcome::CustomActionExecuted { success: true }
                    }
                };

                self.event_log
                    .record(scheduled.event_id, scheduled.time, wall_time, outcome);
            }

            // Send progress updates at regular intervals
            if self.clock.simulated_time - last_report_time >= report_interval {
                let online_nodes = self
                    .clients
                    .values()
                    .filter(|c| {
                        let step = self.clock.simulated_time as usize;
                        c.is_online(step)
                    })
                    .count();

                let speed_factor = match self.clock.mode {
                    TimeControlMode::FastForward => f64::INFINITY,
                    TimeControlMode::RealTime { speed_factor } => speed_factor,
                    TimeControlMode::Paused => 0.0,
                };

                let aggregate_metrics = self.aggregate_metrics();
                let update = SimulationStepUpdate {
                    step: self.clock.simulated_time as usize,
                    total_steps: duration_seconds as usize,
                    online_nodes,
                    total_peers: self.clients.len(),
                    speed_factor,
                    sent_messages: sent_messages_this_interval,
                    received_messages: received_messages_this_interval,
                    rolling_latency: rolling_latency.snapshot(),
                    aggregate_metrics,
                };
                on_progress(update);

                sent_messages_this_interval = 0;
                received_messages_this_interval = 0;
                last_report_time = self.clock.simulated_time;
            }
        }

        self.log_event("Event-based simulation completed");
        self.generate_report()
    }
}

/// Helper function to convert a list of planned sends to MessageSend events
pub fn planned_sends_to_events(
    plan: Vec<PlannedSend>,
    simulated_time_config: &SimulatedTimeConfig,
) -> Vec<ScheduledEvent> {
    let seconds_per_step = simulated_time_config.seconds_per_step as f64;

    plan.into_iter()
        .map(|planned_send| {
            let time = planned_send.step as f64 * seconds_per_step;
            ScheduledEvent {
                time,
                event: Event::MessageSend {
                    sender_id: planned_send.message.sender.clone(),
                    message: planned_send.message,
                },
                event_id: 0, // Will be assigned by EventQueue
            }
        })
        .collect()
}
