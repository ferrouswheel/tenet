use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::crypto::{StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    decrypt_encrypted_payload, Envelope, MessageKind, MetaMessage,
};

use super::{
    HistoricalMessage, MessageEncryption, MetricsTracker, RollingLatencyTracker, SimMessage,
    SimulationMetrics, SIMULATION_ACK_WINDOW_STEPS, SIMULATION_HPKE_INFO, SIMULATION_PAYLOAD_AAD,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreForPayload {
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct StoredForPeerMessage {
    store_for_id: String,
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct PendingOnlineBroadcast {
    sender_id: String,
    recipient_id: String,
    sent_step: usize,
    expires_at: usize,
}

pub(crate) struct ClientContext<'a> {
    pub direct_enabled: bool,
    pub ttl_seconds: u64,
    pub encryption: MessageEncryption,
    pub direct_links: &'a HashSet<(String, String)>,
    pub neighbors: &'a HashMap<String, Vec<String>>,
    pub keypairs: &'a HashMap<String, StoredKeypair>,
    pub message_history: &'a mut HashMap<(String, String), Vec<HistoricalMessage>>,
    pub message_send_steps: &'a mut HashMap<String, usize>,
    pub pending_forwarded_messages: &'a mut HashSet<String>,
    pub metrics: &'a mut SimulationMetrics,
    pub metrics_tracker: &'a mut MetricsTracker,
    pub log_sink: &'a Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
}

impl<'a> ClientContext<'a> {
    fn log_action(&self, step: usize, message: impl Into<String>) {
        if let Some(log_sink) = &self.log_sink {
            log_sink(format!("[sim step={step}] {}", message.into()));
        }
    }
}

pub(crate) struct ClientSendOutcome {
    pub envelopes: Vec<Envelope>,
    pub direct_deliveries: Vec<SimMessage>,
}

pub(crate) struct ClientBroadcastOutcome {
    pub envelopes: Vec<Envelope>,
    pub delivered_missed: usize,
}

pub(crate) trait Client: Send {
    fn id(&self) -> &str;
    fn is_online(&self, step: usize) -> bool;
    fn inbox(&self) -> Vec<SimMessage>;
    fn stored_forward_count(&self) -> usize;
    fn receive_message(&mut self, message: SimMessage) -> bool;

    fn enqueue_sends(
        &mut self,
        step: usize,
        messages: &[SimMessage],
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome;

    fn handle_inbox(
        &mut self,
        step: usize,
        envelopes: Vec<Envelope>,
        context: &mut ClientContext<'_>,
        rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> usize;

    fn announce_online(&mut self, step: usize, context: &mut ClientContext<'_>) -> Vec<Envelope>;

    fn process_pending_online_broadcasts(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome;

    fn forward_store_forwards(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> Vec<Envelope>;

    fn handle_direct_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize>;
}

#[derive(Debug, Clone)]
pub struct SimulationClient {
    id: String,
    schedule: Vec<bool>,
    inbox: Vec<SimMessage>,
    seen: HashSet<String>,
    last_seen_by_peer: HashMap<String, u64>,
    pending_online_broadcasts: Vec<PendingOnlineBroadcast>,
    stored_forwards: Vec<StoredForPeerMessage>,
}

impl SimulationClient {
    pub fn new(id: &str, schedule: Vec<bool>) -> Self {
        Self {
            id: id.to_string(),
            schedule,
            inbox: Vec::new(),
            seen: HashSet::new(),
            last_seen_by_peer: HashMap::new(),
            pending_online_broadcasts: Vec::new(),
            stored_forwards: Vec::new(),
        }
    }

    fn record_message_send(
        &self,
        message: &SimMessage,
        step: usize,
        envelope: &Envelope,
        context: &mut ClientContext<'_>,
    ) {
        context.message_send_steps.insert(message.id.clone(), step);
        context
            .message_history
            .entry((message.sender.clone(), message.recipient.clone()))
            .or_default()
            .push(HistoricalMessage {
                send_step: step,
                envelope: envelope.clone(),
            });
    }

    fn update_last_seen(
        &mut self,
        sender_id: &str,
        message_id: &str,
        context: &mut ClientContext<'_>,
    ) {
        let Some(send_step) = context.message_send_steps.get(message_id) else {
            return;
        };
        let entry = self
            .last_seen_by_peer
            .entry(sender_id.to_string())
            .or_insert(0);
        *entry = (*entry).max(*send_step as u64);
    }

    fn last_seen_for(&self, sender_id: &str) -> u64 {
        self.last_seen_by_peer.get(sender_id).copied().unwrap_or(0)
    }

    fn record_delivery_metrics(
        &mut self,
        message: &SimMessage,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> Option<usize> {
        if context.pending_forwarded_messages.remove(&message.id) {
            context.metrics.store_forwards_delivered =
                context.metrics.store_forwards_delivered.saturating_add(1);
            context.metrics_tracker.record_store_forward_delivery();
        }
        let latency = context
            .metrics_tracker
            .record_delivery(&message.id, &self.id, step);
        self.update_last_seen(&message.sender, &message.id, context);
        latency
    }

    fn apply_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        delivery_kind: DeliveryKind,
    ) -> Option<usize> {
        if !self.receive_message(message.clone()) {
            return None;
        }
        match delivery_kind {
            DeliveryKind::Direct => {
                context.metrics.direct_deliveries =
                    context.metrics.direct_deliveries.saturating_add(1);
            }
            DeliveryKind::Inbox => {
                context.metrics.inbox_deliveries =
                    context.metrics.inbox_deliveries.saturating_add(1);
            }
            DeliveryKind::Missed => {
                context.metrics_tracker.record_missed_message_delivery();
            }
        }
        let latency = self.record_delivery_metrics(&message, step, context);
        if matches!(delivery_kind, DeliveryKind::Missed) {
            context.log_action(
                step + 1,
                format!("{} received missed message {}", self.id, message.id),
            );
        }
        latency
    }

    fn build_meta_envelope(
        &self,
        sender_id: &str,
        recipient_id: &str,
        step: usize,
        meta: &MetaMessage,
        context: &mut ClientContext<'_>,
    ) -> Option<Envelope> {
        let payload = build_meta_payload(meta).ok()?;
        let envelope = build_envelope_from_payload(
            sender_id.to_string(),
            recipient_id.to_string(),
            None,
            None,
            step as u64,
            context.ttl_seconds,
            MessageKind::Meta,
            None,
            payload,
        )
        .ok()?;
        context
            .metrics_tracker
            .record_meta_send(sender_id.to_string(), &envelope);
        Some(envelope)
    }

    fn decode_direct_envelope(
        &self,
        envelope: &Envelope,
        message_id: &str,
        recipient_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<SimMessage> {
        if envelope.header.message_kind != MessageKind::Direct {
            return None;
        }
        let body = match context.encryption {
            MessageEncryption::Plaintext => envelope.payload.body.clone(),
            MessageEncryption::Encrypted => {
                let keypair = context.keypairs.get(recipient_id)?;
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

    fn decode_store_for_envelope(
        &self,
        envelope: &Envelope,
        storage_peer_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<StoredForPeerMessage> {
        if envelope.header.message_kind != MessageKind::StoreForPeer {
            return None;
        }
        let store_for_id = envelope.header.store_for.clone()?;
        let storage_peer = envelope.header.storage_peer_id.clone()?;
        if storage_peer != storage_peer_id {
            return None;
        }
        let keypair = context.keypairs.get(storage_peer_id)?;
        let plaintext = decrypt_encrypted_payload(
            &envelope.payload,
            &keypair.private_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
        )
        .ok()?;
        let payload: StoreForPayload = serde_json::from_slice(&plaintext).ok()?;
        Some(StoredForPeerMessage {
            store_for_id,
            envelope: payload.envelope,
        })
    }

    fn build_store_for_envelope(
        &self,
        sender_id: &str,
        storage_peer_id: &str,
        store_for_id: &str,
        timestamp: u64,
        inner_envelope: &Envelope,
        context: &ClientContext<'_>,
    ) -> Option<Envelope> {
        let keypair = context.keypairs.get(storage_peer_id)?;
        let payload_bytes = serde_json::to_vec(&StoreForPayload {
            envelope: inner_envelope.clone(),
        })
        .ok()?;
        let mut rng = rand::thread_rng();
        let mut content_key = [0u8; CONTENT_KEY_SIZE];
        let mut nonce = [0u8; NONCE_SIZE];
        rng.fill_bytes(&mut content_key);
        rng.fill_bytes(&mut nonce);
        let outer_payload = build_encrypted_payload(
            &payload_bytes,
            &keypair.public_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        )
        .ok()?;
        build_envelope_from_payload(
            sender_id.to_string(),
            storage_peer_id.to_string(),
            Some(store_for_id.to_string()),
            Some(storage_peer_id.to_string()),
            timestamp,
            context.ttl_seconds,
            MessageKind::StoreForPeer,
            None,
            outer_payload,
        )
        .ok()
    }

    fn decode_envelope_action(
        &self,
        envelope: &Envelope,
        recipient_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<IncomingEnvelopeAction> {
        if envelope.header.verify_signature(envelope.version).is_err() {
            return None;
        }
        match envelope.header.message_kind {
            MessageKind::Direct => self
                .decode_direct_envelope(
                    envelope,
                    &envelope.header.message_id.0,
                    recipient_id,
                    context,
                )
                .map(IncomingEnvelopeAction::DirectMessage),
            MessageKind::StoreForPeer => self
                .decode_store_for_envelope(envelope, recipient_id, context)
                .map(IncomingEnvelopeAction::StoredForPeer),
            _ => None,
        }
    }

    fn store_forward_message(
        &mut self,
        step: usize,
        message: StoredForPeerMessage,
        context: &mut ClientContext<'_>,
    ) {
        self.stored_forwards.push(message);
        context.metrics.store_forwards_stored =
            context.metrics.store_forwards_stored.saturating_add(1);
        context.metrics_tracker.record_store_forward_stored();
        context.log_action(
            step + 1,
            format!("{} stored a store-forward message", self.id),
        );
    }

    fn select_storage_peer(
        &self,
        sender: &str,
        recipient: &str,
        context: &ClientContext<'_>,
    ) -> Option<String> {
        let sender_neighbors = context.neighbors.get(sender)?;
        let recipient_neighbors = context.neighbors.get(recipient)?;
        let sender_set: HashSet<&String> = sender_neighbors.iter().collect();
        let mut mutual: Vec<String> = recipient_neighbors
            .iter()
            .filter(|candidate| sender_set.contains(candidate))
            .cloned()
            .collect();
        mutual.sort();
        mutual
            .into_iter()
            .find(|candidate| candidate != sender && candidate != recipient)
    }

    fn handle_message_request(
        &mut self,
        requester: &str,
        responder: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> (Vec<Envelope>, usize) {
        let last_seen = self.last_seen_for(responder);
        let request = MetaMessage::MessageRequest {
            peer_id: responder.to_string(),
            since_timestamp: last_seen,
        };
        let mut envelopes = Vec::new();
        if let Some(envelope) =
            self.build_meta_envelope(requester, responder, step, &request, context)
        {
            envelopes.push(envelope);
        }
        context.metrics_tracker.record_missed_message_request();
        context.log_action(
            step + 1,
            format!("{requester} requested missed messages from {responder}"),
        );
        let Some(history) = context
            .message_history
            .get(&(responder.to_string(), requester.to_string()))
        else {
            return (envelopes, 0);
        };
        let entries: Vec<HistoricalMessage> = history
            .iter()
            .filter(|entry| entry.send_step as u64 >= last_seen)
            .cloned()
            .collect();
        let mut delivered = 0;
        for entry in entries {
            if self.deliver_missed_message(&entry.envelope, requester, step, context) {
                delivered += 1;
            }
        }
        if delivered > 0 {
            context.log_action(
                step + 1,
                format!("{responder} delivered {delivered} missed messages to {requester}"),
            );
        }
        (envelopes, delivered)
    }

    fn deliver_missed_message(
        &mut self,
        envelope: &Envelope,
        recipient_id: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> bool {
        let message_id = envelope.header.message_id.0.clone();
        let message = self.decode_direct_envelope(envelope, &message_id, recipient_id, context);
        let Some(message) = message else {
            return false;
        };
        self.apply_delivery(step, message, context, DeliveryKind::Missed)
            .is_some()
    }

    fn online_at(&self, step: usize) -> bool {
        self.schedule.get(step).copied().unwrap_or(false)
    }
}

impl Client for SimulationClient {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_online(&self, step: usize) -> bool {
        self.online_at(step)
    }

    fn inbox(&self) -> Vec<SimMessage> {
        self.inbox.clone()
    }

    fn stored_forward_count(&self) -> usize {
        self.stored_forwards.len()
    }

    fn receive_message(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.inbox.push(message);
            return true;
        }
        false
    }

    fn enqueue_sends(
        &mut self,
        step: usize,
        messages: &[SimMessage],
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome {
        let mut envelopes = Vec::new();
        let mut direct_deliveries = Vec::new();
        for message in messages {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let envelope = match build_envelope_from_payload(
                message.sender.clone(),
                message.recipient.clone(),
                None,
                None,
                timestamp,
                context.ttl_seconds,
                MessageKind::Direct,
                None,
                message.payload.clone(),
            ) {
                Ok(envelope) => envelope,
                Err(_) => continue,
            };
            self.record_message_send(message, step, &envelope, context);
            let recipient_online = online_set.contains(&message.recipient);
            if context.direct_enabled
                && context
                    .direct_links
                    .contains(&(message.sender.clone(), message.recipient.clone()))
                && recipient_online
            {
                if let Some(decoded) =
                    self.decode_direct_envelope(&envelope, &message.id, &message.recipient, context)
                {
                    direct_deliveries.push(decoded);
                }
            }

            if !recipient_online {
                if let Some(storage_peer_id) =
                    self.select_storage_peer(&message.sender, &message.recipient, context)
                {
                    if let Some(store_envelope) = self.build_store_for_envelope(
                        &message.sender,
                        &storage_peer_id,
                        &message.recipient,
                        timestamp,
                        &envelope,
                        context,
                    ) {
                        context
                            .metrics_tracker
                            .record_send(message, step, &store_envelope);
                        envelopes.push(store_envelope);
                        continue;
                    }
                }
            }

            context
                .metrics_tracker
                .record_send(message, step, &envelope);
            envelopes.push(envelope);
        }
        ClientSendOutcome {
            envelopes,
            direct_deliveries,
        }
    }

    fn handle_inbox(
        &mut self,
        step: usize,
        envelopes: Vec<Envelope>,
        context: &mut ClientContext<'_>,
        mut rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> usize {
        let mut received = 0usize;
        for envelope in envelopes {
            match self.decode_envelope_action(&envelope, &self.id, context) {
                Some(IncomingEnvelopeAction::DirectMessage(message)) => {
                    if let Some(latency) =
                        self.apply_delivery(step, message, context, DeliveryKind::Inbox)
                    {
                        if let Some(tracker) = rolling_latency.as_deref_mut() {
                            tracker.record(latency);
                        }
                        received = received.saturating_add(1);
                    }
                }
                Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                    self.store_forward_message(step, message, context);
                }
                None => {}
            }
        }
        received
    }

    fn announce_online(&mut self, step: usize, context: &mut ClientContext<'_>) -> Vec<Envelope> {
        let Some(neighbors) = context.neighbors.get(&self.id) else {
            return Vec::new();
        };
        if neighbors.is_empty() {
            return Vec::new();
        }
        context.log_action(
            step + 1,
            format!("{} announced online to {}", self.id, neighbors.join(", ")),
        );
        let mut envelopes = Vec::new();
        for neighbor in neighbors {
            let meta = MetaMessage::Online {
                peer_id: self.id.clone(),
                timestamp: step as u64,
            };
            if let Some(envelope) =
                self.build_meta_envelope(&self.id, neighbor, step, &meta, context)
            {
                envelopes.push(envelope);
            }
            self.pending_online_broadcasts.push(PendingOnlineBroadcast {
                sender_id: self.id.clone(),
                recipient_id: neighbor.clone(),
                sent_step: step,
                expires_at: step.saturating_add(SIMULATION_ACK_WINDOW_STEPS),
            });
            context.metrics_tracker.record_online_broadcast();
        }
        envelopes
    }

    fn process_pending_online_broadcasts(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome {
        let pending = std::mem::take(&mut self.pending_online_broadcasts);
        let mut remaining = Vec::with_capacity(pending.len());
        let mut delivered = 0usize;
        let mut envelopes = Vec::new();
        for pending in pending {
            if step > pending.expires_at {
                continue;
            }
            if online_set.contains(&pending.recipient_id) {
                let ack = MetaMessage::Ack {
                    peer_id: pending.recipient_id.clone(),
                    online_timestamp: pending.sent_step as u64,
                };
                if let Some(envelope) = self.build_meta_envelope(
                    &pending.recipient_id,
                    &pending.sender_id,
                    step,
                    &ack,
                    context,
                ) {
                    envelopes.push(envelope);
                }
                context.metrics_tracker.record_ack();
                context.log_action(
                    step + 1,
                    format!(
                        "{} acknowledged {} online",
                        pending.recipient_id, pending.sender_id
                    ),
                );
                let (request_envelopes, delivered_missed) = self.handle_message_request(
                    &pending.sender_id,
                    &pending.recipient_id,
                    step,
                    context,
                );
                envelopes.extend(request_envelopes);
                delivered = delivered.saturating_add(delivered_missed);
                continue;
            }
            remaining.push(pending);
        }
        self.pending_online_broadcasts = remaining;
        ClientBroadcastOutcome {
            envelopes,
            delivered_missed: delivered,
        }
    }

    fn forward_store_forwards(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> Vec<Envelope> {
        if !online_set.contains(&self.id) {
            return Vec::new();
        }
        let mut forwarded = Vec::new();
        let mut remaining = Vec::new();
        for entry in self.stored_forwards.drain(..) {
            if online_set.contains(&entry.store_for_id) {
                let message_id = entry.envelope.header.message_id.0.clone();
                context.pending_forwarded_messages.insert(message_id);
                context.metrics.store_forwards_forwarded =
                    context.metrics.store_forwards_forwarded.saturating_add(1);
                context
                    .metrics_tracker
                    .record_store_forward_forwarded(&self.id);
                context.log_action(
                    step + 1,
                    format!(
                        "{} forwarded stored message to {}",
                        self.id, entry.store_for_id
                    ),
                );
                forwarded.push(entry.envelope);
            } else {
                remaining.push(entry);
            }
        }
        self.stored_forwards = remaining;
        forwarded
    }

    fn handle_direct_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        mut rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize> {
        let latency = self.apply_delivery(step, message, context, DeliveryKind::Direct)?;
        if let Some(tracker) = rolling_latency.as_deref_mut() {
            tracker.record(latency);
        }
        Some(latency)
    }
}

#[derive(Clone, Copy)]
enum DeliveryKind {
    Direct,
    Inbox,
    Missed,
}

enum IncomingEnvelopeAction {
    DirectMessage(SimMessage),
    StoredForPeer(StoredForPeerMessage),
}
