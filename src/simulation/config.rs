use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;
use rand_distr::{Distribution, LogNormal, Normal};
use serde::{Deserialize, Serialize};

use crate::geo::{GeoLocation, GeoPrecision, GeoQuery};
use crate::relay::RelayConfig;

fn default_seconds_per_step() -> u64 {
    60
}

fn default_message_type_weights() -> MessageTypeWeights {
    MessageTypeWeights {
        direct: 1.0,
        public: 0.0,
        group: 0.0,
    }
}

/// Weights for different message types (direct, public, group).
/// These values are normalized at runtime to sum to 1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageTypeWeights {
    #[serde(default = "default_direct_weight")]
    pub direct: f64,
    #[serde(default)]
    pub public: f64,
    #[serde(default)]
    pub group: f64,
}

fn default_direct_weight() -> f64 {
    1.0
}

impl Default for MessageTypeWeights {
    fn default() -> Self {
        default_message_type_weights()
    }
}

impl MessageTypeWeights {
    /// Normalize weights so they sum to 1.0
    pub fn normalized(&self) -> (f64, f64, f64) {
        let total = self.direct.max(0.0) + self.public.max(0.0) + self.group.max(0.0);
        if total <= 0.0 {
            return (1.0, 0.0, 0.0);
        }
        (
            self.direct.max(0.0) / total,
            self.public.max(0.0) / total,
            self.group.max(0.0) / total,
        )
    }

    /// Sample a message type based on weights
    pub fn sample(&self, random_value: f64) -> MessageType {
        let (direct, public, _group) = self.normalized();
        if random_value < direct {
            MessageType::Direct
        } else if random_value < direct + public {
            MessageType::Public
        } else {
            MessageType::Group
        }
    }
}

/// Message type for simulation planning
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    Direct,
    Public,
    Group,
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
    /// Per-node geo overrides keyed by node ID.
    #[serde(default)]
    pub node_geo_overrides: HashMap<String, SimGeoConfig>,
    pub message_size_distribution: MessageSizeDistribution,
    pub encryption: Option<MessageEncryption>,
    #[serde(default)]
    pub groups: Option<GroupsConfig>,
    #[serde(default)]
    pub message_type_weights: Option<MessageTypeWeights>,
    #[serde(default)]
    pub reaction_config: Option<ReactionConfig>,
    #[serde(default)]
    pub friend_request_config: Option<FriendRequestConfig>,
    pub seed: u64,
}

fn default_geo_precision() -> String {
    "city".to_string()
}

/// Geo configuration for simulated peers/cohorts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimGeoConfig {
    pub country_code: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub geohash: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default = "default_geo_precision")]
    pub post_precision: String,
    #[serde(default = "default_geo_precision")]
    pub query_precision: String,
}

impl SimGeoConfig {
    pub fn post_geo_location(&self) -> Option<GeoLocation> {
        let precision = self.post_precision.parse::<GeoPrecision>().ok()?;
        if precision == GeoPrecision::None {
            return None;
        }
        Some(GeoLocation {
            precision,
            country_code: Some(self.country_code.clone()),
            region: self.region.clone(),
            city: self.city.clone(),
            geohash: self.geohash.clone(),
            lat: self.lat,
            lon: self.lon,
        })
    }

    pub fn query_geo(&self) -> Option<GeoQuery> {
        let precision = self.query_precision.parse::<GeoPrecision>().ok()?;
        match precision {
            GeoPrecision::None => None,
            GeoPrecision::Country => Some(GeoQuery {
                geohash_prefix: None,
                country_code: Some(self.country_code.clone()),
                region: None,
                city: None,
            }),
            GeoPrecision::Region => Some(GeoQuery {
                geohash_prefix: None,
                country_code: Some(self.country_code.clone()),
                region: self.region.clone(),
                city: None,
            }),
            GeoPrecision::City => Some(GeoQuery {
                geohash_prefix: None,
                country_code: Some(self.country_code.clone()),
                region: self.region.clone(),
                city: self.city.clone(),
            }),
            GeoPrecision::Neighborhood | GeoPrecision::Exact => Some(GeoQuery {
                geohash_prefix: self
                    .geohash
                    .as_ref()
                    .map(|g| g.chars().take(5).collect::<String>()),
                country_code: None,
                region: None,
                city: None,
            }),
        }
    }
}

