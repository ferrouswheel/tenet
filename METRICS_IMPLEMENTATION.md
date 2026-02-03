# Message Metrics Implementation

This document describes the message metrics functionality already implemented in the Tenet simulation.

## Overview

All requested message metrics features are **fully implemented** in `src/simulation/metrics.rs`. The implementation provides:

1. **Per-(sender, recipient) message counts** - Track traffic patterns between specific pairs of nodes
2. **Per-message latency tracking** - Measure delivery delays for individual messages
3. **Histogram-based latency reporting** - Analyze latency distribution across all messages

## Implementation Details

### 1. Per-(Sender, Recipient) Message Counts

**Location**: `src/simulation/metrics.rs:240-296`

The `MetricsTracker` maintains a `HashMap<SenderRecipient, usize>` that tracks how many messages were sent from each sender to each recipient.

```rust
pub struct MetricsTracker {
    per_sender_recipient_counts: HashMap<SenderRecipient, usize>,
    // ... other fields
}
```

**Tracking**: Messages are counted when `record_send()` is called:

```rust
pub fn record_send(&mut self, message: &SimMessage, step: usize, envelope: &Envelope) {
    let key = SenderRecipient {
        sender: message.sender.clone(),
        recipient: message.recipient.clone(),
    };
    *self.per_sender_recipient_counts.entry(key).or_insert(0) += 1;
    // ...
}
```

**Reporting**: The counts are included in the `SimulationMetricsReport`:

```rust
pub struct SimulationMetricsReport {
    pub per_sender_recipient_counts: Vec<SenderRecipientCount>,
    // ...
}

pub struct SenderRecipientCount {
    pub sender: String,
    pub recipient: String,
    pub count: usize,
}
```

### 2. Per-Message Latency Tracking

**Location**: `src/simulation/metrics.rs:241-358`

The tracker maintains separate latency statistics for:
- **First delivery**: Latency from send until the first recipient receives the message
- **All recipients delivery**: Latency from send until all intended recipients have received the message

```rust
pub struct MetricsTracker {
    first_delivery_latency: LatencyStats,
    all_recipients_delivery_latency: LatencyStats,
    first_delivery_latency_seconds: LatencyStats,
    all_recipients_delivery_latency_seconds: LatencyStats,
    // ...
}
```

**Tracking**: The `record_delivery()` method tracks both metrics:

```rust
pub fn record_delivery(&mut self, message_id: &str, recipient_id: &str, step: usize) -> Option<usize> {
    let tracking = self.messages.get_mut(message_id)?;
    let latency = step.saturating_sub(tracking.send_step);

    // Record first delivery
    if !tracking.first_delivery_recorded {
        self.first_delivery_latency.record(latency);
        let latency_seconds = ((latency as f64) * self.simulated_seconds_per_step).round() as usize;
        self.first_delivery_latency_seconds.record(latency_seconds);
        tracking.first_delivery_recorded = true;
    }

    // Record all-recipients delivery
    if tracking.delivered_recipients.len() == tracking.recipients.len()
       && !tracking.all_delivery_recorded {
        self.all_recipients_delivery_latency.record(latency);
        let latency_seconds = ((latency as f64) * self.simulated_seconds_per_step).round() as usize;
        self.all_recipients_delivery_latency_seconds.record(latency_seconds);
        tracking.all_delivery_recorded = true;
    }

    Some(latency)
}
```

### 3. Histogram-Based Latency Reporting

**Location**: `src/simulation/metrics.rs:35-41, 174-206`

Latencies are reported as histograms with the following structure:

```rust
pub struct LatencyHistogram {
    pub counts: HashMap<usize, usize>,  // Histogram buckets: latency -> count
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub average: Option<f64>,
    pub samples: usize,
}
```

The `LatencyStats` internal structure accumulates latency data:

```rust
struct LatencyStats {
    counts: HashMap<usize, usize>,  // Each unique latency value is a bucket
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
```

## Accessing the Metrics

### From Simulation Code

The metrics are accessible through the `SimulationHarness` and `EventBasedHarness`:

```rust
// Get current metrics
let metrics = harness.metrics();

// Get detailed metrics report
let report = harness.metrics_report();
```

### In the Metrics Report

When running a simulation, the final JSON output includes all metrics:

```json
{
  "metrics": {
    "planned_messages": 100,
    "sent_messages": 95,
    ...
  },
  "report": {
    "per_sender_recipient_counts": [
      {
        "sender": "peer-1",
        "recipient": "peer-2",
        "count": 15
      },
      ...
    ],
    "first_delivery_latency": {
      "counts": {
        "5": 10,
        "10": 20,
        "15": 8
      },
      "min": 5,
      "max": 15,
      "average": 9.8,
      "samples": 38
    },
    "all_recipients_delivery_latency": {
      "counts": {
        "10": 12,
        "20": 15,
        "30": 11
      },
      "min": 10,
      "max": 30,
      "average": 19.5,
      "samples": 38
    },
    "first_delivery_latency_seconds": {
      ...
    },
    "all_recipients_delivery_latency_seconds": {
      ...
    },
    ...
  }
}
```

## Dual Time Scales

The implementation tracks latencies in **two time scales**:

1. **Simulation steps** (`first_delivery_latency`, `all_recipients_delivery_latency`)
   - Raw step counts from the simulation
   - Useful for debugging simulation logic

2. **Simulated seconds** (`first_delivery_latency_seconds`, `all_recipients_delivery_latency_seconds`)
   - Converted to real-world time units
   - Uses `simulated_seconds_per_step` for conversion
   - Useful for analyzing realistic network performance

## Example: Analyzing Traffic Patterns

To analyze which node pairs exchange the most messages:

```rust
let report = harness.metrics_report();

// Sort by message count
let mut counts = report.per_sender_recipient_counts;
counts.sort_by(|a, b| b.count.cmp(&a.count));

// Print top 10 busiest pairs
for pair in counts.iter().take(10) {
    println!("{} -> {}: {} messages", pair.sender, pair.recipient, pair.count);
}
```

## Example: Analyzing Latency Distribution

To see how latencies are distributed:

```rust
let report = harness.metrics_report();
let latency = &report.first_delivery_latency;

println!("First delivery latency:");
println!("  Min: {:?} steps", latency.min);
println!("  Max: {:?} steps", latency.max);
println!("  Avg: {:.2} steps", latency.average.unwrap_or(0.0));
println!("  Histogram:");

// Sort buckets by latency value
let mut buckets: Vec<_> = latency.counts.iter().collect();
buckets.sort_by_key(|(latency, _)| *latency);

for (steps, count) in buckets {
    println!("    {} steps: {} messages", steps, count);
}
```

## Testing the Metrics

The metrics are tested indirectly through:
- `tests/simulation_client_tests.rs` - Tests client message handling
- `tests/simulation_harness_tests.rs` - Tests multi-peer scenarios
- Various scenario files in `scenarios/` - Integration testing

You can verify the metrics are working by running any simulation scenario:

```bash
cargo run --bin tenet-sim -- scenarios/small_dense_6.toml
```

The output JSON will include all metrics in the `report` section.

## Summary

✅ **Per-(sender, recipient) message counts** - Fully implemented and reported
✅ **First-receipt latency tracking** - Tracked per message with histogram
✅ **All-recipients delivery latency** - Tracked per message with histogram
✅ **Histogram reporting** - Complete with counts, min, max, average
✅ **Dual time scales** - Both steps and seconds provided

All requested features are production-ready and actively used in the simulation.
