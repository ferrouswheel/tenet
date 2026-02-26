use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tenet::client::{ClientConfig, ClientEncryption, RelayClient, SyncEventOutcome};
use tenet::crypto::{derive_user_id_from_public_key, generate_keypair, StoredKeypair};
use tenet::identity::{
    create_identity, list_identities, resolve_identity, save_config, TenetConfig,
};
use tenet::protocol::MessageKind;
use tenet::storage::{db_path, IdentityRow, OutboxRow, PeerRow, Storage};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = env::args().collect::<Vec<String>>();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    // Extract --identity flag from anywhere in args
    let mut identity_flag: Option<String> = env::var("TENET_IDENTITY").ok();
    let mut filtered_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" => {
                i += 1;
                if i < args.len() {
                    identity_flag = Some(args[i].clone());
                }
            }
            _ => {
                filtered_args.push(args[i].clone());
            }
        }
        i += 1;
    }

    let command = filtered_args.get(1).cloned().unwrap_or_default();
    let command_args: Vec<String> = if filtered_args.len() > 2 {
        filtered_args[2..].to_vec()
    } else {
        Vec::new()
    };

    match command.as_str() {
        "init" => init_identity(identity_flag.as_deref()),
        "add-peer" => add_peer(&command_args, identity_flag.as_deref()),
        "send" => send_message(&command_args, identity_flag.as_deref()),
        "sync" => sync_messages(&command_args, identity_flag.as_deref()),
        "post" => post_message(&command_args, identity_flag.as_deref()),
        "receive-all" => receive_all(&command_args, identity_flag.as_deref()),
        "export-key" => export_key(&command_args, identity_flag.as_deref()),
        "import-key" => import_key(&command_args, identity_flag.as_deref()),
        "rotate-key" => rotate_identity(identity_flag.as_deref()),
        "list-peers" => list_peers_cmd(&command_args, identity_flag.as_deref()),
        "list-friends" => list_friends_cmd(&command_args, identity_flag.as_deref()),
        "peer" => peer_info_cmd(&command_args, identity_flag.as_deref()),
        "feed" => feed_cmd(&command_args, identity_flag.as_deref()),
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!(
        "tenet commands:\n\
         \n\
         init\n\
         add-peer <name> <public_key_hex>\n\
         send <peer_name> <message> [--relay <url>]\n\
         sync [--relay <url>]\n\
         post <message> [--relay <url>]\n\
         receive-all [--relay <url>]\n\
         export-key [--public|--private]\n\
         import-key <public_key_hex> <private_key_hex>\n\
         rotate-key\n\
         \n\
         list-peers [--sort recent|name|added] [--limit N] [--friends]\n\
         list-friends [--sort recent|name|added] [--limit N]\n\
         peer <peer_id_or_name> [--limit N]\n\
         feed [--limit N]\n\
         \n\
         Options:\n\
         --identity <id>   Select identity (short ID prefix)\n\
         --relay <url>     Relay URL (default: http://127.0.0.1:8080)\n\
         \n\
         Environment:\n\
         TENET_HOME       defaults to .tenet\n\
         TENET_RELAY_URL  provides a relay URL default for send/sync/post/receive-all\n\
         TENET_IDENTITY   select identity by short ID prefix"
    );
}

const DEFAULT_TTL_SECONDS: u64 = 3600;
const CLI_HPKE_INFO: &[u8] = b"tenet-cli";
const CLI_PAYLOAD_AAD: &[u8] = b"tenet-cli";
const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:8080";

fn build_relay_client(identity: StoredKeypair, relay_url: &str) -> RelayClient {
    let config = ClientConfig::new(
        relay_url.to_string(),
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: CLI_HPKE_INFO.to_vec(),
            payload_aad: CLI_PAYLOAD_AAD.to_vec(),
        },
    );
    RelayClient::new(identity, config)
}

