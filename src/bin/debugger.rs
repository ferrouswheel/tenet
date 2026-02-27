use std::collections::HashMap;
use std::error::Error;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use tenet::client::{ClientConfig, ClientEncryption, ClientMessage, MessageHandler, RelayClient};
use tenet::crypto::{generate_keypair, StoredKeypair};
use tenet::message_handler::StorageMessageHandler;
use tenet::protocol::{
    build_envelope_from_payload, build_meta_payload, Envelope, MessageKind, MetaMessage,
};
use tenet::storage::{GroupInviteRow, GroupMemberRow, GroupRow, OutboxRow, PeerRow, Storage};

const DEFAULT_TTL_SECONDS: u64 = 3600;
const HPKE_INFO: &[u8] = b"tenet-hpke";
const PAYLOAD_AAD: &[u8] = b"tenet-toy-ui";
const PROMPT: &str = "\x1b[1;34mtenet-debugger>\x1b[0m ";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

struct DebugPeer {
    name: String,
    client: RelayClient,
    keypair: StoredKeypair,
    storage: Storage,
    db_path: PathBuf,
    data_dir: PathBuf,
}

impl DebugPeer {
    fn id(&self) -> &str {
        self.client.id()
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let (peer_count, relay_url_override) = parse_args()?;

    // Temporary directory — deleted automatically on process exit.
    let temp_dir = tempfile::tempdir()?;

    // Start in-process relay (Tokio runtime kept alive until end of run()).
    let (relay_url, _rt) = match relay_url_override {
        Some(url) => (url, None),
        None => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let addr = start_in_process_relay(&rt)?;
            let url = format!("http://127.0.0.1:{}", addr.port());
            (url, Some(rt))
        }
    };

    let client_config = ClientConfig::new(
        relay_url.clone(),
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: HPKE_INFO.to_vec(),
            payload_aad: PAYLOAD_AAD.to_vec(),
        },
    );

    let mut peers = spawn_peers(peer_count, &client_config, temp_dir.path())?;
    let peer_directory = build_peer_directory(&peers);

    println!(
        "Debugger started with {} peers. Relay: {}",
        peers.len(),
        relay_url
    );
    print_help();

    let mut stdout = io::stdout();
    let mut editor = DefaultEditor::new()?;

    loop {
        let line = editor.readline(PROMPT);
        match line {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                editor.add_history_entry(input)?;

                if matches!(input, "quit" | "exit") {
                    println!("Goodbye!");
                    break;
                }

                if let Err(error) =
                    handle_command(input, &mut peers, &peer_directory, &relay_url, &mut stdout)
                {
                    eprintln!("error: {error}");
                }

                stdout.flush()?;
            }
            Err(ReadlineError::Interrupted) => {
                println!("(ctrl-c) type 'exit' to quit.");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye!");
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn parse_args() -> Result<(usize, Option<String>), Box<dyn Error>> {
    let mut args = std::env::args().skip(1).peekable();
    let mut peers = None;
    let mut relay_url = std::env::var("TENET_RELAY_URL").ok();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--peers" => {
                let value = args.next().ok_or("--peers requires a value")?;
                peers = Some(value.parse::<usize>()?);
            }
            "--relay" => {
                let value = args.next().ok_or("--relay requires a value")?;
                relay_url = Some(value);
            }
            _ => {
                if peers.is_none() {
                    peers = Some(arg.parse::<usize>()?);
                } else {
                    return Err(format!("unexpected argument: {arg}").into());
                }
            }
        }
    }

    let peer_count = peers.unwrap_or(3).max(1);
    Ok((peer_count, relay_url))
}

fn start_in_process_relay(rt: &tokio::runtime::Runtime) -> Result<SocketAddr, Box<dyn Error>> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tenet::relay::{app, RelayConfig, RelayState};

    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    rt.spawn(async move {
        let config = RelayConfig {
            ttl: Duration::from_secs(DEFAULT_TTL_SECONDS),
            max_messages: 10_000,
            max_bytes: 100 * 1024 * 1024,
            retry_backoff: vec![Duration::from_millis(10), Duration::from_millis(50)],
            peer_log_window: Duration::from_secs(60),
            peer_log_interval: Duration::ZERO,
            log_sink: Some(Arc::new(|_| {})), // silence relay logs
            pause_flag: Arc::new(AtomicBool::new(false)),
            qos: tenet::relay::RelayQosConfig::default(),
            blob_max_chunk_bytes: 512 * 1024,
            blob_daily_quota_bytes: 500 * 1024 * 1024,
        };
        let state = RelayState::new(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind relay");
        let addr = listener.local_addr().expect("local_addr");
        addr_tx.send(addr).ok();
        axum::serve(listener, app(state))
            .await
            .expect("relay serve");
    });

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "relay did not start within 5 seconds")?;
    Ok(addr)
}

