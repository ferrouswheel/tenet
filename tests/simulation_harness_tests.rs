use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use tenet_crypto::relay::{app, RelayConfig, RelayState};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    sender_id: String,
    recipient_id: String,
    timestamp: u64,
    content_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Payload {
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    message_id: String,
    header: Header,
    payload: Payload,
}

#[derive(Debug, Clone)]
struct SimMessage {
    id: String,
    sender: String,
    recipient: String,
    body: String,
}

#[derive(Debug, Clone)]
struct PlannedSend {
    step: usize,
    message: SimMessage,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    schedule: Vec<bool>,
    inbox: Vec<SimMessage>,
    seen: HashSet<String>,
}

impl Node {
    fn new(id: &str, schedule: Vec<bool>) -> Self {
        Self {
            id: id.to_string(),
            schedule,
            inbox: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn online_at(&self, step: usize) -> bool {
        self.schedule.get(step).copied().unwrap_or(false)
    }

    fn receive(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.inbox.push(message);
            return true;
        }
        false
    }
}

struct SimulationHarness {
    relay_base_url: String,
    nodes: HashMap<String, Node>,
    direct_links: HashSet<(String, String)>,
    direct_enabled: bool,
}

impl SimulationHarness {
    fn new(
        relay_base_url: String,
        nodes: Vec<Node>,
        direct_links: HashSet<(String, String)>,
        direct_enabled: bool,
    ) -> Self {
        Self {
            relay_base_url,
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            direct_links,
            direct_enabled,
        }
    }

    async fn run(&mut self, steps: usize, plan: Vec<PlannedSend>) {
        let mut sends_by_step: HashMap<usize, Vec<SimMessage>> = HashMap::new();
        for item in plan {
            sends_by_step
                .entry(item.step)
                .or_default()
                .push(item.message);
        }

        for step in 0..steps {
            if let Some(messages) = sends_by_step.get(&step) {
                for message in messages {
                    if self
                        .nodes
                        .get(&message.sender)
                        .is_some_and(|node| node.online_at(step))
                    {
                        self.route_message(step, message).await;
                    }
                }
            }

            let online_ids: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.online_at(step))
                .map(|(id, _)| id.clone())
                .collect();

            for node_id in online_ids {
                let envelopes = fetch_inbox(&self.relay_base_url, &node_id)
                    .await
                    .unwrap_or_default();
                if let Some(node) = self.nodes.get_mut(&node_id) {
                    for envelope in envelopes {
                        let message = SimMessage {
                            id: envelope.message_id,
                            sender: envelope.header.sender_id,
                            recipient: envelope.header.recipient_id,
                            body: envelope.payload.body,
                        };
                        node.receive(message);
                    }
                }
            }
        }
    }

    fn inbox(&self, node_id: &str) -> Vec<SimMessage> {
        self.nodes
            .get(node_id)
            .map(|node| node.inbox.clone())
            .unwrap_or_default()
    }

    async fn route_message(&mut self, step: usize, message: &SimMessage) {
        if self.direct_enabled
            && self
                .direct_links
                .contains(&(message.sender.clone(), message.recipient.clone()))
        {
            let recipient_online = self
                .nodes
                .get(&message.recipient)
                .is_some_and(|node| node.online_at(step));
            if recipient_online {
                if let Some(node) = self.nodes.get_mut(&message.recipient) {
                    node.receive(message.clone());
                }
            }
        }

        let envelope = Envelope {
            message_id: message.id.clone(),
            header: Header {
                sender_id: message.sender.clone(),
                recipient_id: message.recipient.clone(),
                timestamp: step as u64,
                content_type: "text/plain".to_string(),
            },
            payload: Payload {
                body: message.body.clone(),
            },
        };
        let _ = post_envelope(&self.relay_base_url, &envelope).await;
    }
}

async fn start_relay(config: RelayConfig) -> (String, oneshot::Sender<()>) {
    let state = RelayState::new(config);
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

    (format!("http://{}", addr), shutdown_tx)
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

#[tokio::test]
async fn simulation_harness_routes_relay_and_direct_with_dedup() {
    let (base_url, shutdown_tx) = start_relay(RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
    })
    .await;

    let nodes = vec![
        Node::new("node-a", vec![true, true, true, true, false, false]),
        Node::new("node-b", vec![false, false, true, true, true, true]),
        Node::new("node-c", vec![true, false, true, false, true, true]),
    ];

    let direct_links = HashSet::from([(
        "node-c".to_string(),
        "node-b".to_string(),
    )]);

    let mut harness = SimulationHarness::new(base_url, nodes, direct_links, true);

    let message_a = SimMessage {
        id: "message-a".to_string(),
        sender: "node-a".to_string(),
        recipient: "node-b".to_string(),
        body: "relay me".to_string(),
    };

    let message_c = SimMessage {
        id: "message-c".to_string(),
        sender: "node-c".to_string(),
        recipient: "node-b".to_string(),
        body: "direct and relay".to_string(),
    };

    let plan = vec![
        PlannedSend {
            step: 0,
            message: message_a.clone(),
        },
        PlannedSend {
            step: 1,
            message: message_a.clone(),
        },
        PlannedSend {
            step: 2,
            message: message_a.clone(),
        },
        PlannedSend {
            step: 2,
            message: message_c.clone(),
        },
        PlannedSend {
            step: 3,
            message: message_c.clone(),
        },
    ];

    harness.run(6, plan).await;
    shutdown_tx.send(()).ok();

    let inbox = harness.inbox("node-b");
    let mut ids: HashSet<String> = inbox.iter().map(|message| message.id.clone()).collect();

    assert_eq!(inbox.len(), 2);
    assert!(ids.remove("message-a"));
    assert!(ids.remove("message-c"));
    assert!(ids.is_empty());
}
