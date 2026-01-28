use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::crypto::{generate_content_key, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    build_plaintext_envelope, decrypt_encrypted_payload, Envelope, EnvelopeBuildError, MessageKind,
    MetaMessage, PayloadCryptoError,
};

use crate::simulation::{
    HistoricalMessage, MessageEncryption, MetricsTracker, RollingLatencyTracker, SimMessage,
    SimulationMetrics, SIMULATION_ACK_WINDOW_STEPS, SIMULATION_HPKE_INFO, SIMULATION_PAYLOAD_AAD,
};

#[derive(Debug, Clone)]
pub enum ClientEncryption {
    Plaintext,
    Encrypted {
        hpke_info: Vec<u8>,
        payload_aad: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    relay_url: String,
    ttl_seconds: u64,
    encryption: ClientEncryption,
}

impl ClientConfig {
    pub fn new(
        relay_url: impl Into<String>,
        ttl_seconds: u64,
        encryption: ClientEncryption,
    ) -> Self {
        Self {
            relay_url: relay_url.into(),
            ttl_seconds,
            encryption,
        }
    }

    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    pub fn encryption(&self) -> &ClientEncryption {
        &self.encryption
    }
}

#[derive(Debug, Clone)]
pub struct ClientMessage {
    pub message_id: String,
    pub sender_id: String,
    pub timestamp: u64,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub messages: Vec<ClientMessage>,
    pub errors: Vec<String>,
    pub fetched: usize,
}

#[derive(Debug)]
pub enum ClientError {
    Http(String),
    Crypto(PayloadCryptoError),
    Envelope(EnvelopeBuildError),
    Protocol(String),
    Time(String),
    Offline,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(error) => write!(f, "http error: {error}"),
            ClientError::Crypto(error) => write!(f, "crypto error: {error}"),
            ClientError::Envelope(error) => write!(f, "envelope error: {error}"),
            ClientError::Protocol(error) => write!(f, "protocol error: {error}"),
            ClientError::Time(error) => write!(f, "time error: {error}"),
            ClientError::Offline => write!(f, "client is offline"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<PayloadCryptoError> for ClientError {
    fn from(error: PayloadCryptoError) -> Self {
        ClientError::Crypto(error)
    }
}

impl From<EnvelopeBuildError> for ClientError {
    fn from(error: EnvelopeBuildError) -> Self {
        ClientError::Envelope(error)
    }
}

#[derive(Debug, Clone)]
pub struct RelayClient {
    keypair: StoredKeypair,
    config: ClientConfig,
    online: bool,
    feed: Vec<ClientMessage>,
    seen: HashSet<String>,
    known_peers: HashMap<String, String>, // peer_id -> signing_public_key_hex
}

impl RelayClient {
    pub fn new(keypair: StoredKeypair, config: ClientConfig) -> Self {
        Self {
            keypair,
            config,
            online: true,
            feed: Vec::new(),
            seen: HashSet::new(),
            known_peers: HashMap::new(),
        }
    }

    pub fn add_peer(&mut self, peer_id: String, signing_public_key_hex: String) {
        self.known_peers.insert(peer_id, signing_public_key_hex);
    }

    pub fn id(&self) -> &str {
        &self.keypair.id
    }

    pub fn public_key_hex(&self) -> &str {
        &self.keypair.public_key_hex
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn set_online(&mut self, online: bool) {
        self.online = online;
    }

    pub fn feed(&self) -> &[ClientMessage] {
        &self.feed
    }

    pub fn send_message(
        &self,
        recipient_id: &str,
        recipient_public_key_hex: &str,
        message: &str,
    ) -> Result<Envelope, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| ClientError::Time(err.to_string()))?
            .as_secs();

        let envelope = match self.config.encryption() {
            ClientEncryption::Plaintext => {
                let mut salt = [0u8; 16];
                OsRng.fill_bytes(&mut salt);
                build_plaintext_envelope(
                    self.id(),
                    recipient_id,
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Direct,
                    None,
                    message,
                    &salt,
                    &self.keypair.signing_private_key_hex,
                )?
            }
            ClientEncryption::Encrypted {
                hpke_info,
                payload_aad,
            } => {
                let content_key = generate_content_key();
                let mut nonce = [0u8; NONCE_SIZE];
                OsRng.fill_bytes(&mut nonce);
                let payload = build_encrypted_payload(
                    message.as_bytes(),
                    recipient_public_key_hex,
                    payload_aad,
                    hpke_info,
                    &content_key,
                    &nonce,
                    None,
                )?;
                build_envelope_from_payload(
                    self.id(),
                    recipient_id,
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Direct,
                    None,
                    payload,
                    &self.keypair.signing_private_key_hex,
                )?
            }
        };

        self.post_envelope(&envelope)?;
        Ok(envelope)
    }

    pub fn sync_inbox(&mut self, limit: Option<usize>) -> Result<SyncOutcome, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }
        let envelopes = self.fetch_inbox(limit)?;
        let fetched = envelopes.len();
        if fetched == 0 {
            return Ok(SyncOutcome {
                messages: Vec::new(),
                errors: Vec::new(),
                fetched: 0,
            });
        }