fn data_dir() -> PathBuf {
    env::var("TENET_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".tenet"))
}

fn ensure_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Resolve identity and return (keypair, storage, stored_relay_url).
fn resolve_and_log(
    explicit_id: Option<&str>,
) -> Result<(StoredKeypair, Storage, Option<String>), Box<dyn Error>> {
    let dir = data_dir();
    let resolved = resolve_identity(&dir, explicit_id)?;

    if resolved.newly_created {
        eprintln!(
            "  identities: 1 available (newly created), using: {}",
            resolved.keypair.id
        );
    } else {
        eprintln!(
            "  identities: {} available, using: {}",
            resolved.total_identities, resolved.keypair.id
        );
    }

    Ok((
        resolved.keypair,
        resolved.storage,
        resolved.stored_relay_url,
    ))
}

/// Populate a relay client's peer registry from the SQLite peers table so
/// that signature verification works for incoming direct messages.
fn populate_peer_registry(
    client: &mut RelayClient,
    storage: &Storage,
) -> Result<(), Box<dyn Error>> {
    for peer in storage.list_peers()? {
        if let Some(enc_key) = peer.encryption_public_key {
            client.add_peer_with_encryption(peer.peer_id, peer.signing_public_key, enc_key);
        } else {
            client.add_peer(peer.peer_id, peer.signing_public_key);
        }
    }
    Ok(())
}

/// Format a Unix timestamp as a human-readable "X ago" string.
fn format_ago(ts: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if ts == 0 || now < ts {
        return "unknown".to_string();
    }
    let diff = now - ts;
    if diff < 60 {
        format!("{}s ago", diff)
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

/// Get the best display label for a peer (display_name or truncated peer_id).
fn peer_label(peer: &PeerRow) -> String {
    peer.display_name
        .clone()
        .unwrap_or_else(|| peer.peer_id.chars().take(12).collect())
}

/// Search for a peer by peer_id prefix or display_name (case-insensitive).
fn find_peer<'a>(peers: &'a [PeerRow], query: &str) -> Option<&'a PeerRow> {
    let lower = query.to_lowercase();
    // Exact peer_id match first
    if let Some(p) = peers.iter().find(|p| p.peer_id == query) {
        return Some(p);
    }
    // peer_id prefix match
    if let Some(p) = peers.iter().find(|p| p.peer_id.starts_with(query)) {
        return Some(p);
    }
    // display_name exact match (case-insensitive)
    if let Some(p) = peers
        .iter()
        .find(|p| p.display_name.as_deref().map(str::to_lowercase).as_deref() == Some(&lower))
    {
        return Some(p);
    }
    // display_name contains match
    peers.iter().find(|p| {
        p.display_name
            .as_deref()
            .map(|n| n.to_lowercase().contains(&lower))
            .unwrap_or(false)
    })
}

fn resolve_relay(relay_url_override: Option<String>, stored_relay: Option<String>) -> String {
    relay_url_override
        .or(stored_relay)
        .or_else(|| env::var("TENET_RELAY_URL").ok())
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string())
}

fn init_identity(explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let dir = data_dir();
    ensure_dir(&dir)?;

    // If an explicit_id is given, check if it already exists
    if let Some(id) = explicit_id {
        let identities = list_identities(&dir)?;
        if let Some(entry) = identities
            .iter()
            .find(|e| e.short_id == id || e.short_id.starts_with(id))
        {
            let db = db_path(&entry.path);
            let storage = Storage::open(&db)?;
            if let Some(row) = storage.get_identity()? {
                println!("identity already exists: {}", row.id);
                return Ok(());
            }
        }
    }

    let resolved = create_identity(&dir)?;
    println!("identity created: {}", resolved.keypair.id);
    eprintln!("  identities: {} available", resolved.total_identities);
    Ok(())
}

fn add_peer(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("add-peer requires <name> <public_key_hex>".into());
    }

    let (_keypair, storage, _) = resolve_and_log(explicit_id)?;

    let name = args[0].clone();
    let public_key_hex = args[1].clone();
    let public_key_bytes = hex::decode(&public_key_hex)?;
    let id = derive_user_id_from_public_key(&public_key_bytes);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    storage.insert_peer(&PeerRow {
        peer_id: id.clone(),
        display_name: Some(name.clone()),
        // Signing key is unknown when adding a peer by encryption key only;
        // an empty string causes signature verification to be skipped.
        signing_public_key: String::new(),
        encryption_public_key: Some(public_key_hex),
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
    })?;

    println!("peer saved: {} ({})", name, id);
    Ok(())
}