/// Configuration for dynamic friend-request simulation.
///
/// When present, the scenario starts with only `initial_friend_fraction` of the
/// planned friendships active.  The remaining pairs exchange FriendRequest /
/// FriendAccept meta messages over time, activating the friendship (and the
/// routing edge) once the accept is processed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FriendRequestConfig {
    /// Fraction of friendships active at simulation start (0.0 – 1.0).
    /// The remaining pairs are established dynamically.  Defaults to 0.5.
    #[serde(default = "default_initial_friend_fraction")]
    pub initial_friend_fraction: f64,

    /// Poisson rate at which each node sends new friend requests (per hour).
    /// Controls how quickly the pending pairs get connected.  Defaults to 2.0.
    #[serde(default = "default_friend_request_rate")]
    pub request_rate_per_hour: f64,

    /// How long after a FriendRequest is sent before the accept arrives.
    #[serde(default)]
    pub acceptance_delay: LatencyDistribution,
}

fn default_initial_friend_fraction() -> f64 {
    0.5
}

fn default_friend_request_rate() -> f64 {
    2.0
}

/// Configuration for dynamic event generation (reactions)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionConfig {
    /// Probability that receiving a message triggers a reply
    #[serde(default)]
    pub reply_probability: f64,

    /// Delay before sending reply (in seconds)
    #[serde(default)]
    pub reply_delay_distribution: LatencyDistribution,
}

/// Configuration for groups in simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupsConfig {
    /// Number of groups to create
    #[serde(default = "default_group_count")]
    pub count: usize,
    /// Distribution of group sizes
    #[serde(default)]
    pub size_distribution: GroupSizeDistribution,
    /// How many groups each node should be a member of
    #[serde(default)]
    pub memberships_per_node: GroupMembershipsPerNode,
}

fn default_group_count() -> usize {
    3
}

impl Default for GroupsConfig {
    fn default() -> Self {
        Self {
            count: default_group_count(),
            size_distribution: GroupSizeDistribution::default(),
            memberships_per_node: GroupMembershipsPerNode::default(),
        }
    }
}

/// Distribution of group sizes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupSizeDistribution {
    /// All groups have the same size range
    Uniform { min: usize, max: usize },
    /// Groups are sized to include a fraction of all nodes
    FractionOfNodes {
        min_fraction: f64,
        max_fraction: f64,
    },
}

impl Default for GroupSizeDistribution {
    fn default() -> Self {
        GroupSizeDistribution::FractionOfNodes {
            min_fraction: 0.2,
            max_fraction: 0.5,
        }
    }
}

/// How many groups each node should be a member of
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupMembershipsPerNode {
    /// Each node is in a fixed number of groups
    Fixed { count: usize },
    /// Each node is in a random number of groups
    Uniform { min: usize, max: usize },
}

impl Default for GroupMembershipsPerNode {
    fn default() -> Self {
        GroupMembershipsPerNode::Fixed { count: 1 }
    }
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
        #[serde(default)]
        message_type_weights: Option<MessageTypeWeights>,
        #[serde(default)]
        geo: Option<SimGeoConfig>,
    },
    RarelyOnline {
        name: String,
        share: f64,
        online_probability: f64,
        #[serde(default)]
        message_type_weights: Option<MessageTypeWeights>,
        #[serde(default)]
        geo: Option<SimGeoConfig>,
    },
    Diurnal {
        name: String,
        share: f64,
        online_probability: f64,
        #[serde(default)]
        timezone_offset_hours: i32,
        #[serde(default)]
        hourly_weights: Vec<f64>,
        #[serde(default)]
        message_type_weights: Option<MessageTypeWeights>,
        #[serde(default)]
        geo: Option<SimGeoConfig>,
    },
}

impl OnlineCohortDefinition {
    /// Get the message type weights for this cohort, returning defaults if not specified
    pub fn message_type_weights(&self) -> MessageTypeWeights {
        match self {
            OnlineCohortDefinition::AlwaysOnline {
                message_type_weights,
                ..
            }
            | OnlineCohortDefinition::RarelyOnline {
                message_type_weights,
                ..
            }
            | OnlineCohortDefinition::Diurnal {
                message_type_weights,
                ..
            } => message_type_weights.clone().unwrap_or_default(),
        }
    }