        let mut messages = Vec::new();
        let mut errors = Vec::new();
        for envelope in envelopes {
            if self.seen.contains(&envelope.header.message_id.0) {
                continue;
            }
            let result = self.decode_envelope(&envelope);
            match result {
                Ok(message) => {
                    self.seen.insert(envelope.header.message_id.0.clone());
                    self.feed.push(message.clone());
                    messages.push(message);
                }
                Err(error) => {
                    errors.push(error);
                }
            }
        }

        Ok(SyncOutcome {
            messages,
            errors,
            fetched,
        })
    }

    fn decode_envelope(&self, envelope: &Envelope) -> Result<ClientMessage, String> {
        // Verify signature using sender's public key
        if let Some(sender_signing_key) = self.known_peers.get(&envelope.header.sender_id) {
            envelope
                .header
                .verify_signature(envelope.version, sender_signing_key)
                .map_err(|error| format!("invalid header signature: {error:?}"))?;
        } else {
            return Err(format!(
                "unknown sender: {} (cannot verify signature)",
                envelope.header.sender_id
            ));
        }

        if envelope.header.message_kind != MessageKind::Direct {
            return Err(format!(
                "unexpected message kind: {:?}",
                envelope.header.message_kind
            ));
        }

        let body = match self.config.encryption() {
            ClientEncryption::Plaintext => envelope.payload.body.clone(),
            ClientEncryption::Encrypted {
                hpke_info,
                payload_aad,
            } => {
                let plaintext = decrypt_encrypted_payload(
                    &envelope.payload,
                    &self.keypair.private_key_hex,
                    payload_aad,
                    hpke_info,
                )
                .map_err(|error| format!("{error}"))?;
                String::from_utf8(plaintext).map_err(|error| format!("utf-8 error: {error}"))?
            }
        };

        Ok(ClientMessage {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: envelope.header.sender_id.clone(),
            timestamp: envelope.header.timestamp,
            body,
        })
    }

    fn post_envelope(&self, envelope: &Envelope) -> Result<(), ClientError> {
        let url = format!(
            "{}/envelopes",
            self.config.relay_url().trim_end_matches('/')
        );
        let response = ureq::post(&url).send_json(
            serde_json::to_value(envelope)
                .map_err(|error| ClientError::Protocol(format!("serialize envelope: {error}")))?,
        );

        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => {
                Err(ClientError::Http(format!("relay error: {code}")))
            }
            Err(err) => Err(ClientError::Http(err.to_string())),
        }
    }

    fn fetch_inbox(&self, limit: Option<usize>) -> Result<Vec<Envelope>, ClientError> {
        let base = self.config.relay_url().trim_end_matches('/');
        let url = if let Some(limit) = limit {
            format!("{base}/inbox/{}?limit={limit}", self.id())
        } else {
            format!("{base}/inbox/{}", self.id())
        };
        let response = ureq::get(&url)
            .call()
            .map_err(|error| ClientError::Http(error.to_string()))?;
        response
            .into_json()
            .map_err(|error| ClientError::Protocol(format!("deserialize inbox: {error}")))
    }
}

impl Client for RelayClient {
    fn id(&self) -> &str {
        &self.keypair.id
    }

    fn is_online(&self, _step: usize) -> bool {
        self.online
    }