fn send_message(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override: Option<String> = None;
    let mut peer_name: Option<String> = None;
    let mut message_parts: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--relay" => {
                index += 1;
                if index >= args.len() {
                    return Err("--relay requires a URL".into());
                }
                relay_url_override = Some(args[index].clone());
            }
            value => {
                if peer_name.is_none() {
                    peer_name = Some(value.to_string());
                } else {
                    message_parts.push(value.to_string());
                }
            }
        }
        index += 1;
    }

    let (identity, storage, stored_relay) = resolve_and_log(explicit_id)?;
    let relay_url = resolve_relay(relay_url_override, stored_relay);

    let peer_name = peer_name.ok_or("send requires <peer_name>")?;
    let message = message_parts.join(" ");
    if message.trim().is_empty() {
        return Err("send requires a message".into());
    }

    let peers = storage.list_peers()?;
    let peer = peers
        .iter()
        .find(|p| p.display_name.as_deref() == Some(peer_name.as_str()))
        .ok_or("peer not found")?;

    let enc_key = peer
        .encryption_public_key
        .as_deref()
        .ok_or("peer has no encryption key")?;

    let client = build_relay_client(identity, &relay_url);
    let envelope = client.send_message(&peer.peer_id, enc_key, &message)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    storage.insert_outbox(&OutboxRow {
        message_id: envelope.header.message_id.0.clone(),
        envelope: serde_json::to_string(&envelope)?,
        sent_at: now,
        delivered: false,
    })?;

    println!(
        "sent message {} to {}",
        envelope.header.message_id.0,
        peer.display_name.as_deref().unwrap_or(&peer.peer_id)
    );
    Ok(())
}

fn sync_messages(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--relay" => {
                index += 1;
                if index >= args.len() {
                    return Err("--relay requires a URL".into());
                }
                relay_url_override = Some(args[index].clone());
            }
            _ => {}
        }
        index += 1;
    }

    let (identity, storage, stored_relay) = resolve_and_log(explicit_id)?;
    let relay_url = resolve_relay(relay_url_override, stored_relay);

    let mut client = build_relay_client(identity, &relay_url);
    populate_peer_registry(&mut client, &storage)?;

    let outcome = client.sync_inbox(None)?;
    if outcome.fetched == 0 {
        println!("no envelopes available");
        return Ok(());
    }

    for error in &outcome.errors {
        eprintln!("failed to process envelope: {error}");
    }

    let mut received = 0;
    for event in &outcome.events {
        if let SyncEventOutcome::Message(ref message) = event.outcome {
            if event.envelope.header.message_kind == MessageKind::Direct {
                println!("from {}: {}", message.sender_id, message.body);
                storage.store_received_envelope(&event.envelope, Some(&message.body))?;
                received += 1;
            }
        }
    }

    println!("synced {} direct message(s)", received);
    Ok(())
}

fn post_message(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override: Option<String> = None;
    let mut message_parts: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--relay" => {
                index += 1;
                if index >= args.len() {
                    return Err("--relay requires a URL".into());
                }
                relay_url_override = Some(args[index].clone());
            }
            value => {
                message_parts.push(value.to_string());
            }
        }
        index += 1;
    }

    let (identity, storage, stored_relay) = resolve_and_log(explicit_id)?;
    let relay_url = resolve_relay(relay_url_override, stored_relay);

    let message = message_parts.join(" ");
    if message.trim().is_empty() {
        return Err("post requires a message".into());
    }

    let mut client = build_relay_client(identity, &relay_url);
    populate_peer_registry(&mut client, &storage)?;

    let envelope = client.send_public_message(&message)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    storage.insert_outbox(&OutboxRow {
        message_id: envelope.header.message_id.0.clone(),
        envelope: serde_json::to_string(&envelope)?,
        sent_at: now,
        delivered: false,
    })?;

    println!("posted public message {}", envelope.header.message_id.0);
    Ok(())
}

