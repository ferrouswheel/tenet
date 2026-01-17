use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::client::ClientMetrics;
use crate::protocol::{Envelope, MessageKind};

use super::{SimMessage, MESSAGE_KIND_SUMMARIES};

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
    pub(crate) fn empty() -> Self {
        Self {
            min: None,
            max: None,
            average: None,
        }
    }

    pub(crate) fn from_counts<I: IntoIterator<Item = usize>>(counts: I) -> Self {
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

#[derive(Debug, Clone)]
struct MessageTracking {
    send_step: usize,
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
    pub(crate) fn new(
        simulated_seconds_per_step: f64,
        cohort_online_rates: HashMap<String, f64>,
    ) -> Self {
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

    pub(crate) fn set_simulated_seconds_per_step(&mut self, simulated_seconds_per_step: f64) {
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

    pub(crate) fn sent_message_summary_by_kind(
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

    pub(crate) fn stored_forwarded_summary(&self, peer_ids: &[String]) -> CountSummary {
        CountSummary::from_counts(peer_ids.iter().map(|peer_id| {
            self.per_peer_store_forwarded
                .get(peer_id)
                .copied()
                .unwrap_or(0)
        }))
    }

    pub(crate) fn message_size_summary_by_kind(&self) -> HashMap<MessageKind, SizeSummary> {
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

    pub(crate) fn report(
        &self,
        aggregate_metrics: SimulationAggregateMetrics,
    ) -> SimulationMetricsReport {
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

#[derive(Debug)]
pub struct RollingLatencyTracker {
    window: usize,
    samples: VecDeque<usize>,
}

impl RollingLatencyTracker {
    pub(crate) fn new(window: usize) -> Self {
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

    pub(crate) fn snapshot(&self) -> RollingLatencySnapshot {
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