    fn inbox(&self) -> Vec<SimMessage> {
        Vec::new()
    }

    fn stored_forward_count(&self) -> usize {
        0
    }

    fn metrics(&self) -> ClientMetrics {
        ClientMetrics::default()
    }

    fn receive_message(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.feed.push(ClientMessage {
                message_id: message.id,
                sender_id: message.sender,
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                body: message.body,
            });
            return true;
        }
        false
    }

    fn enqueue_sends(
        &mut self,
        _step: usize,
        messages: &[SimMessage],
        _online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome {
        if !self.online {
            return ClientSendOutcome {
                envelopes: Vec::new(),
                direct_deliveries: Vec::new(),
            };
        }

        let mut envelopes = Vec::new();
        for message in messages {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let Ok(envelope) = build_envelope_from_payload(
                message.sender.clone(),
                message.recipient.clone(),
                None,
                None,
                timestamp,
                context.ttl_seconds,
                MessageKind::Direct,
                None,
                message.payload.clone(),
                &self.keypair.signing_private_key_hex,
            ) else {
                continue;
            };
            if self.post_envelope(&envelope).is_ok() {
                envelopes.push(envelope);
            }
        }

        ClientSendOutcome {
            envelopes,
            direct_deliveries: Vec::new(),
        }
    }

    fn handle_inbox(
        &mut self,
        _step: usize,
        envelopes: Vec<Envelope>,
        _context: &mut ClientContext<'_>,
        _rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> usize {
        let mut received = 0usize;
        for envelope in envelopes {
            if self.seen.contains(&envelope.header.message_id.0) {
                continue;
            }
            match self.decode_envelope(&envelope) {
                Ok(message) => {
                    self.seen.insert(envelope.header.message_id.0.clone());
                    self.feed.push(message);
                    received = received.saturating_add(1);
                }
                Err(_) => continue,
            }
        }
        received
    }

    fn announce_online(&mut self, _step: usize, _context: &mut ClientContext<'_>) -> Vec<Envelope> {
        Vec::new()
    }

    fn process_pending_online_broadcasts(
        &mut self,
        _step: usize,
        _online_set: &HashSet<String>,
        _context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome {
        ClientBroadcastOutcome {
            envelopes: Vec::new(),
            delivered_missed: 0,
        }
    }

    fn forward_store_forwards(
        &mut self,
        _step: usize,
        _online_set: &HashSet<String>,
        _context: &mut ClientContext<'_>,
    ) -> Vec<Envelope> {
        Vec::new()
    }

    fn handle_direct_delivery(
        &mut self,
        _step: usize,
        _message: SimMessage,
        _context: &mut ClientContext<'_>,
        _rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize> {
        None
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientMetrics {
    pub messages_sent: usize,
    pub direct_deliveries: usize,
    pub inbox_deliveries: usize,
    pub missed_deliveries: usize,
    pub store_forwards_stored: usize,
    pub store_forwards_forwarded: usize,
    pub store_forwards_delivered: usize,
}

#[derive(Debug, Clone)]
pub struct ClientLogEvent {
    pub step: usize,
    pub client_id: String,
    pub message: String,
}

pub trait ClientLogSink: Send + Sync {
    fn log(&self, event: ClientLogEvent);
}

impl<F> ClientLogSink for F
where
    F: Fn(ClientLogEvent) + Send + Sync,
{
    fn log(&self, event: ClientLogEvent) {
        self(event);
    }
}

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
    fn metrics(&self) -> ClientMetrics;
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

#[derive(Clone)]
pub struct SimulationClient {
    id: String,
    schedule: Vec<bool>,
    inbox: Vec<SimMessage>,
    seen: HashSet<String>,
    last_seen_by_peer: HashMap<String, u64>,
    pending_online_broadcasts: Vec<PendingOnlineBroadcast>,
    stored_forwards: Vec<StoredForPeerMessage>,
    metrics: ClientMetrics,
    log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
}

impl std::fmt::Debug for SimulationClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulationClient")
            .field("id", &self.id)
            .field("schedule", &self.schedule)
            .field("inbox", &self.inbox)
            .field("seen", &self.seen)
            .field("last_seen_by_peer", &self.last_seen_by_peer)
            .field("pending_online_broadcasts", &self.pending_online_broadcasts)
            .field("stored_forwards", &self.stored_forwards)
            .field("metrics", &self.metrics)
            .field("has_log_sink", &self.log_sink.is_some())
            .finish()
    }
}

impl SimulationClient {
    pub fn new(
        id: &str,
        schedule: Vec<bool>,
        log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
    ) -> Self {
        Self {
            id: id.to_string(),
            schedule,
            inbox: Vec::new(),
            seen: HashSet::new(),
            last_seen_by_peer: HashMap::new(),
            pending_online_broadcasts: Vec::new(),
            stored_forwards: Vec::new(),
            metrics: ClientMetrics::default(),
            log_sink,
        }
    }

    pub fn set_log_sink(&mut self, log_sink: Option<std::sync::Arc<dyn ClientLogSink>>) {
        self.log_sink = log_sink;
    }

    fn log_action(&self, step: usize, message: impl Into<String>) {
        if let Some(log_sink) = &self.log_sink {
            log_sink.log(ClientLogEvent {
                step,
                client_id: self.id.clone(),
                message: message.into(),
            });
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
            self.metrics.store_forwards_delivered =
                self.metrics.store_forwards_delivered.saturating_add(1);
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
                self.metrics.direct_deliveries = self.metrics.direct_deliveries.saturating_add(1);
            }
            DeliveryKind::Inbox => {
                context.metrics.inbox_deliveries =
                    context.metrics.inbox_deliveries.saturating_add(1);
                self.metrics.inbox_deliveries = self.metrics.inbox_deliveries.saturating_add(1);
            }
            DeliveryKind::Missed => {
                context.metrics_tracker.record_missed_message_delivery();
                self.metrics.missed_deliveries = self.metrics.missed_deliveries.saturating_add(1);
            }
        }
        let latency = self.record_delivery_metrics(&message, step, context);
        if matches!(delivery_kind, DeliveryKind::Missed) {
            self.log_action(
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
        let sender_keypair = context.keypairs.get(sender_id)?;
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
            &sender_keypair.signing_private_key_hex,
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
        let sender_keypair = context.keypairs.get(sender_id)?;
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
            &sender_keypair.signing_private_key_hex,
        )
        .ok()
    }

    fn decode_envelope_action(
        &self,
        envelope: &Envelope,
        recipient_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<IncomingEnvelopeAction> {
        let sender_keypair = context.keypairs.get(&envelope.header.sender_id)?;
        if envelope
            .header
            .verify_signature(envelope.version, &sender_keypair.signing_public_key_hex)
            .is_err()
        {
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
        self.metrics.store_forwards_stored = self.metrics.store_forwards_stored.saturating_add(1);
        self.log_action(
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
        self.log_action(
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
            self.log_action(
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

    fn metrics(&self) -> ClientMetrics {
        self.metrics.clone()
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
            let Some(sender_keypair) = context.keypairs.get(&message.sender) else {
                continue;
            };
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
                &sender_keypair.signing_private_key_hex,
            ) {
                Ok(envelope) => envelope,
                Err(_) => continue,
            };
            self.record_message_send(message, step, &envelope, context);
            self.metrics.messages_sent = self.metrics.messages_sent.saturating_add(1);
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
        self.log_action(
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
                self.log_action(
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
        let log_sink = self.log_sink.clone();
        let client_id = self.id.clone();
        for entry in self.stored_forwards.drain(..) {
            if online_set.contains(&entry.store_for_id) {
                let message_id = entry.envelope.header.message_id.0.clone();
                context.pending_forwarded_messages.insert(message_id);
                context.metrics.store_forwards_forwarded =
                    context.metrics.store_forwards_forwarded.saturating_add(1);
                context
                    .metrics_tracker
                    .record_store_forward_forwarded(&self.id);
                self.metrics.store_forwards_forwarded =
                    self.metrics.store_forwards_forwarded.saturating_add(1);
                if let Some(log_sink) = &log_sink {
                    log_sink.log(ClientLogEvent {
                        step: step + 1,
                        client_id: client_id.clone(),
                        message: format!(
                            "{} forwarded stored message to {}",
                            client_id, entry.store_for_id
                        ),
                    });
                }
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