fn receive_all(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--relay" => {
                index += 1;
                if index >= args.len() {
                    return Err("--relay requires a URL".into());
                }
                relay_url_override = Some(args[index].clone());
            }
            _ => {}
        }
        index += 1;
    }

    let (identity, storage, stored_relay) = resolve_and_log(explicit_id)?;
    let relay_url = resolve_relay(relay_url_override, stored_relay);

    let mut client = build_relay_client(identity, &relay_url);
    populate_peer_registry(&mut client, &storage)?;

    let outcome = client.sync_inbox(None)?;
    if outcome.fetched == 0 {
        println!("no envelopes available");
        return Ok(());
    }

    for error in &outcome.errors {
        eprintln!("failed to process envelope: {error}");
    }

    let mut received = 0;
    for event in &outcome.events {
        if let SyncEventOutcome::Message(ref message) = event.outcome {
            let kind_label = match event.envelope.header.message_kind {
                MessageKind::Public => "public",
                MessageKind::Direct => "direct",
                MessageKind::FriendGroup => "group",
                _ => "message",
            };
            println!(
                "[{}] from {}: {}",
                kind_label, message.sender_id, message.body
            );
            storage.store_received_envelope(&event.envelope, Some(&message.body))?;
            received += 1;
        }
    }

    println!("received {} message(s)", received);
    Ok(())
}

fn export_key(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    if args.len() > 1 {
        return Err("export-key accepts at most one flag".into());
    }

    let (identity, _, _) = resolve_and_log(explicit_id)?;
    match args.first().map(String::as_str) {
        None => println!("{}", serde_json::to_string_pretty(&identity)?),
        Some("--public") => println!("{}", identity.public_key_hex),
        Some("--private") => println!("{}", identity.private_key_hex),
        Some(flag) => return Err(format!("unknown flag: {}", flag).into()),
    }

    Ok(())
}

fn import_key(args: &[String], _explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("import-key requires <public_key_hex> <private_key_hex>".into());
    }

    let dir = data_dir();
    ensure_dir(&dir)?;

    let public_key_hex = args[0].clone();
    let private_key_hex = args[1].clone();
    let public_key_bytes = hex::decode(&public_key_hex)?;
    let private_key_bytes = hex::decode(&private_key_hex)?;

    validate_key_len(&public_key_bytes, "public")?;
    validate_key_len(&private_key_bytes, "private")?;

    let id = derive_user_id_from_public_key(&public_key_bytes);

    // Generate Ed25519 signing keys for the imported identity
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let keypair = StoredKeypair {
        id: id.clone(),
        public_key_hex,
        private_key_hex,
        signing_public_key_hex: hex::encode(verifying_key.to_bytes()),
        signing_private_key_hex: hex::encode(signing_key.to_bytes()),
    };

    let short_id: String = id.chars().take(12).collect();
    let identity_dir = tenet::identity::identities_dir(&dir).join(&short_id);
    fs::create_dir_all(&identity_dir)?;

    let db = db_path(&identity_dir);
    let storage = Storage::open(&db)?;

    if storage.get_identity()?.is_none() {
        let row = IdentityRow::from(&keypair);
        storage.insert_identity(&row)?;
    }

    let identities = list_identities(&dir)?;
    if identities.len() == 1 {
        let cfg = TenetConfig {
            default_identity: Some(short_id),
        };
        save_config(&dir, &cfg)?;
    }

    println!("identity imported: {}", keypair.id);
    println!("note: new Ed25519 signing keys were generated");
    eprintln!("  identities: {} available", identities.len());
    Ok(())
}

