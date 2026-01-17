use rand::Rng;
use rand_distr::{Distribution, LogNormal, Normal};

use super::MessageSizeDistribution;

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