fn spawn_peers(
    count: usize,
    client_config: &ClientConfig,
    base_dir: &Path,
) -> Result<Vec<DebugPeer>, Box<dyn Error>> {
    let mut peers = Vec::with_capacity(count);
    for index in 1..=count {
        let name = format!("peer-{index}");
        let data_dir = base_dir.join(&name);
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("tenet.db");
        let storage = Storage::open(&db_path)?;
        let keypair = generate_keypair();
        let client = RelayClient::new(keypair.clone(), client_config.clone());
        peers.push(DebugPeer {
            name,
            client,
            keypair,
            storage,
            db_path,
            data_dir,
        });
    }
    Ok(peers)
}

fn build_peer_directory(peers: &[DebugPeer]) -> HashMap<String, (String, String)> {
    peers
        .iter()
        .map(|peer| {
            (
                peer.client.id().to_string(),
                (peer.name.clone(), peer.client.public_key_hex().to_string()),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

fn print_help() {
    println!(
        "Commands:
  help

  -- Peer management --
  peers
  keys <peer>
  add-peer <peer> <target>
  add-all-peers
  remove-peer <peer> <target>
  online <peer>
  offline <peer>

  -- Messaging --
  send <from> <to> <message>
  broadcast <peer> <message>

  -- Sync --
  sync <peer|all> [limit]
  mesh-query <peer-a> <peer-b>

  -- Feeds & storage --
  feed <peer>
  public-feed <peer>
  messages <peer> [--kind direct|public|friend_group] [--limit N]
  conversations <peer>

  -- Groups --
  create-group <peer> <group-id> [member1 member2 ...]
  groups <peer>
  group-info <peer> <group-id>
  send-group <peer> <group-id> <message>
  add-member <peer> <group-id> <new-member>

  -- Relay --
  relay-status

  -- Inspection --
  inspect <peer>

  exit | quit
"
    );
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

fn handle_command(
    input: &str,
    peers: &mut [DebugPeer],
    peer_directory: &HashMap<String, (String, String)>,
    relay_url: &str,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    let command = parts.first().copied().unwrap_or("");

    match command {
        "help" => print_help(),

        // Peer management
        "peers" => list_peers(peers, stdout)?,
        "keys" => show_keys(peers, parts.get(1).copied(), stdout)?,
        "add-peer" => add_peer(peers, parts.get(1).copied(), parts.get(2).copied())?,
        "add-all-peers" => add_all_peers(peers)?,
        "remove-peer" => remove_peer(peers, parts.get(1).copied(), parts.get(2).copied())?,
        "online" => toggle_peer(peers, parts.get(1).copied(), true)?,
        "offline" => toggle_peer(peers, parts.get(1).copied(), false)?,

        // Messaging
        "send" => {
            let from = parts.get(1).copied();
            let to = parts.get(2).copied();
            let message = if parts.len() > 3 {
                parts[3..].join(" ")
            } else {
                String::new()
            };
            send_message(peers, from, to, message)?;
        }
        "broadcast" => {
            let peer = parts.get(1).copied();
            let message = if parts.len() > 2 {
                parts[2..].join(" ")
            } else {
                String::new()
            };
            broadcast_message(peers, peer, message)?;
        }

        // Sync
        "sync" => {
            let target = parts.get(1).copied();
            let limit = parts.get(2).map(|v| v.parse::<usize>()).transpose()?;
            sync_peers(peers, target, limit)?;
        }
        "mesh-query" => {
            mesh_query(peers, parts.get(1).copied(), parts.get(2).copied())?;
        }

        // Feeds & storage
        "feed" => show_feed(peers, peer_directory, parts.get(1).copied(), stdout)?,
        "public-feed" => show_public_feed(peers, peer_directory, parts.get(1).copied(), stdout)?,
        "messages" => show_messages(peers, &parts[1..], stdout)?,
        "conversations" => show_conversations(peers, parts.get(1).copied(), stdout)?,

        // Groups
        "create-group" => {
            let peer = parts.get(1).copied();
            let group_id = parts.get(2).copied();
            let members: Vec<&str> = parts[3.min(parts.len())..].to_vec();
            create_group(peers, peer, group_id, &members)?;
        }
        "groups" => show_groups(peers, parts.get(1).copied(), stdout)?,
        "group-info" => {
            show_group_info(peers, parts.get(1).copied(), parts.get(2).copied(), stdout)?
        }
        "send-group" => {
            let peer = parts.get(1).copied();
            let group_id = parts.get(2).copied();
            let message = if parts.len() > 3 {
                parts[3..].join(" ")
            } else {
                String::new()
            };
            send_group_message(peers, peer, group_id, message)?;
        }
        "add-member" => add_group_member(
            peers,
            parts.get(1).copied(),
            parts.get(2).copied(),
            parts.get(3).copied(),
        )?,

        // Relay
        "relay-status" => show_relay_status(relay_url, stdout)?,

        // Inspection
        "inspect" => inspect_peer(peers, parts.get(1).copied(), stdout)?,

        _ => println!("Unknown command: {command}. Type 'help' for commands."),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Peer management commands
// ---------------------------------------------------------------------------

fn list_peers(peers: &[DebugPeer], stdout: &mut io::Stdout) -> Result<(), Box<dyn Error>> {
    writeln!(stdout, "Peers:")?;
    for peer in peers {
        writeln!(
            stdout,
            "  {} (id: {}…) [{}]",
            peer.name,
            &peer.id()[..8],
            if peer.client.is_online() {
                "online"
            } else {
                "offline"
            }
        )?;
    }
    Ok(())
}

fn show_keys(
    peers: &[DebugPeer],
    name: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let name = name.ok_or("keys requires <peer>")?;
    let peer = find_peer(peers, name)?;
    writeln!(stdout, "Keys for {}:", peer.name)?;
    writeln!(stdout, "  id:             {}", peer.client.id())?;
    writeln!(
        stdout,
        "  signing key:    {}",
        peer.client.signing_public_key_hex()
    )?;
    writeln!(stdout, "  encryption key: {}", peer.client.public_key_hex())?;
    Ok(())
}

fn add_peer(
    peers: &mut [DebugPeer],
    peer_name: Option<&str>,
    target_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("add-peer requires <peer> <target>")?;
    let target_name = target_name.ok_or("add-peer requires <peer> <target>")?;

    let (target_id, target_signing_key, target_enc_key, target_display) = {
        let target = find_peer(peers, target_name)?;
        (
            target.client.id().to_string(),
            target.client.signing_public_key_hex().to_string(),
            target.client.public_key_hex().to_string(),
            target.name.clone(),
        )
    };

    let peer = find_peer_mut(peers, peer_name)?;
    peer.client.add_peer_with_encryption(
        target_id.clone(),
        target_signing_key.clone(),
        target_enc_key.clone(),
    );
    let row = PeerRow {
        peer_id: target_id,
        display_name: Some(target_display),
        signing_public_key: target_signing_key,
        encryption_public_key: Some(target_enc_key),
        added_at: now_secs(),
        is_friend: true,
        last_seen_online: None,
        online: false,
        last_profile_requested_at: None,
        last_profile_responded_at: None,
        is_blocked: false,
        is_muted: false,
        blocked_at: None,
        muted_at: None,
    };
    let _ = peer.storage.insert_peer(&row);

    println!(
        "registered {} in {}'s peer registry",
        target_name, peer_name
    );
    Ok(())
}

fn add_all_peers(peers: &mut [DebugPeer]) -> Result<(), Box<dyn Error>> {
    let info: Vec<(String, String, String, String)> = peers
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                p.client.id().to_string(),
                p.client.signing_public_key_hex().to_string(),
                p.client.public_key_hex().to_string(),
            )
        })
        .collect();

    let now = now_secs();
    for (i, peer) in peers.iter_mut().enumerate() {
        for (j, (tname, tid, tsk, tek)) in info.iter().enumerate() {
            if i == j {
                continue;
            }
            peer.client
                .add_peer_with_encryption(tid.clone(), tsk.clone(), tek.clone());
            let row = PeerRow {
                peer_id: tid.clone(),
                display_name: Some(tname.clone()),
                signing_public_key: tsk.clone(),
                encryption_public_key: Some(tek.clone()),
                added_at: now,
                is_friend: true,
                last_seen_online: None,
                online: false,
                last_profile_requested_at: None,
                last_profile_responded_at: None,
                is_blocked: false,
                is_muted: false,
                blocked_at: None,
                muted_at: None,
            };
            let _ = peer.storage.insert_peer(&row);
        }
    }

    println!("registered all {} peers with each other", peers.len());
    Ok(())
}

fn remove_peer(
    peers: &mut [DebugPeer],
    peer_name: Option<&str>,
    target_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("remove-peer requires <peer> <target>")?;
    let target_name = target_name.ok_or("remove-peer requires <peer> <target>")?;

    let target_id = {
        let target = find_peer(peers, target_name)?;
        target.client.id().to_string()
    };

    let peer = find_peer_mut(peers, peer_name)?;
    peer.client.remove_peer(&target_id);
    let _ = peer.storage.delete_peer(&target_id);

    println!("removed {} from {}'s peer registry", target_name, peer_name);
    Ok(())
}

fn toggle_peer(
    peers: &mut [DebugPeer],
    name: Option<&str>,
    online: bool,
) -> Result<(), Box<dyn Error>> {
    let name = name.ok_or("peer name required")?;
    let peer = find_peer_mut(peers, name)?;
    peer.client.set_online(online);
    println!(
        "{} is now {}",
        peer.name,
        if peer.client.is_online() {
            "online"
        } else {
            "offline"
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Messaging commands
// ---------------------------------------------------------------------------

fn send_message(
    peers: &mut [DebugPeer],
    from: Option<&str>,
    to: Option<&str>,
    message: String,
) -> Result<(), Box<dyn Error>> {
    let from = from.ok_or("send requires <from> <to> <message>")?;
    let to = to.ok_or("send requires <from> <to> <message>")?;
    if message.trim().is_empty() {
        return Err("send requires a message".into());
    }

    let (recipient_id, recipient_public_key_hex) = {
        let recipient = find_peer(peers, to)?;
        (
            recipient.client.id().to_string(),
            recipient.client.public_key_hex().to_string(),
        )
    };

    let sender = find_peer_mut(peers, from)?;
    if !sender.client.is_online() {
        println!("{} is offline; cannot send", sender.name);
        return Ok(());
    }

    let envelope =
        sender
            .client
            .send_message(&recipient_id, &recipient_public_key_hex, &message)?;

    if let Ok(envelope_json) = serde_json::to_string(&envelope) {
        let row = OutboxRow {
            message_id: envelope.header.message_id.0.clone(),
            envelope: envelope_json,
            sent_at: now_secs(),
            delivered: false,
        };
        let _ = sender.storage.insert_outbox(&row);
    }

    println!(
        "sent {} -> {} (id: {}…)",
        sender.name,
        to,
        &envelope.header.message_id.0[..8]
    );
    Ok(())
}

fn broadcast_message(
    peers: &mut [DebugPeer],
    peer_name: Option<&str>,
    message: String,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("broadcast requires <peer> <message>")?;
    if message.trim().is_empty() {
        return Err("broadcast requires a message".into());
    }

    let peer = find_peer_mut(peers, peer_name)?;
    if !peer.client.is_online() {
        println!("{} is offline; cannot broadcast", peer.name);
        return Ok(());
    }

    let envelope = peer.client.send_public_message(&message)?;

    if let Ok(envelope_json) = serde_json::to_string(&envelope) {
        let row = OutboxRow {
            message_id: envelope.header.message_id.0.clone(),
            envelope: envelope_json,
            sent_at: now_secs(),
            delivered: false,
        };
        let _ = peer.storage.insert_outbox(&row);
    }

    println!(
        "broadcast from {} (id: {}…)",
        peer_name,
        &envelope.header.message_id.0[..8]
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Wraps `StorageMessageHandler` and auto-accepts group invites, which is
/// appropriate for the automated testing / debugging context.
struct DebuggerMessageHandler {
    inner: StorageMessageHandler,
    my_peer_id: String,
    signing_private_key_hex: String,
    peer_name: String,
}

impl MessageHandler for DebuggerMessageHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) -> Vec<Envelope> {
        self.inner.on_message(envelope, message)
    }

    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        let mut outgoing = self.inner.on_meta(meta);
        // Auto-accept group invites (debugger-specific behaviour; web client prompts user).
        if let MetaMessage::GroupInvite {
            peer_id: inviter_id,
            group_id,
            ..
        } = meta
        {
            if let Ok(Some(invite)) = self.inner.storage_mut().find_group_invite(
                group_id,
                inviter_id,
                &self.my_peer_id,
                "incoming",
            ) {
                if invite.status == "pending" {
                    let _ = self
                        .inner
                        .storage_mut()
                        .update_group_invite_status(invite.id, "accepted");
                    let accept = MetaMessage::GroupInviteAccept {
                        peer_id: self.my_peer_id.clone(),
                        group_id: group_id.clone(),
                    };
                    if let Ok(payload) = build_meta_payload(&accept) {
                        let now = now_secs();
                        if let Ok(env) = build_envelope_from_payload(
                            self.my_peer_id.clone(),
                            inviter_id.clone(),
                            None,
                            None,
                            now,
                            DEFAULT_TTL_SECONDS,
                            MessageKind::Meta,
                            None,
                            None,
                            payload,
                            &self.signing_private_key_hex,
                        ) {
                            outgoing.push(env);
                        }
                    }
                    println!(
                        "{} auto-accepted invite to group '{}' from {}",
                        self.peer_name,
                        group_id,
                        &inviter_id[..8.min(inviter_id.len())]
                    );
                }
            }
        }
        outgoing
    }

    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) -> Vec<Envelope> {
        self.inner.on_raw_meta(envelope, body)
    }

    fn take_pending_group_keys(&mut self) -> Vec<(String, Vec<u8>)> {
        self.inner.take_pending_group_keys()
    }

    fn on_group_created(
        &mut self,
        group_id: &str,
        group_key: &[u8; 32],
        creator_id: &str,
        members: &[String],
    ) {
        self.inner
            .on_group_created(group_id, group_key, creator_id, members);
    }
}

fn sync_peers(
    peers: &mut [DebugPeer],
    target: Option<&str>,
    limit: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("sync requires <peer|all>")?;

    if target == "all" {
        for i in 0..peers.len() {
            if peers[i].client.is_online() {
                sync_one(peers, i, limit)?;
            }
        }
        return Ok(());
    }

    let index = peers
        .iter()
        .position(|p| p.name == target)
        .ok_or_else(|| format!("unknown peer: {target}"))?;

    if !peers[index].client.is_online() {
        println!("{} is offline; cannot sync", target);
        return Ok(());
    }
    sync_one(peers, index, limit)
}

fn sync_one(
    peers: &mut [DebugPeer],
    index: usize,
    limit: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    // Open a fresh storage connection for the handler (WAL mode allows concurrent access).
    let handler_storage = Storage::open(&peers[index].db_path)?;
    let inner = StorageMessageHandler::new_with_crypto(
        handler_storage,
        peers[index].keypair.clone(),
        HPKE_INFO.to_vec(),
        PAYLOAD_AAD.to_vec(),
    );
    peers[index]
        .client
        .set_handler(Box::new(DebuggerMessageHandler {
            inner,
            my_peer_id: peers[index].keypair.id.clone(),
            signing_private_key_hex: peers[index].keypair.signing_private_key_hex.clone(),
            peer_name: peers[index].name.clone(),
        }));

    let outcome = peers[index].client.sync_inbox(limit)?;

    peers[index].client.clear_handler();

    // After sync, load any newly received group keys into the in-memory GroupManager
    // so that send_group_message can use them. The handler persisted them to the db;
    // we reload from storage so we don't need to extract the handler.
    if let Ok(groups) = peers[index].storage.list_groups() {
        for row in groups {
            if peers[index].client.get_group(&row.group_id).is_none() {
                peers[index]
                    .client
                    .group_manager_mut()
                    .add_group_key(row.group_id, row.group_key);
            }
        }
    }

    if outcome.fetched == 0 {
        println!("{} inbox is empty", peers[index].name);
        return Ok(());
    }

    for error in &outcome.errors {
        println!("{} failed to process message: {error}", peers[index].name);
    }

    println!(
        "{} synced {} envelopes ({} decoded, {} errors)",
        peers[index].name,
        outcome.fetched,
        outcome.messages.len(),
        outcome.errors.len()
    );
    Ok(())
}

/// Perform an interactive four-phase mesh catch-up query from `peer_a` to `peer_b`.
///
/// Phase 1 — peer-a posts a `MessageRequest` to peer-b's relay inbox.
/// Phase 2 — peer-b syncs: receives `MessageRequest`, handler posts `MeshAvailable` to peer-a.
/// Phase 3 — peer-a syncs: receives `MeshAvailable`, handler posts `MeshRequest` to peer-b.
/// Phase 4 — peer-b syncs: receives `MeshRequest`, handler posts `MeshDelivery` to peer-a.
/// Phase 5 — peer-a syncs: receives `MeshDelivery`, stores newly-discovered public messages.
///
/// Both peers must be online. The operation is useful for testing public-message catch-up
/// after a period of disconnection.
fn mesh_query(
    peers: &mut [DebugPeer],
    peer_a_name: Option<&str>,
    peer_b_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let peer_a_name = peer_a_name.ok_or("mesh-query requires <peer-a> <peer-b>")?;
    let peer_b_name = peer_b_name.ok_or("mesh-query requires <peer-a> <peer-b>")?;
    if peer_a_name == peer_b_name {
        return Err("mesh-query requires two different peers".into());
    }

    let peer_a_index = peers
        .iter()
        .position(|p| p.name == peer_a_name)
        .ok_or_else(|| format!("unknown peer: {peer_a_name}"))?;
    let peer_b_index = peers
        .iter()
        .position(|p| p.name == peer_b_name)
        .ok_or_else(|| format!("unknown peer: {peer_b_name}"))?;

    if !peers[peer_a_index].client.is_online() {
        println!("{} is offline; cannot issue mesh query", peer_a_name);
        return Ok(());
    }
    if !peers[peer_b_index].client.is_online() {
        println!("{} is offline; cannot respond to mesh query", peer_b_name);
        return Ok(());
    }

    // --- Phase 1: peer-a sends MessageRequest to peer-b ---
    let peer_a_id = peers[peer_a_index].client.id().to_string();
    let peer_b_id = peers[peer_b_index].client.id().to_string();
    let signing_key = peers[peer_a_index]
        .client
        .signing_private_key_hex()
        .to_string();

    let meta = MetaMessage::MessageRequest {
        peer_id: peer_a_id.clone(),
        since_timestamp: 0, // request all stored public messages
    };
    let payload = build_meta_payload(&meta)?;
    let now = now_secs();
    let envelope = build_envelope_from_payload(
        peer_a_id,
        peer_b_id,
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Meta,
        None,
        None,
        payload,
        &signing_key,
    )?;
    peers[peer_a_index].client.post_envelope(&envelope)?;
    println!(
        "mesh-query: {} → {} (MessageRequest posted)",
        peer_a_name, peer_b_name
    );

    // --- Phase 2: peer-b receives MessageRequest, posts MeshAvailable ---
    println!(
        "mesh-query: syncing {} (expect MeshAvailable response)…",
        peer_b_name
    );
    sync_one(peers, peer_b_index, None)?;

    // --- Phase 3: peer-a receives MeshAvailable, posts MeshRequest ---
    println!(
        "mesh-query: syncing {} (expect MeshRequest response)…",
        peer_a_name
    );
    sync_one(peers, peer_a_index, None)?;

    // --- Phase 4: peer-b receives MeshRequest, posts MeshDelivery ---
    println!(
        "mesh-query: syncing {} (expect MeshDelivery response)…",
        peer_b_name
    );
    sync_one(peers, peer_b_index, None)?;

    // --- Phase 5: peer-a receives MeshDelivery, stores public messages ---
    println!(
        "mesh-query: syncing {} (storing delivered messages)…",
        peer_a_name
    );
    sync_one(peers, peer_a_index, None)?;

    println!("mesh-query: complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Feed & storage display commands
// ---------------------------------------------------------------------------

fn show_feed(
    peers: &[DebugPeer],
    peer_directory: &HashMap<String, (String, String)>,
    target: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("feed requires <peer>")?;
    let peer = find_peer(peers, target)?;

    writeln!(stdout, "Feed for {}:", peer.name)?;
    if peer.client.feed().is_empty() {
        writeln!(stdout, "  (empty)")?;
        return Ok(());
    }
    for msg in peer.client.feed() {
        let (from_name, _) = peer_directory_lookup(peer_directory, &msg.sender_id);
        writeln!(stdout, "  [{}] {} → {}", msg.timestamp, from_name, msg.body)?;
    }
    Ok(())
}

fn show_public_feed(
    peers: &[DebugPeer],
    peer_directory: &HashMap<String, (String, String)>,
    target: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("public-feed requires <peer>")?;
    let peer = find_peer(peers, target)?;

    writeln!(stdout, "Public feed for {}:", peer.name)?;
    let cache = peer.client.public_message_cache();
    if cache.is_empty() {
        writeln!(stdout, "  (empty)")?;
        return Ok(());
    }
    for envelope in cache {
        let (from_name, _) = peer_directory_lookup(peer_directory, &envelope.header.sender_id);
        writeln!(
            stdout,
            "  [{}] {} → {}",
            envelope.header.timestamp, from_name, envelope.payload.body
        )?;
    }
    Ok(())
}

fn show_messages(
    peers: &[DebugPeer],
    args: &[&str],
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let peer_name = args.first().copied().ok_or("messages requires <peer>")?;
    let peer = find_peer(peers, peer_name)?;

    let mut kind: Option<&str> = None;
    let mut limit: u32 = 20;

    let mut i = 1;
    while i < args.len() {
        match args[i] {
            "--kind" => {
                i += 1;
                kind = args.get(i).copied();
            }
            "--limit" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    limit = v.parse()?;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let rows = peer.storage.list_messages(kind, None, None, limit)?;
    if rows.is_empty() {
        writeln!(stdout, "No stored messages for {}", peer.name)?;
        return Ok(());
    }

    writeln!(stdout, "Messages for {} ({} shown):", peer.name, rows.len())?;
    for row in &rows {
        writeln!(
            stdout,
            "  [{}] {} {}…→{}… | {}",
            row.message_kind,
            row.timestamp,
            &row.sender_id[..8.min(row.sender_id.len())],
            &row.recipient_id[..8.min(row.recipient_id.len())],
            row.body.as_deref().unwrap_or("<no body>"),
        )?;
    }
    Ok(())
}

fn show_conversations(
    peers: &[DebugPeer],
    peer_name: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("conversations requires <peer>")?;
    let peer = find_peer(peers, peer_name)?;

    let convs = peer.storage.list_conversations(peer.client.id())?;
    if convs.is_empty() {
        writeln!(stdout, "No conversations for {}", peer.name)?;
        return Ok(());
    }

    writeln!(stdout, "Conversations for {}:", peer.name)?;
    for c in &convs {
        writeln!(
            stdout,
            "  {}… last:{} unread:{} | {}",
            &c.peer_id[..8.min(c.peer_id.len())],
            c.last_timestamp,
            c.unread_count,
            c.last_message.as_deref().unwrap_or("<none>"),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Group commands
// ---------------------------------------------------------------------------

fn create_group(
    peers: &mut [DebugPeer],
    owner_name: Option<&str>,
    group_id: Option<&str>,
    member_names: &[&str],
) -> Result<(), Box<dyn Error>> {
    let owner_name = owner_name.ok_or("create-group requires <peer> <group-id>")?;
    let group_id = group_id.ok_or("create-group requires <peer> <group-id>")?;

    // Collect non-owner member IDs.
    let owner_id = find_peer(peers, owner_name)?.client.id().to_string();
    let mut invitee_ids: Vec<String> = Vec::new();
    for &name in member_names {
        if name == owner_name {
            continue;
        }
        let id = find_peer(peers, name)?.client.id().to_string();
        if !invitee_ids.contains(&id) {
            invitee_ids.push(id);
        }
    }
    let mut all_member_ids = vec![owner_id.clone()];
    all_member_ids.extend_from_slice(&invitee_ids);

    // Create the group on the owner — generates the symmetric key in-memory and
    // persists the group row to storage (needed later for key distribution).
    let now = now_secs();
    {
        let owner = find_peer_mut(peers, owner_name)?;
        let info = owner
            .client
            .create_group(group_id.to_string(), all_member_ids.clone())?;
        // Persist the group and self-membership so the handler can find it during
        // subsequent syncs when it receives GroupInviteAccept messages.
        let _ = owner.storage.insert_group(&GroupRow {
            group_id: group_id.to_string(),
            group_key: info.group_key.to_vec(),
            creator_id: owner_id.clone(),
            created_at: now,
            key_version: 1,
        });
        let _ = owner.storage.insert_group_member(&GroupMemberRow {
            group_id: group_id.to_string(),
            peer_id: owner_id.clone(),
            joined_at: now,
        });
    }

    // Send a GroupInvite meta message via the relay for each non-owner member.
    for invitee_id in &invitee_ids {
        let meta = MetaMessage::GroupInvite {
            peer_id: owner_id.clone(),
            group_id: group_id.to_string(),
            group_name: None,
            message: None,
        };
        let payload = build_meta_payload(&meta)?;
        let owner = find_peer_mut(peers, owner_name)?;
        let signing_key = owner.client.signing_private_key_hex().to_string();
        let sender_id = owner.client.id().to_string();
        let envelope = build_envelope_from_payload(
            sender_id.clone(),
            invitee_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            None,
            payload,
            &signing_key,
        )?;
        owner.client.post_envelope(&envelope)?;
        // Record the outgoing invite row.
        let _ = owner.storage.insert_group_invite(&GroupInviteRow {
            id: 0,
            group_id: group_id.to_string(),
            from_peer_id: sender_id.clone(),
            to_peer_id: invitee_id.clone(),
            status: "pending".to_string(),
            message: None,
            direction: "outgoing".to_string(),
            created_at: now,
            updated_at: now,
        });
    }

    if invitee_ids.is_empty() {
        println!("created group '{}' (no invites needed)", group_id);
    } else {
        println!(
            "created group '{}' and sent {} invite(s) — sync invitees then sync {} to distribute key",
            group_id,
            invitee_ids.len(),
            owner_name
        );
    }
    Ok(())
}

fn show_groups(
    peers: &[DebugPeer],
    peer_name: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("groups requires <peer>")?;
    let peer = find_peer(peers, peer_name)?;

    let groups = peer.client.list_groups();
    if groups.is_empty() {
        writeln!(stdout, "{} is not a member of any groups", peer.name)?;
    } else {
        writeln!(stdout, "Groups for {}:", peer.name)?;
        for g_id in groups {
            writeln!(stdout, "  {}", g_id)?;
        }
    }
    Ok(())
}

fn show_group_info(
    peers: &[DebugPeer],
    peer_name: Option<&str>,
    group_id: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("group-info requires <peer> <group-id>")?;
    let group_id = group_id.ok_or("group-info requires <peer> <group-id>")?;
    let peer = find_peer(peers, peer_name)?;

    let group = peer
        .client
        .get_group(group_id)
        .ok_or_else(|| format!("group '{}' not found for {}", group_id, peer_name))?;

    writeln!(stdout, "Group '{}':", group_id)?;
    writeln!(stdout, "  key version: {}", group.key_version)?;
    writeln!(stdout, "  creator:     {}", group.creator_id)?;
    writeln!(stdout, "  members ({}):", group.members.len())?;
    let mut members: Vec<&str> = group.members.iter().map(|s| s.as_str()).collect();
    members.sort_unstable();
    for m in members {
        writeln!(stdout, "    {}…", &m[..8.min(m.len())])?;
    }
    Ok(())
}

fn send_group_message(
    peers: &mut [DebugPeer],
    peer_name: Option<&str>,
    group_id: Option<&str>,
    message: String,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("send-group requires <peer> <group-id> <message>")?;
    let group_id = group_id.ok_or("send-group requires <peer> <group-id> <message>")?;
    if message.trim().is_empty() {
        return Err("send-group requires a message".into());
    }

    let peer = find_peer_mut(peers, peer_name)?;
    if !peer.client.is_online() {
        println!("{} is offline; cannot send", peer.name);
        return Ok(());
    }

    let envelope = peer.client.send_group_message(group_id, &message)?;

    if let Ok(envelope_json) = serde_json::to_string(&envelope) {
        let row = OutboxRow {
            message_id: envelope.header.message_id.0.clone(),
            envelope: envelope_json,
            sent_at: now_secs(),
            delivered: false,
        };
        let _ = peer.storage.insert_outbox(&row);
    }

    println!(
        "sent group message {} → '{}' (id: {}…)",
        peer_name,
        group_id,
        &envelope.header.message_id.0[..8]
    );
    Ok(())
}

fn add_group_member(
    peers: &mut [DebugPeer],
    owner_name: Option<&str>,
    group_id: Option<&str>,
    new_member_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let owner_name = owner_name.ok_or("add-member requires <peer> <group-id> <new-member>")?;
    let group_id = group_id.ok_or("add-member requires <peer> <group-id> <new-member>")?;
    let new_member_name =
        new_member_name.ok_or("add-member requires <peer> <group-id> <new-member>")?;

    let new_member_id = find_peer(peers, new_member_name)?.client.id().to_string();

    // Add to the owner's in-memory group manager.
    {
        let owner = find_peer_mut(peers, owner_name)?;
        owner
            .client
            .group_manager_mut()
            .add_member(group_id, &new_member_id)?;
    }

    // Send a GroupInvite via the relay (real protocol flow).
    let now = now_secs();
    let meta = MetaMessage::GroupInvite {
        peer_id: find_peer(peers, owner_name)?.client.id().to_string(),
        group_id: group_id.to_string(),
        group_name: None,
        message: None,
    };
    let payload = build_meta_payload(&meta)?;
    let owner = find_peer_mut(peers, owner_name)?;
    let signing_key = owner.client.signing_private_key_hex().to_string();
    let sender_id = owner.client.id().to_string();
    let envelope = build_envelope_from_payload(
        sender_id.clone(),
        new_member_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Meta,
        None,
        None,
        payload,
        &signing_key,
    )?;
    owner.client.post_envelope(&envelope)?;
    let _ = owner.storage.insert_group_invite(&GroupInviteRow {
        id: 0,
        group_id: group_id.to_string(),
        from_peer_id: sender_id.clone(),
        to_peer_id: new_member_id.clone(),
        status: "pending".to_string(),
        message: None,
        direction: "outgoing".to_string(),
        created_at: now,
        updated_at: now,
    });

    println!(
        "invited {} to group '{}' — sync {} then sync {} to distribute key",
        new_member_name, group_id, new_member_name, owner_name
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Relay status
// ---------------------------------------------------------------------------

fn show_relay_status(relay_url: &str, stdout: &mut io::Stdout) -> Result<(), Box<dyn Error>> {
    let stats_url = format!("{}/debug/stats", relay_url.trim_end_matches('/'));
    match ureq::get(&stats_url).call() {
        Ok(response) => {
            let value: serde_json::Value = response.into_json()?;
            writeln!(stdout, "Relay: {}", relay_url)?;
            if let Some(total) = value.get("total").and_then(|v| v.as_u64()) {
                writeln!(stdout, "  queued messages: {}", total)?;
            }
            if let Some(n) = value.get("peer_count").and_then(|v| v.as_u64()) {
                writeln!(stdout, "  known peers:     {}", n)?;
            }
            if let Some(queues) = value.get("queues").and_then(|v| v.as_object()) {
                if !queues.is_empty() {
                    writeln!(stdout, "  per-inbox:")?;
                    let mut entries: Vec<(&str, u64)> = queues
                        .iter()
                        .filter_map(|(k, v)| v.as_u64().map(|n| (k.as_str(), n)))
                        .filter(|(_, n)| *n > 0)
                        .collect();
                    entries.sort_by(|a, b| b.1.cmp(&a.1));
                    for (id, count) in entries {
                        writeln!(
                            stdout,
                            "    {}… : {} message(s)",
                            &id[..8.min(id.len())],
                            count
                        )?;
                    }
                }
            }
        }
        Err(e) => writeln!(stdout, "relay unreachable: {e}")?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Peer inspection
// ---------------------------------------------------------------------------

fn inspect_peer(
    peers: &[DebugPeer],
    peer_name: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let peer_name = peer_name.ok_or("inspect requires <peer>")?;
    let peer = find_peer(peers, peer_name)?;

    let stored_count = peer
        .storage
        .list_messages(None, None, None, 100_000)
        .map(|v| v.len())
        .unwrap_or(0);

    writeln!(stdout, "Peer: {}", peer.name)?;
    writeln!(stdout, "  id:              {}", peer.client.id())?;
    writeln!(stdout, "  online:          {}", peer.client.is_online())?;
    writeln!(stdout, "  feed size:       {}", peer.client.feed().len())?;
    writeln!(
        stdout,
        "  public cache:    {}",
        peer.client.public_message_cache().len()
    )?;
    writeln!(
        stdout,
        "  groups:          {}",
        peer.client.list_groups().len()
    )?;
    writeln!(
        stdout,
        "  known peers:     {}",
        peer.client.list_peers().len()
    )?;
    writeln!(stdout, "  stored messages: {}", stored_count)?;
    writeln!(stdout, "  data dir:        {}", peer.data_dir.display())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn find_peer<'a>(peers: &'a [DebugPeer], name: &str) -> Result<&'a DebugPeer, Box<dyn Error>> {
    peers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn find_peer_mut<'a>(
    peers: &'a mut [DebugPeer],
    name: &str,
) -> Result<&'a mut DebugPeer, Box<dyn Error>> {
    peers
        .iter_mut()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn peer_directory_lookup(
    peer_directory: &HashMap<String, (String, String)>,
    sender_id: &str,
) -> (String, String) {
    peer_directory.get(sender_id).cloned().unwrap_or_else(|| {
        (
            format!("{}…", &sender_id[..8.min(sender_id.len())]),
            String::new(),
        )
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