fn rotate_identity(explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let dir = data_dir();
    ensure_dir(&dir)?;

    let (old_keypair, _, _) = resolve_and_log(explicit_id)?;

    let new_keypair = generate_keypair();

    let new_short_id: String = new_keypair.id.chars().take(12).collect();
    let new_identity_dir = tenet::identity::identities_dir(&dir).join(&new_short_id);
    fs::create_dir_all(&new_identity_dir)?;

    let db = db_path(&new_identity_dir);
    let storage = Storage::open(&db)?;
    let row = IdentityRow::from(&new_keypair);
    storage.insert_identity(&row)?;

    let cfg = TenetConfig {
        default_identity: Some(new_short_id),
    };
    save_config(&dir, &cfg)?;

    println!(
        "identity rotated from {} to {} (share the new public key with peers)",
        old_keypair.id, new_keypair.id
    );
    Ok(())
}

/// List peers with optional sort and filter.
///
/// Usage: list-peers [--sort recent|name|added] [--limit N] [--friends]
fn list_peers_cmd(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut sort = "recent".to_string();
    let mut limit: usize = 20;
    let mut friends_only = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--sort" => {
                index += 1;
                if index >= args.len() {
                    return Err("--sort requires recent|name|added".into());
                }
                sort = args[index].clone();
            }
            "--limit" => {
                index += 1;
                if index >= args.len() {
                    return Err("--limit requires a number".into());
                }
                limit = args[index]
                    .parse()
                    .map_err(|_| "--limit requires a number")?;
            }
            "--friends" => {
                friends_only = true;
            }
            unknown => {
                return Err(format!("unknown option: {unknown}").into());
            }
        }
        index += 1;
    }

    let (keypair, storage, _) = resolve_and_log(explicit_id)?;

    let mut peers = storage.list_peers()?;

    if friends_only {
        peers.retain(|p| p.is_friend);
    }

    // Build a lookup of peer_id -> last message timestamp from conversations
    let convs = storage.list_conversations(&keypair.id)?;
    let conv_map: std::collections::HashMap<String, u64> = convs
        .iter()
        .map(|c| (c.peer_id.clone(), c.last_timestamp))
        .collect();

    match sort.as_str() {
        "recent" => {
            peers.sort_by(|a, b| {
                let ta = conv_map.get(&a.peer_id).copied().unwrap_or(0);
                let tb = conv_map.get(&b.peer_id).copied().unwrap_or(0);
                tb.cmp(&ta)
            });
        }
        "name" => {
            peers.sort_by(|a, b| {
                let na = peer_label(a).to_lowercase();
                let nb = peer_label(b).to_lowercase();
                na.cmp(&nb)
            });
        }
        "added" => {
            peers.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        }
        other => {
            return Err(format!("unknown sort: {other} (use recent|name|added)").into());
        }
    }

    peers.truncate(limit);

    if peers.is_empty() {
        println!("no peers found");
        return Ok(());
    }

    println!(
        "{:<20} {:<16} {:<8} {}",
        "NAME", "PEER ID", "FRIEND", "LAST MESSAGE"
    );
    println!("{}", "-".repeat(72));

    for peer in &peers {
        let name = peer_label(peer);
        let short_id: String = peer.peer_id.chars().take(14).collect();
        let friend_flag = if peer.is_friend { "yes" } else { "no" };
        let last_msg = if let Some(&ts) = conv_map.get(&peer.peer_id) {
            format_ago(ts)
        } else {
            "never".to_string()
        };
        println!(
            "{:<20} {:<16} {:<8} {}",
            truncate(&name, 20),
            short_id,
            friend_flag,
            last_msg
        );
    }

    Ok(())
}

/// Shortcut for list-peers --friends.
fn list_friends_cmd(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut new_args = vec!["--friends".to_string()];
    new_args.extend_from_slice(args);
    list_peers_cmd(&new_args, explicit_id)
}

