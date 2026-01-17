use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::relay::RelayConfig;

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