    pub fn geo_config(&self) -> Option<&SimGeoConfig> {
        match self {
            OnlineCohortDefinition::AlwaysOnline { geo, .. }
            | OnlineCohortDefinition::RarelyOnline { geo, .. }
            | OnlineCohortDefinition::Diurnal { geo, .. } => geo.as_ref(),
        }
    }
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
        /// Optional time distribution for event spacing
        #[serde(default)]
        time_distribution: Option<TimeDistribution>,
    },
    WeightedSchedule {
        weights: Vec<f64>,
        total_posts: usize,
    },
}

/// Distribution of event times within a time window
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeDistribution {
    /// Events uniformly distributed across time window
    Uniform,
    /// Events clustered around certain times
    Clustered {
        /// Number of clusters
        cluster_count: usize,
        /// Cluster spread in seconds
        cluster_spread: f64,
    },
    /// Bursty traffic - events come in bursts
    Bursty {
        /// Number of bursts
        burst_count: usize,
        /// Duration of each burst in seconds
        burst_duration: f64,
    },
}

impl Default for TimeDistribution {
    fn default() -> Self {
        TimeDistribution::Uniform
    }
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
            qos: crate::relay::RelayQosConfig::default(),
            blob_max_chunk_bytes: 512 * 1024,
            blob_daily_quota_bytes: 500 * 1024 * 1024,
            blob_ttl: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationScenarioConfig {
    pub simulation: SimulationConfig,
    pub relay: RelayConfigToml,
    pub direct_enabled: Option<bool>,
    /// Optional network-impairment settings (drop rate, latency distributions).
    /// When absent all messages are delivered instantly with no drops.
    #[serde(default)]
    pub network_conditions: Option<NetworkConditions>,
}

/// Latency distribution for network operations
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LatencyDistribution {
    /// Fixed latency (e.g., 0.5 seconds)
    Fixed { seconds: f64 },

    /// Uniform random latency
    Uniform { min: f64, max: f64 },

    /// Normal distribution (mean, std_dev)
    Normal { mean: f64, std_dev: f64 },

    /// Log-normal (for heavy-tailed delays)
    LogNormal { mean: f64, std_dev: f64 },
}

impl Default for LatencyDistribution {
    fn default() -> Self {
        LatencyDistribution::Fixed { seconds: 0.0 }
    }
}

impl LatencyDistribution {
    /// Sample a latency value from the distribution
    pub fn sample(&self, rng: &mut impl Rng) -> f64 {
        match self {
            LatencyDistribution::Fixed { seconds } => *seconds,
            LatencyDistribution::Uniform { min, max } => min + rng.gen::<f64>() * (max - min),
            LatencyDistribution::Normal { mean, std_dev } => {
                let normal = Normal::new(*mean, *std_dev).unwrap_or(Normal::new(0.0, 1.0).unwrap());
                normal.sample(rng).max(0.0)
            }
            LatencyDistribution::LogNormal { mean, std_dev } => {
                let log_normal =
                    LogNormal::new(*mean, *std_dev).unwrap_or(LogNormal::new(0.0, 1.0).unwrap());
                log_normal.sample(rng)
            }
        }
    }
}

/// Network conditions for event-based simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConditions {
    /// Latency for direct peer-to-peer delivery
    #[serde(default)]
    pub direct_latency: LatencyDistribution,

    /// Latency for posting to relay
    #[serde(default)]
    pub relay_post_latency: LatencyDistribution,

    /// Latency for fetching from relay
    #[serde(default)]
    pub relay_fetch_latency: LatencyDistribution,

    /// Probability of message drop (0.0 = no drops, 0.1 = 10% drop rate)
    #[serde(default)]
    pub drop_probability: f64,
}

impl Default for NetworkConditions {
    fn default() -> Self {
        NetworkConditions {
            direct_latency: LatencyDistribution::default(),
            relay_post_latency: LatencyDistribution::default(),
            relay_fetch_latency: LatencyDistribution::default(),
            drop_probability: 0.0,
        }
    }
}

/// Time control configuration for event-based simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeControlConfig {
    /// Initial time control mode
    #[serde(default)]
    pub mode: crate::simulation::event::TimeControlMode,

    /// Speed factor for real-time mode (simulated_seconds / real_seconds)
    #[serde(default = "default_speed_factor")]
    pub speed_factor: f64,
}

impl Default for TimeControlConfig {
    fn default() -> Self {
        TimeControlConfig {
            mode: crate::simulation::event::TimeControlMode::FastForward,
            speed_factor: default_speed_factor(),
        }
    }
}