/// Show detailed info for a single peer including recent messages and posts.
///
/// Usage: peer <peer_id_or_name> [--limit N]
fn peer_info_cmd(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut query: Option<String> = None;
    let mut limit: u32 = 10;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                index += 1;
                if index >= args.len() {
                    return Err("--limit requires a number".into());
                }
                limit = args[index]
                    .parse()
                    .map_err(|_| "--limit requires a number")?;
            }
            value if !value.starts_with("--") => {
                query = Some(value.to_string());
            }
            unknown => {
                return Err(format!("unknown option: {unknown}").into());
            }
        }
        index += 1;
    }

    let query = query.ok_or("peer requires <peer_id_or_name>")?;

    let (keypair, storage, _) = resolve_and_log(explicit_id)?;
    let peers = storage.list_peers()?;

    let peer = find_peer(&peers, &query).ok_or("peer not found")?;

    // Header
    let name = peer_label(peer);
    println!("Peer: {}", name);
    println!("  ID:      {}", peer.peer_id);
    println!("  Friend:  {}", if peer.is_friend { "yes" } else { "no" });
    if peer.is_muted {
        println!("  Muted:   yes");
    }
    if peer.is_blocked {
        println!("  Blocked: yes");
    }
    println!("  Added:   {}", format_ago(peer.added_at));
    if let Some(ts) = peer.last_seen_online {
        println!("  Last seen: {}", format_ago(ts));
    }

    // Profile
    if let Some(profile) = storage.get_profile(&peer.peer_id)? {
        if let Some(bio) = &profile.bio {
            if !bio.is_empty() {
                println!("  Bio:     {}", bio);
            }
        }
        if let Some(display) = &profile.display_name {
            if !display.is_empty() && display != &name {
                println!("  Profile name: {}", display);
            }
        }
    }

    // Recent direct messages
    let direct = storage.list_conversation_messages(&keypair.id, &peer.peer_id, None, limit)?;
    if !direct.is_empty() {
        println!("\nDirect messages ({}):", direct.len());
        // Messages come back newest-first; reverse to show oldest first
        for msg in direct.iter().rev() {
            let sender = if msg.sender_id == keypair.id {
                "you".to_string()
            } else {
                name.clone()
            };
            let body = msg.body.as_deref().unwrap_or("(no body)");
            println!("  [{}] {}: {}", format_ago(msg.timestamp), sender, body);
        }
    }

    // Recent public posts from this peer
    let all_public = storage.list_messages(Some("public"), None, None, 200)?;
    let posts: Vec<_> = all_public
        .iter()
        .filter(|m| m.sender_id == peer.peer_id)
        .take(limit as usize)
        .collect();

    if !posts.is_empty() {
        println!("\nPublic posts ({}):", posts.len());
        for post in posts.iter().rev() {
            let body = post.body.as_deref().unwrap_or("(no body)");
            println!("  [{}] {}", format_ago(post.timestamp), body);
        }
    }

    if direct.is_empty() && posts.is_empty() {
        println!("\nNo messages or posts stored for this peer.");
    }

    Ok(())
}

