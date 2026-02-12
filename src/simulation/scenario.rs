use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rand::distributions::Uniform;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};
use serde::{Deserialize, Serialize};

use crate::crypto::{generate_keypair_with_rng, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::groups::GroupInfo;
use crate::message_handler::StorageMessageHandler;
use crate::protocol::{
    build_encrypted_payload, build_plaintext_payload, ContentId, Envelope, MessageKind, Payload,
};
use crate::storage::Storage;

use super::random::{generate_message_body, sample_poisson, sample_weighted_index, sample_zipf};
use super::{
    start_relay, ClusteringConfig, FriendRequestConfig, FriendsPerNode, GroupMembershipsPerNode,
    GroupSizeDistribution, LatencyDistribution, MessageEncryption, MessageType, MessageTypeWeights,
    OnlineAvailability, OnlineCohortDefinition, PostFrequency, ScheduledEvent, SimulationClient,
    SimulationConfig, SimulationHarness, SimulationReport, SimulationScenarioConfig,
    SimulationStepUpdate, SimulationTimingConfig, TimeDistribution, SIMULATION_HOURS_PER_DAY,
    SIMULATION_HPKE_INFO, SIMULATION_PAYLOAD_AAD,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimMessage {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub payload: Payload,
    #[serde(default = "default_message_kind")]
    pub message_kind: MessageKind,
    #[serde(default)]
    pub group_id: Option<String>,
}

fn default_message_kind() -> MessageKind {
    MessageKind::Direct
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
    pub groups: HashMap<String, GroupInfo>,
    pub node_groups: HashMap<String, Vec<String>>,
    pub message_size_distribution: super::MessageSizeDistribution,
}

/// Mapping of nodes to their assigned cohorts (for message type weights)
#[derive(Debug, Clone)]
pub struct NodeCohortAssignment {
    pub node_id: String,
    pub cohort_name: String,
    pub message_type_weights: MessageTypeWeights,
}

pub struct OnlineSchedulePlan {
    pub schedules: HashMap<String, Vec<bool>>,
    pub cohort_online_rates: HashMap<String, f64>,
    pub node_cohort_assignments: HashMap<String, NodeCohortAssignment>,
}

/// Run event-based simulation scenario
pub async fn run_event_based_scenario(
    scenario: SimulationScenarioConfig,
) -> Result<SimulationReport, String> {
    use super::event::{SimulationClock, TimeControlMode};
    use super::EventBasedHarness;
    use crate::simulation::planned_sends_to_events;

    let relay_config = scenario.relay.clone();
    let (base_url, shutdown_tx, _relay_control) =
        start_relay(relay_config.clone().into_relay_config()).await;

    let inputs = build_simulation_inputs(&scenario.simulation);
    let duration_seconds = scenario.simulation.duration_seconds.unwrap_or_else(|| {
        let steps = scenario.simulation.effective_steps();
        let seconds_per_step = scenario.simulation.simulated_time.seconds_per_step;
        (steps as u64 * seconds_per_step) as u64
    }) as f64;

    let time_control_mode = TimeControlMode::FastForward;
    let clock = SimulationClock::new(time_control_mode);

    let network_conditions = scenario
        .network_conditions
        .clone()
        .unwrap_or_default();

    let mut rng = rand::rngs::StdRng::seed_from_u64(scenario.simulation.seed);
    let (active_links, pending_friendships, friend_accept_delay) =
        split_friend_graph(inputs.direct_links, &scenario.simulation.friend_request_config, &mut rng);

    let mut harness = EventBasedHarness::new(
        clock,
        base_url,
        inputs.clients,
        active_links,
        scenario.direct_enabled.unwrap_or(true),
        relay_config.ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        network_conditions,
        inputs.cohort_online_rates,
        scenario.simulation.reaction_config.clone(),
        inputs.message_size_distribution,
        pending_friendships.clone(),
        friend_accept_delay,
        None,
        None,
        None,
        scenario.simulation.seed,
    );

    // Convert planned sends to events and schedule them
    let events = planned_sends_to_events(inputs.planned_sends, &scenario.simulation.simulated_time);
    for event in events {
        harness.schedule_event(event.time, event.event);
    }

    // Schedule online/offline events
    let (online_events, _node_cohorts) =
        build_online_events(&scenario.simulation, 0.0, duration_seconds, &mut rng);
    for event in online_events {
        harness.schedule_event(event.time, event.event);
    }

    // Schedule friend request events for pending friendships
    if let Some(fr_config) = &scenario.simulation.friend_request_config {
        let fr_events = build_friend_request_events(&pending_friendships, fr_config, 0.0, duration_seconds, &mut rng);
        for event in fr_events {
            harness.schedule_event(event.time, event.event);
        }
    }

    let report = harness.run(duration_seconds).await;
    shutdown_tx.send(()).ok();
    Ok(report)
}

/// Run event-based simulation scenario with TUI progress updates
pub async fn run_event_based_scenario_with_tui<F>(
    scenario: SimulationScenarioConfig,
    control_rx: tokio::sync::mpsc::UnboundedReceiver<super::SimulationControlCommand>,
    relay_config_override: crate::relay::RelayConfig,
    log_sink: Option<std::sync::Arc<dyn Fn(super::LogEntry) + Send + Sync>>,
    relay_time_sync: Option<std::sync::Arc<std::sync::Mutex<f64>>>,
    mut on_progress: F,
) -> Result<SimulationReport, String>
where
    F: FnMut(SimulationStepUpdate),
{
    use super::event::{SimulationClock, TimeControlMode};
    use super::EventBasedHarness;
    use crate::simulation::planned_sends_to_events;

    let (base_url, shutdown_tx, relay_control) = start_relay(relay_config_override).await;

    let inputs = build_simulation_inputs(&scenario.simulation);
    let duration_seconds = scenario.simulation.duration_seconds.unwrap_or_else(|| {
        let steps = scenario.simulation.effective_steps();
        let seconds_per_step = scenario.simulation.simulated_time.seconds_per_step;
        (steps as u64 * seconds_per_step) as u64
    }) as f64;

    // Default to fast-forward mode for TUI (users can adjust speed with controls)
    let time_control_mode = TimeControlMode::FastForward;
    let clock = SimulationClock::new(time_control_mode);

    let network_conditions = scenario
        .network_conditions
        .clone()
        .unwrap_or_default();

    let ttl_seconds = scenario.relay.ttl_seconds;

    let mut rng = rand::rngs::StdRng::seed_from_u64(scenario.simulation.seed);
    let (active_links, pending_friendships, friend_accept_delay) =
        split_friend_graph(inputs.direct_links, &scenario.simulation.friend_request_config, &mut rng);

    let mut harness = EventBasedHarness::new(
        clock,
        base_url,
        inputs.clients,
        active_links,
        scenario.direct_enabled.unwrap_or(true),
        ttl_seconds,
        inputs.encryption,
        inputs.keypairs,
        network_conditions,
        inputs.cohort_online_rates,
        scenario.simulation.reaction_config.clone(),
        inputs.message_size_distribution,
        pending_friendships.clone(),
        friend_accept_delay,
        log_sink,
        Some(relay_control),
        relay_time_sync,
        scenario.simulation.seed,
    );

    // Convert planned sends to events and schedule them
    let events = planned_sends_to_events(inputs.planned_sends, &scenario.simulation.simulated_time);
    harness.log_event(format!(
        "Scheduling {} message events across {:.0}s duration",
        events.len(),
        duration_seconds
    ));
    // Log time range of events
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        harness.log_event(format!(
            "Message events from {:.0}s to {:.0}s",
            first.time, last.time
        ));
    }
    for event in events {
        harness.schedule_event(event.time, event.event);
    }

    // Schedule online/offline events (reuse rng already seeded above)
    let (online_events, _node_cohorts) =
        build_online_events(&scenario.simulation, 0.0, duration_seconds, &mut rng);
    harness.log_event(format!(
        "Scheduling {} online/offline events",
        online_events.len()
    ));
    for event in online_events {
        harness.schedule_event(event.time, event.event);
    }

    // Schedule friend request events for pending friendships
    if let Some(fr_config) = &scenario.simulation.friend_request_config {
        let fr_events = build_friend_request_events(&pending_friendships, fr_config, 0.0, duration_seconds, &mut rng);
        harness.log_event(format!("Scheduling {} friend-request events", fr_events.len()));
        for event in fr_events {
            harness.schedule_event(event.time, event.event);
        }
    }

    let report = harness
        .run_with_progress_and_controls(duration_seconds, control_rx, |update| {
            on_progress(update);
        })
        .await;

    shutdown_tx.send(()).ok();
    Ok(report)
}

pub fn build_simulation_inputs(config: &SimulationConfig) -> SimulationInputs {
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let encryption = config
        .encryption
        .clone()
        .unwrap_or(MessageEncryption::Encrypted);
    let mut crypto_rng = ChaCha20Rng::seed_from_u64(config.seed ^ 0x5A17_5EED);
    let graph = build_friend_graph(config, &mut rng);
    let schedule_plan = build_online_schedules(config, &mut rng);
    let mut keypairs = HashMap::new();
    for node_id in &config.node_ids {
        keypairs.insert(node_id.clone(), generate_keypair_with_rng(&mut crypto_rng));
    }

    // Build groups if configured
    let (groups, node_groups) = build_groups(config, &mut rng);

    let planned_sends = build_planned_sends(
        config,
        &mut rng,
        &graph,
        &encryption,
        &keypairs,
        &schedule_plan.node_cohort_assignments,
        &node_groups,
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
        let mut client = SimulationHarness::build_client(node_id, schedule);

        // Attach a per-peer in-memory StorageMessageHandler so received
        // messages are persisted in an in-memory SQLite database.
        if let Some(keypair) = keypairs.get(node_id) {
            if let Ok(storage) = Storage::open_in_memory(std::path::Path::new("/tmp")) {
                let handler = StorageMessageHandler::new(storage, keypair.clone());
                client.set_handler(Box::new(handler));
            }
        }

        clients.push(client);
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
        groups,
        node_groups,
        message_size_distribution: config.message_size_distribution.clone(),
    }
}

/// Build groups and assign nodes to them based on configuration
fn build_groups(
    config: &SimulationConfig,
    rng: &mut impl Rng,
) -> (HashMap<String, GroupInfo>, HashMap<String, Vec<String>>) {
    let mut groups = HashMap::new();
    let mut node_groups: HashMap<String, Vec<String>> = HashMap::new();

    // Initialize empty group lists for all nodes
    for node_id in &config.node_ids {
        node_groups.insert(node_id.clone(), Vec::new());
    }

    let Some(groups_config) = &config.groups else {
        return (groups, node_groups);
    };

    let node_count = config.node_ids.len();
    if node_count == 0 || groups_config.count == 0 {
        return (groups, node_groups);
    }

    // Create the specified number of groups
    for i in 0..groups_config.count {
        let group_id = format!("group-{}", i + 1);

        // Determine group size based on distribution
        let target_size = match &groups_config.size_distribution {
            GroupSizeDistribution::Uniform { min, max } => {
                rng.gen_range(*min..=(*max).min(node_count))
            }
            GroupSizeDistribution::FractionOfNodes {
                min_fraction,
                max_fraction,
            } => {
                let min_size = (node_count as f64 * min_fraction.clamp(0.0, 1.0)).ceil() as usize;
                let max_size = (node_count as f64 * max_fraction.clamp(0.0, 1.0)).ceil() as usize;
                rng.gen_range(min_size.max(1)..=max_size.max(min_size).min(node_count))
            }
        };

        // Select random members for this group
        let mut available_nodes: Vec<&String> = config.node_ids.iter().collect();
        let mut members = Vec::new();
        for _ in 0..target_size.min(available_nodes.len()) {
            let idx = rng.gen_range(0..available_nodes.len());
            members.push(available_nodes.remove(idx).clone());
        }

        // Create the group (first member is creator for simplicity)
        let creator = members.first().cloned().unwrap_or_default();
        let group_info = GroupInfo::new(group_id.clone(), members.clone(), creator);

        // Update node_groups mapping
        for member in &members {
            if let Some(groups_list) = node_groups.get_mut(member) {
                groups_list.push(group_id.clone());
            }
        }

        groups.insert(group_id, group_info);
    }

    // Ensure each node is in the required number of groups based on memberships_per_node
    match &groups_config.memberships_per_node {
        GroupMembershipsPerNode::Fixed { count } => {
            ensure_minimum_memberships(config, &mut groups, &mut node_groups, *count, rng);
        }
        GroupMembershipsPerNode::Uniform { min, max } => {
            // For each node, ensure they're in at least min groups
            ensure_minimum_memberships(config, &mut groups, &mut node_groups, *min, rng);
            // Add additional random memberships up to max
            add_random_memberships(config, &mut groups, &mut node_groups, *min, *max, rng);
        }
    }

    (groups, node_groups)
}

fn ensure_minimum_memberships(
    config: &SimulationConfig,
    groups: &mut HashMap<String, GroupInfo>,
    node_groups: &mut HashMap<String, Vec<String>>,
    min_count: usize,
    rng: &mut impl Rng,
) {
    if groups.is_empty() || min_count == 0 {
        return;
    }

    let group_ids: Vec<String> = groups.keys().cloned().collect();

    for node_id in &config.node_ids {
        let current_count = node_groups.get(node_id).map(|g| g.len()).unwrap_or(0);
        if current_count >= min_count {
            continue;
        }

        // Add node to random groups until they reach min_count
        let needed = min_count - current_count;
        let current_groups: HashSet<String> = node_groups
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let mut available_groups: Vec<&String> = group_ids
            .iter()
            .filter(|g| !current_groups.contains(*g))
            .collect();

        for _ in 0..needed.min(available_groups.len()) {
            let idx = rng.gen_range(0..available_groups.len());
            let group_id = available_groups.remove(idx).clone();

            // Add node to this group
            if let Some(group) = groups.get_mut(&group_id) {
                group.members.insert(node_id.clone());
            }
            if let Some(groups_list) = node_groups.get_mut(node_id) {
                groups_list.push(group_id);
            }
        }
    }
}

fn add_random_memberships(
    config: &SimulationConfig,
    groups: &mut HashMap<String, GroupInfo>,
    node_groups: &mut HashMap<String, Vec<String>>,
    min: usize,
    max: usize,
    rng: &mut impl Rng,
) {
    if groups.is_empty() || max <= min {
        return;
    }

    let group_ids: Vec<String> = groups.keys().cloned().collect();

    for node_id in &config.node_ids {
        let current_count = node_groups.get(node_id).map(|g| g.len()).unwrap_or(0);
        let target = rng.gen_range(min..=max);
        if current_count >= target {
            continue;
        }

        let needed = target - current_count;
        let current_groups: HashSet<String> = node_groups
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let mut available_groups: Vec<&String> = group_ids
            .iter()
            .filter(|g| !current_groups.contains(*g))
            .collect();

        for _ in 0..needed.min(available_groups.len()) {
            let idx = rng.gen_range(0..available_groups.len());
            let group_id = available_groups.remove(idx).clone();

            if let Some(group) = groups.get_mut(&group_id) {
                group.members.insert(node_id.clone());
            }
            if let Some(groups_list) = node_groups.get_mut(node_id) {
                groups_list.push(group_id);
            }
        }
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

/// Build online events for all clients in a scenario
pub fn build_online_events(
    config: &SimulationConfig,
    start_time: f64,
    end_time: f64,
    rng: &mut impl Rng,
) -> (
    Vec<super::ScheduledEvent>,
    HashMap<String, NodeCohortAssignment>,
) {
    use super::event::{Event, ScheduledEvent};

    let mut all_events = Vec::new();
    let mut node_cohort_assignments: HashMap<String, NodeCohortAssignment> = HashMap::new();

    let cohorts = if config.cohorts.is_empty() {
        None
    } else {
        Some(config.cohorts.as_slice())
    };

    // Default message type weights from config
    let default_weights = config
        .message_type_weights
        .clone()
        .unwrap_or_else(MessageTypeWeights::default);

    for node_id in &config.node_ids {
        let (cohort_name_str, events, message_weights) = if let Some(cohorts) = cohorts {
            let cohort = select_cohort(cohorts, rng);
            let name = cohort_name(cohort).to_string();
            let events =
                generate_online_schedule_events(node_id, cohort, start_time, end_time, rng);

            // Use cohort-specific weights if defined, otherwise use config default
            let weights = cohort.message_type_weights();
            let weights = if weights.direct == 1.0 && weights.public == 0.0 && weights.group == 0.0
            {
                default_weights.clone()
            } else {
                weights
            };
            (name, events, weights)
        } else {
            // Legacy mode: always online
            let name = "legacy".to_string();
            let events = vec![ScheduledEvent {
                time: start_time,
                event: Event::OnlineTransition {
                    client_id: node_id.clone(),
                    going_online: true,
                },
                event_id: 0,
            }];
            (name, events, default_weights.clone())
        };

        all_events.extend(events);

        node_cohort_assignments.insert(
            node_id.clone(),
            NodeCohortAssignment {
                node_id: node_id.clone(),
                cohort_name: cohort_name_str,
                message_type_weights: message_weights,
            },
        );
    }

    (all_events, node_cohort_assignments)
}

/// Generate online/offline transition events for a client based on their cohort
pub fn generate_online_schedule_events(
    client_id: &str,
    cohort: &OnlineCohortDefinition,
    start_time: f64,
    end_time: f64,
    rng: &mut impl Rng,
) -> Vec<super::ScheduledEvent> {
    use super::event::{Event, ScheduledEvent};

    let mut events = Vec::new();

    match cohort {
        OnlineCohortDefinition::AlwaysOnline { .. } => {
            // Single online event at the start
            events.push(ScheduledEvent {
                time: start_time,
                event: Event::OnlineTransition {
                    client_id: client_id.to_string(),
                    going_online: true,
                },
                event_id: 0, // Will be assigned by EventQueue
            });
        }
        OnlineCohortDefinition::RarelyOnline {
            online_probability, ..
        } => {
            // Generate occasional online sessions
            // Sample session start times using Poisson process
            // Average 2 sessions per day, each lasting 5-15 minutes
            let duration_hours = (end_time - start_time) / 3600.0;
            let sessions_per_hour = 2.0 / 24.0; // 2 sessions per day
            let expected_sessions = (duration_hours * sessions_per_hour).max(1.0);
            let session_count = sample_poisson(rng, expected_sessions);

            for _ in 0..session_count {
                // Random session start time
                let session_start = start_time + rng.gen::<f64>() * (end_time - start_time);

                // Session duration: 5-15 minutes, scaled by online_probability
                let base_duration = 300.0 + rng.gen::<f64>() * 600.0; // 300-900 seconds
                let duration = base_duration * online_probability.clamp(0.1, 1.0);
                let session_end = (session_start + duration).min(end_time);

                // Online event
                events.push(ScheduledEvent {
                    time: session_start,
                    event: Event::OnlineTransition {
                        client_id: client_id.to_string(),
                        going_online: true,
                    },
                    event_id: 0,
                });

                // Offline event
                if session_end < end_time {
                    events.push(ScheduledEvent {
                        time: session_end,
                        event: Event::OnlineTransition {
                            client_id: client_id.to_string(),
                            going_online: false,
                        },
                        event_id: 0,
                    });
                }
            }
        }
        OnlineCohortDefinition::Diurnal {
            online_probability,
            timezone_offset_hours,
            hourly_weights,
            ..
        } => {
            // Sample online/offline transitions based on time-of-day weights
            let weights = normalize_hourly_weights(hourly_weights);
            let max_weight = weights
                .iter()
                .copied()
                .fold(0.0_f64, |max, weight| max.max(weight));
            let max_weight = if max_weight > 0.0 { max_weight } else { 1.0 };
            let base_probability = online_probability.clamp(0.0, 1.0);

            // Sample every 15 minutes (900 seconds) to create smooth transitions
            let sample_interval = 900.0; // 15 minutes
            let mut current_time = start_time;
            let mut is_online = false;

            while current_time < end_time {
                // Calculate hour of day with timezone offset
                let hour = ((current_time / 3600.0).floor() as i64 + *timezone_offset_hours as i64)
                    .rem_euclid(SIMULATION_HOURS_PER_DAY as i64)
                    as usize;

                // Calculate probability for this hour
                let weight_factor = weights[hour] / max_weight;
                let probability = (base_probability * weight_factor).clamp(0.0, 1.0);

                // Determine if should be online at this time
                let should_be_online = rng.gen::<f64>() < probability;

                // Generate transition event if state changed
                if should_be_online != is_online {
                    events.push(ScheduledEvent {
                        time: current_time,
                        event: Event::OnlineTransition {
                            client_id: client_id.to_string(),
                            going_online: should_be_online,
                        },
                        event_id: 0,
                    });
                    is_online = should_be_online;
                }

                current_time += sample_interval;
            }

            // Ensure we end offline if we're online at the end
            if is_online && end_time > start_time {
                events.push(ScheduledEvent {
                    time: end_time - 1.0,
                    event: Event::OnlineTransition {
                        client_id: client_id.to_string(),
                        going_online: false,
                    },
                    event_id: 0,
                });
            }
        }
    }

    // Sort events by time to ensure chronological order
    events.sort_by(|a, b| {
        a.time
            .partial_cmp(&b.time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    events
}

/// Generate message send events over a time window using time-based distributions
/// This is an alternative to step-based planned sends for event-based simulations
pub fn generate_message_events(
    sender_id: &str,
    friends: &[String],
    groups: &[String],
    start_time: f64,
    end_time: f64,
    lambda_per_hour: f64,
    time_distribution: &TimeDistribution,
    message_type_weights: &MessageTypeWeights,
    config: &SimulationConfig,
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    crypto_rng: &mut Option<&mut dyn RngCore>,
    counter: &mut usize,
    rng: &mut impl Rng,
) -> Vec<super::ScheduledEvent> {
    use super::event::{Event, ScheduledEvent};

    let duration_hours = (end_time - start_time) / 3600.0;
    let expected_count = lambda_per_hour * duration_hours;

    // Sample total count from Poisson distribution
    let count = sample_poisson(rng, expected_count);

    // Generate event times based on distribution
    let event_times = match time_distribution {
        TimeDistribution::Uniform => {
            // Uniform distribution of event times within window
            (0..count)
                .map(|_| start_time + rng.gen::<f64>() * (end_time - start_time))
                .collect::<Vec<f64>>()
        }
        TimeDistribution::Clustered {
            cluster_count,
            cluster_spread,
        } => {
            // Create cluster centers uniformly distributed
            let cluster_centers: Vec<f64> = (0..*cluster_count)
                .map(|_| start_time + rng.gen::<f64>() * (end_time - start_time))
                .collect();

            // Distribute events among clusters
            (0..count)
                .map(|_| {
                    // Pick a random cluster
                    let cluster_idx = rng.gen_range(0..cluster_centers.len());
                    let center = cluster_centers[cluster_idx];

                    // Sample from normal distribution around cluster center
                    let offset = rng.gen::<f64>() * cluster_spread - cluster_spread / 2.0;
                    (center + offset).clamp(start_time, end_time)
                })
                .collect()
        }
        TimeDistribution::Bursty {
            burst_count,
            burst_duration,
        } => {
            // Create burst start times uniformly distributed
            let burst_starts: Vec<f64> = (0..*burst_count)
                .map(|_| start_time + rng.gen::<f64>() * (end_time - start_time - burst_duration))
                .collect();

            // Distribute events among bursts
            (0..count)
                .map(|_| {
                    // Pick a random burst
                    let burst_idx = rng.gen_range(0..burst_starts.len());
                    let burst_start = burst_starts[burst_idx];

                    // Sample uniformly within burst duration
                    burst_start + rng.gen::<f64>() * burst_duration
                })
                .collect()
        }
    };

    // Create events for each time
    event_times
        .into_iter()
        .filter_map(|time| {
            // Build a message for this event
            let message = build_planned_message(
                sender_id,
                friends,
                groups,
                message_type_weights,
                config,
                encryption,
                keypairs,
                crypto_rng,
                counter,
                rng,
            )?;

            Some(ScheduledEvent {
                time,
                event: Event::MessageSend {
                    sender_id: sender_id.to_string(),
                    message,
                },
                event_id: 0, // Will be assigned by EventQueue
            })
        })
        .collect()
}

pub fn build_online_schedules(config: &SimulationConfig, rng: &mut impl Rng) -> OnlineSchedulePlan {
    let mut schedules = HashMap::new();
    let mut cohort_online_counts: HashMap<String, usize> = HashMap::new();
    let mut cohort_total_counts: HashMap<String, usize> = HashMap::new();
    let mut node_cohort_assignments: HashMap<String, NodeCohortAssignment> = HashMap::new();
    let simulated_seconds_per_step = simulated_seconds_per_step(config);
    let steps = config.effective_steps();
    let cohorts = if config.cohorts.is_empty() {
        None
    } else {
        Some(config.cohorts.as_slice())
    };

    // Default message type weights from config (used when cohort doesn't specify)
    let default_weights = config
        .message_type_weights
        .clone()
        .unwrap_or_else(MessageTypeWeights::default);

    for node_id in &config.node_ids {
        let (cohort_name_str, schedule, message_weights) = if let Some(cohorts) = cohorts {
            let cohort = select_cohort(cohorts, rng);
            let name = cohort_name(cohort).to_string();
            let schedule = build_cohort_schedule(cohort, steps, simulated_seconds_per_step, rng);
            // Use cohort-specific weights if defined, otherwise use config default
            let weights = cohort.message_type_weights();
            let weights = if weights.direct == 1.0 && weights.public == 0.0 && weights.group == 0.0
            {
                // This is the default - check if cohort actually defined weights
                // If not, use the simulation-level default
                default_weights.clone()
            } else {
                weights
            };
            (name, schedule, weights)
        } else {
            let name = "legacy".to_string();
            let schedule = build_availability_schedule(config, rng);
            (name, schedule, default_weights.clone())
        };

        let online_count = schedule.iter().filter(|online| **online).count();
        *cohort_online_counts
            .entry(cohort_name_str.clone())
            .or_insert(0) += online_count;
        *cohort_total_counts
            .entry(cohort_name_str.clone())
            .or_insert(0) += schedule.len();
        schedules.insert(node_id.clone(), schedule);

        // Store the cohort assignment with message type weights
        node_cohort_assignments.insert(
            node_id.clone(),
            NodeCohortAssignment {
                node_id: node_id.clone(),
                cohort_name: cohort_name_str,
                message_type_weights: message_weights,
            },
        );
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
        node_cohort_assignments,
    }
}

pub fn build_planned_sends(
    config: &SimulationConfig,
    rng: &mut impl Rng,
    graph: &HashMap<String, Vec<String>>,
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    node_cohort_assignments: &HashMap<String, NodeCohortAssignment>,
    node_groups: &HashMap<String, Vec<String>>,
    mut crypto_rng: Option<&mut dyn RngCore>,
) -> Vec<PlannedSend> {
    let mut planned = Vec::new();
    let mut counter = 0usize;
    let steps = config.effective_steps();

    // Default message type weights
    let default_weights = MessageTypeWeights::default();

    for node_id in &config.node_ids {
        let friends = graph
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();

        // Get the node's groups
        let groups = node_groups.get(node_id).cloned().unwrap_or_default();

        // Get message type weights for this node
        let weights = node_cohort_assignments
            .get(node_id)
            .map(|a| &a.message_type_weights)
            .unwrap_or(&default_weights);

        match &config.post_frequency {
            PostFrequency::Poisson {
                lambda_per_step,
                lambda_per_hour,
                time_distribution: _,
            } => {
                let simulated_seconds_per_step = simulated_seconds_per_step(config).max(1.0);

                // For event-based simulation (lambda_per_hour specified), generate events
                // uniformly across the entire duration to avoid clustering in discrete steps
                if let Some(lambda_per_hour) = lambda_per_hour {
                    let total_duration_seconds = steps as f64 * simulated_seconds_per_step;
                    let total_duration_hours = total_duration_seconds / 3600.0;
                    let expected_total = lambda_per_hour * total_duration_hours;
                    let total_count = sample_poisson(rng, expected_total);

                    // Generate events at random times across the duration
                    for _ in 0..total_count {
                        let time_seconds = rng.gen::<f64>() * total_duration_seconds;
                        let step = (time_seconds / simulated_seconds_per_step).floor() as usize;
                        let step = step.min(steps.saturating_sub(1));

                        if let Some(message) = build_planned_message(
                            node_id,
                            &friends,
                            &groups,
                            weights,
                            config,
                            encryption,
                            keypairs,
                            &mut crypto_rng,
                            &mut counter,
                            rng,
                        ) {
                            planned.push(PlannedSend { step, message });
                        }
                    }
                } else {
                    // Legacy step-based generation for lambda_per_step
                    let per_step = lambda_per_step.unwrap_or(0.0);
                    for step in 0..steps {
                        let count = sample_poisson(rng, per_step);
                        for _ in 0..count {
                            if let Some(message) = build_planned_message(
                                node_id,
                                &friends,
                                &groups,
                                weights,
                                config,
                                encryption,
                                keypairs,
                                &mut crypto_rng,
                                &mut counter,
                                rng,
                            ) {
                                planned.push(PlannedSend { step, message });
                            }
                        }
                    }
                }
            }
            PostFrequency::WeightedSchedule {
                weights: schedule_weights,
                total_posts,
            } => {
                let simulated_seconds_per_step = simulated_seconds_per_step(config).max(1.0);
                let use_hourly_weights = schedule_weights.len() == SIMULATION_HOURS_PER_DAY;
                let schedule_weights = if schedule_weights.len() == steps || use_hourly_weights {
                    schedule_weights.clone()
                } else {
                    vec![1.0; steps.max(1)]
                };
                for _ in 0..*total_posts {
                    let step = if use_hourly_weights {
                        let hour = sample_weighted_index(rng, &schedule_weights);
                        let offset_seconds = rng.gen_range(0.0..3600.0);
                        let simulated_seconds = (hour as f64 * 3600.0) + offset_seconds;
                        let step =
                            (simulated_seconds / simulated_seconds_per_step).floor() as usize;
                        step.min(steps.saturating_sub(1))
                    } else {
                        sample_weighted_index(rng, &schedule_weights)
                    };
                    if let Some(message) = build_planned_message(
                        node_id,
                        &friends,
                        &groups,
                        weights,
                        config,
                        encryption,
                        keypairs,
                        &mut crypto_rng,
                        &mut counter,
                        rng,
                    ) {
                        planned.push(PlannedSend { step, message });
                    }
                }
            }
        }
    }
    planned
}

/// Build a planned message based on message type weights
fn build_planned_message(
    node_id: &str,
    friends: &[String],
    groups: &[String],
    weights: &MessageTypeWeights,
    config: &SimulationConfig,
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    crypto_rng: &mut Option<&mut dyn RngCore>,
    counter: &mut usize,
    rng: &mut impl Rng,
) -> Option<SimMessage> {
    // Sample message type based on weights
    let random_value: f64 = rng.gen();
    let message_type = weights.sample(random_value);

    match message_type {
        MessageType::Direct => {
            // Direct message requires a friend as recipient
            if friends.is_empty() {
                return None;
            }
            let recipient = friends[rng.gen_range(0..friends.len())].clone();
            let body = generate_message_body(rng, &config.message_size_distribution);
            let payload = build_message_payload(
                encryption, keypairs, crypto_rng, &body, &recipient, node_id, *counter,
            );
            let message_id = ContentId::from_value(&payload).expect("serialize simulation payload");
            *counter += 1;
            Some(SimMessage {
                id: message_id.0.clone(),
                sender: node_id.to_string(),
                recipient,
                body,
                payload,
                message_kind: MessageKind::Direct,
                group_id: None,
            })
        }
        MessageType::Public => {
            // Public message - recipient is "*" (broadcast)
            let body = generate_message_body(rng, &config.message_size_distribution);
            // Public messages are always plaintext
            let salt = format!("{node_id}-{counter}");
            let payload = build_plaintext_payload(body.clone(), salt.as_bytes());
            let message_id = ContentId::from_value(&payload).expect("serialize simulation payload");
            *counter += 1;
            Some(SimMessage {
                id: message_id.0.clone(),
                sender: node_id.to_string(),
                recipient: "*".to_string(),
                body,
                payload,
                message_kind: MessageKind::Public,
                group_id: None,
            })
        }
        MessageType::Group => {
            // Group message requires being in a group
            if groups.is_empty() {
                // Fall back to direct message if no groups
                if friends.is_empty() {
                    return None;
                }
                let recipient = friends[rng.gen_range(0..friends.len())].clone();
                let body = generate_message_body(rng, &config.message_size_distribution);
                let payload = build_message_payload(
                    encryption, keypairs, crypto_rng, &body, &recipient, node_id, *counter,
                );
                let message_id =
                    ContentId::from_value(&payload).expect("serialize simulation payload");
                *counter += 1;
                return Some(SimMessage {
                    id: message_id.0.clone(),
                    sender: node_id.to_string(),
                    recipient,
                    body,
                    payload,
                    message_kind: MessageKind::Direct,
                    group_id: None,
                });
            }

            // Select a random group
            let group_id = groups[rng.gen_range(0..groups.len())].clone();
            let body = generate_message_body(rng, &config.message_size_distribution);
            // Group messages use symmetric encryption (handled by the client when sending)
            // For planning, we build a plaintext payload that will be encrypted at send time
            let salt = format!("{node_id}-{group_id}-{counter}");
            let payload = build_plaintext_payload(body.clone(), salt.as_bytes());
            let message_id = ContentId::from_value(&payload).expect("serialize simulation payload");
            *counter += 1;
            Some(SimMessage {
                id: message_id.0.clone(),
                sender: node_id.to_string(),
                // For group messages, recipient is the group ID for tracking purposes
                recipient: group_id.clone(),
                body,
                payload,
                message_kind: MessageKind::FriendGroup,
                group_id: Some(group_id),
            })
        }
    }
}

pub(crate) fn build_message_payload(
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

/// Split the full friend graph into active and pending sets based on
/// `FriendRequestConfig.initial_friend_fraction`.
///
/// Returns `(active_links, pending_pairs, acceptance_delay)`.  When no
/// `FriendRequestConfig` is configured all links are active immediately and the
/// pending list is empty.
fn split_friend_graph(
    all_links: HashSet<(String, String)>,
    fr_config: &Option<FriendRequestConfig>,
    rng: &mut impl Rng,
) -> (
    HashSet<(String, String)>,
    Vec<(String, String)>,
    Option<LatencyDistribution>,
) {
    let Some(cfg) = fr_config else {
        return (all_links, Vec::new(), None);
    };

    let fraction = cfg.initial_friend_fraction.clamp(0.0, 1.0);
    // De-duplicate: treat (a,b) and (b,a) as the same pair so we don't
    // put both directions into pending and then activate both separately.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (a, b) in &all_links {
        let key = if a <= b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        };
        if seen.insert(key.clone()) {
            pairs.push(key);
        }
    }

    let n_active = ((pairs.len() as f64) * fraction).round() as usize;
    // Shuffle so the active subset is random
    use rand::seq::SliceRandom;
    pairs.shuffle(rng);

    let (active_pairs, pending_pairs) = pairs.split_at(n_active.min(pairs.len()));

    let mut active_links: HashSet<(String, String)> = HashSet::new();
    for (a, b) in active_pairs {
        active_links.insert((a.clone(), b.clone()));
        active_links.insert((b.clone(), a.clone()));
    }

    let pending: Vec<(String, String)> = pending_pairs.to_vec();
    let delay = Some(cfg.acceptance_delay.clone());
    (active_links, pending, delay)
}

/// Build `FriendRequestSend` events for a list of pending friendship pairs.
///
/// Events are scattered across `[start_time, end_time)` using a Poisson
/// process whose rate is `fr_config.request_rate_per_hour` per pending pair.
fn build_friend_request_events(
    pending: &[(String, String)],
    fr_config: &FriendRequestConfig,
    start_time: f64,
    end_time: f64,
    rng: &mut impl Rng,
) -> Vec<ScheduledEvent> {
    use super::event::Event;

    let duration_hours = (end_time - start_time) / 3600.0;
    let mut events = Vec::new();
    let mut counter = 0u64;

    for (a, b) in pending {
        // Expected number of requests for this pair over the duration.
        // We schedule exactly one request per pair (the first one that would
        // fire under the Poisson process) so that each pending friendship is
        // activated at most once.
        let expected = fr_config.request_rate_per_hour * duration_hours;
        if expected <= 0.0 {
            continue;
        }
        // Sample the time of the first request as an exponential inter-arrival.
        let u: f64 = rng.gen::<f64>().max(1e-9); // avoid log(0)
        let delay_hours = -u.ln() / fr_config.request_rate_per_hour;
        let event_time = start_time + (delay_hours * 3600.0);
        if event_time >= end_time {
            continue; // Would fire after simulation ends
        }
        events.push(ScheduledEvent {
            time: event_time,
            event: Event::FriendRequestSend {
                sender_id: a.clone(),
                recipient_id: b.clone(),
            },
            event_id: counter,
        });
        counter += 1;
    }
    events
}

/// Build a reply SimMessage of the same kind as the original.
///
/// Returns `None` when the original kind does not warrant a reply (Meta,
/// StoreForPeer) or when the required context is missing (e.g. no group_id for
/// a FriendGroup message).
pub(crate) fn build_reply_sim_message(
    replier_id: &str,
    original_kind: MessageKind,
    original_sender: &str,
    original_group_id: Option<&str>,
    encryption: &MessageEncryption,
    keypairs: &HashMap<String, StoredKeypair>,
    size_dist: &super::MessageSizeDistribution,
    rng: &mut impl Rng,
    counter: usize,
) -> Option<SimMessage> {
    match original_kind {
        MessageKind::Direct => {
            let body = generate_message_body(rng, size_dist);
            let rng_dyn: &mut dyn RngCore = rng;
            let mut crypto_rng_opt: Option<&mut dyn RngCore> = Some(rng_dyn);
            let payload = build_message_payload(
                encryption,
                keypairs,
                &mut crypto_rng_opt,
                &body,
                original_sender,
                replier_id,
                counter,
            );
            let message_id = ContentId::from_value(&payload).ok()?;
            Some(SimMessage {
                id: message_id.0,
                sender: replier_id.to_string(),
                recipient: original_sender.to_string(),
                body,
                payload,
                message_kind: MessageKind::Direct,
                group_id: None,
            })
        }
        MessageKind::Public => {
            let body = generate_message_body(rng, size_dist);
            let salt = format!("{replier_id}-reply-{counter}");
            let payload = build_plaintext_payload(body.clone(), salt.as_bytes());
            let message_id = ContentId::from_value(&payload).ok()?;
            Some(SimMessage {
                id: message_id.0,
                sender: replier_id.to_string(),
                recipient: "*".to_string(),
                body,
                payload,
                message_kind: MessageKind::Public,
                group_id: None,
            })
        }
        MessageKind::FriendGroup => {
            let group_id = original_group_id?.to_string();
            let body = generate_message_body(rng, size_dist);
            let salt = format!("{replier_id}-{group_id}-reply-{counter}");
            let payload = build_plaintext_payload(body.clone(), salt.as_bytes());
            let message_id = ContentId::from_value(&payload).ok()?;
            Some(SimMessage {
                id: message_id.0,
                sender: replier_id.to_string(),
                recipient: group_id.clone(),
                body,
                payload,
                message_kind: MessageKind::FriendGroup,
                group_id: Some(group_id),
            })
        }
        // Do not reply to meta or store-for-peer messages.
        MessageKind::Meta | MessageKind::StoreForPeer => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::{MessageSizeDistribution, SimulatedTimeConfig};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn test_generate_online_schedule_events_always_online() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let cohort = OnlineCohortDefinition::AlwaysOnline {
            name: "always".to_string(),
            share: 1.0,
            message_type_weights: None,
        };

        let events = generate_online_schedule_events("client1", &cohort, 0.0, 3600.0, &mut rng);

        // Should have exactly one event: going online at start
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].time, 0.0);

        if let super::super::Event::OnlineTransition {
            client_id,
            going_online,
        } = &events[0].event
        {
            assert_eq!(client_id, "client1");
            assert!(going_online);
        } else {
            panic!("Expected OnlineTransition event");
        }
    }

    #[test]
    fn test_generate_online_schedule_events_rarely_online() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let cohort = OnlineCohortDefinition::RarelyOnline {
            name: "rarely".to_string(),
            share: 0.3,
            online_probability: 0.5,
            message_type_weights: None,
        };

        // Test over a full day (86400 seconds)
        let events = generate_online_schedule_events("client1", &cohort, 0.0, 86400.0, &mut rng);

        // Should have at least some events
        assert!(events.len() > 0, "Expected some online/offline events");

        // Verify events are OnlineTransition events
        for event in &events {
            assert!(matches!(
                event.event,
                super::super::Event::OnlineTransition { .. }
            ));
        }

        // Verify events are in chronological order
        for i in 1..events.len() {
            assert!(events[i].time >= events[i - 1].time);
        }

        // Verify they alternate between online and offline (or at least toggle state)
        if events.len() > 1 {
            let mut last_state = None;
            for event in &events {
                if let super::super::Event::OnlineTransition { going_online, .. } = &event.event {
                    if let Some(last) = last_state {
                        // State should change between consecutive events
                        assert_ne!(last, *going_online, "Events should alternate state");
                    }
                    last_state = Some(*going_online);
                }
            }
        }
    }

    #[test]
    fn test_generate_online_schedule_events_diurnal() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let cohort = OnlineCohortDefinition::Diurnal {
            name: "diurnal".to_string(),
            share: 0.5,
            online_probability: 0.5, // Reduced from 0.8 to increase chance of transitions
            timezone_offset_hours: 0,
            hourly_weights: vec![],
            message_type_weights: None,
        };

        // Test over a full day to ensure we see transitions
        let events = generate_online_schedule_events("client1", &cohort, 0.0, 86400.0, &mut rng);

        // Should have some transition events (might be 0 with low probability, so just check it doesn't crash)
        // With a 50% probability and sampling every 15 minutes over 24 hours, we should get some transitions

        // Verify events are OnlineTransition events
        for event in &events {
            assert!(matches!(
                event.event,
                super::super::Event::OnlineTransition { .. }
            ));
        }

        // Verify events are in chronological order
        for i in 1..events.len() {
            assert!(events[i].time >= events[i - 1].time);
        }
    }

    #[test]
    fn test_build_online_events_multiple_clients() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let config = SimulationConfig {
            node_ids: vec!["client1".to_string(), "client2".to_string()],
            steps: 0,
            duration_seconds: Some(3600),
            simulated_time: SimulatedTimeConfig {
                seconds_per_step: 60,
                default_speed_factor: 1.0,
            },
            friends_per_node: FriendsPerNode::Uniform { min: 1, max: 2 },
            clustering: None,
            post_frequency: PostFrequency::Poisson {
                lambda_per_step: None,
                lambda_per_hour: Some(1.0),
                time_distribution: None,
            },
            availability: None,
            cohorts: vec![OnlineCohortDefinition::AlwaysOnline {
                name: "always".to_string(),
                share: 1.0,
                message_type_weights: None,
            }],
            message_size_distribution: MessageSizeDistribution::Uniform { min: 100, max: 100 },
            encryption: None,
            groups: None,
            message_type_weights: None,
            reaction_config: None,
            friend_request_config: None,
            seed: 42,
        };

        let (events, assignments) = build_online_events(&config, 0.0, 3600.0, &mut rng);

        // Should have events for both clients
        assert_eq!(events.len(), 2);

        // Should have assignments for both clients
        assert_eq!(assignments.len(), 2);
        assert!(assignments.contains_key("client1"));
        assert!(assignments.contains_key("client2"));

        // Both should be in "always" cohort
        assert_eq!(assignments["client1"].cohort_name, "always");
        assert_eq!(assignments["client2"].cohort_name, "always");
    }
}
