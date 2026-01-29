use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rand::distributions::Uniform;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};
use serde::{Deserialize, Serialize};

use crate::crypto::{generate_keypair_with_rng, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::groups::GroupInfo;
use crate::protocol::{
    build_encrypted_payload, build_plaintext_payload, ContentId, Envelope, MessageKind, Payload,
};

use super::random::{generate_message_body, sample_poisson, sample_weighted_index, sample_zipf};
use super::{
    start_relay, ClusteringConfig, FriendsPerNode, GroupMembershipsPerNode, GroupSizeDistribution,
    MessageEncryption, MessageType, MessageTypeWeights, OnlineAvailability, OnlineCohortDefinition,
    PostFrequency, SimulationClient, SimulationConfig, SimulationHarness, SimulationReport,
    SimulationScenarioConfig, SimulationStepUpdate, SimulationTimingConfig,
    SIMULATION_HOURS_PER_DAY, SIMULATION_HPKE_INFO, SIMULATION_PAYLOAD_AAD,
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
        groups,
        node_groups,
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