/// Show the public message feed.
///
/// Usage: feed [--limit N]
fn feed_cmd(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut limit: u32 = 20;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                index += 1;
                if index >= args.len() {
                    return Err("--limit requires a number".into());
                }
                limit = args[index]
                    .parse()
                    .map_err(|_| "--limit requires a number")?;
            }
            unknown => {
                return Err(format!("unknown option: {unknown}").into());
            }
        }
        index += 1;
    }

    let (_keypair, storage, _) = resolve_and_log(explicit_id)?;

    let messages = storage.list_messages(Some("public"), None, None, limit)?;

    if messages.is_empty() {
        println!("no public messages in feed (run sync or receive-all to fetch)");
        return Ok(());
    }

    // Build peer name lookup
    let peers = storage.list_peers()?;
    let peer_names: std::collections::HashMap<String, String> = peers
        .iter()
        .map(|p| (p.peer_id.clone(), peer_label(p)))
        .collect();

    println!("Public feed ({} messages):", messages.len());
    println!("{}", "-".repeat(60));

    // Messages come back newest-first; show newest first
    for msg in &messages {
        let sender_name = peer_names
            .get(&msg.sender_id)
            .cloned()
            .unwrap_or_else(|| msg.sender_id.chars().take(12).collect());
        let body = msg.body.as_deref().unwrap_or("(no body)");
        println!("[{}] {}: {}", format_ago(msg.timestamp), sender_name, body);
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

fn validate_key_len(bytes: &[u8], label: &str) -> Result<(), Box<dyn Error>> {
    if bytes.len() != 32 {
        return Err(format!("{label} key must be 32 bytes").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tenet::storage::PeerRow;

    fn make_peer(peer_id: &str, display_name: Option<&str>) -> PeerRow {
        PeerRow {
            peer_id: peer_id.to_string(),
            display_name: display_name.map(str::to_string),
            signing_public_key: String::new(),
            encryption_public_key: None,
            added_at: 0,
            is_friend: false,
            last_seen_online: None,
            online: false,
            last_profile_requested_at: None,
            last_profile_responded_at: None,
            is_blocked: false,
            is_muted: false,
            blocked_at: None,
            muted_at: None,
        }
    }

    // --- format_ago ---

    #[test]
    fn format_ago_zero_returns_unknown() {
        assert_eq!(format_ago(0), "unknown");
    }

    #[test]
    fn format_ago_seconds() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_ago(now - 30);
        assert!(result.ends_with("s ago"), "got: {result}");
    }

    #[test]
    fn format_ago_minutes() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_ago(now - 90); // 1.5 minutes
        assert!(result.ends_with("m ago"), "got: {result}");
    }

    #[test]
    fn format_ago_hours() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_ago(now - 7200); // 2 hours
        assert_eq!(result, "2h ago");
    }

    #[test]
    fn format_ago_days() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_ago(now - 3 * 86400); // 3 days
        assert_eq!(result, "3d ago");
    }

    // --- peer_label ---

    #[test]
    fn peer_label_uses_display_name_when_present() {
        let peer = make_peer("abc123def456xyz", Some("alice"));
        assert_eq!(peer_label(&peer), "alice");
    }

    #[test]
    fn peer_label_falls_back_to_truncated_id() {
        let peer = make_peer("abc123def456xyz", None);
        assert_eq!(peer_label(&peer), "abc123def456");
    }

    // --- truncate ---

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate("hello world", 8);
        assert!(result.ends_with('…'), "got: {result}");
        assert!(result.chars().count() <= 8, "got: {result}");
    }

    // --- find_peer ---

    #[test]
    fn find_peer_exact_id_match() {
        let peers = vec![
            make_peer("abc123", Some("alice")),
            make_peer("def456", Some("bob")),
        ];
        let found = find_peer(&peers, "abc123").unwrap();
        assert_eq!(found.peer_id, "abc123");
    }

    #[test]
    fn find_peer_prefix_id_match() {
        let peers = vec![
            make_peer("abc123def456", Some("alice")),
            make_peer("xyz789", Some("bob")),
        ];
        let found = find_peer(&peers, "abc123").unwrap();
        assert_eq!(found.peer_id, "abc123def456");
    }

    #[test]
    fn find_peer_display_name_exact() {
        let peers = vec![
            make_peer("abc123", Some("Alice")),
            make_peer("def456", Some("Bob")),
        ];
        let found = find_peer(&peers, "alice").unwrap(); // case-insensitive
        assert_eq!(found.peer_id, "abc123");
    }

    #[test]
    fn find_peer_display_name_substring() {
        let peers = vec![
            make_peer("abc123", Some("Alice Smith")),
            make_peer("def456", Some("Bob Jones")),
        ];
        let found = find_peer(&peers, "smith").unwrap();
        assert_eq!(found.peer_id, "abc123");
    }

    #[test]
    fn find_peer_no_match_returns_none() {
        let peers = vec![make_peer("abc123", Some("alice"))];
        assert!(find_peer(&peers, "zzzzzz").is_none());
    }

    // --- validate_key_len ---

    #[test]
    fn validate_key_len_correct() {
        assert!(validate_key_len(&[0u8; 32], "public").is_ok());
    }

    #[test]
    fn validate_key_len_wrong_size() {
        assert!(validate_key_len(&[0u8; 16], "public").is_err());
    }
}
