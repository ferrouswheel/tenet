use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::{derive_user_id_from_public_key, generate_keypair, StoredKeypair};
use tenet::identity::{
    create_identity, list_identities, resolve_identity, save_config, TenetConfig,
};
use tenet::storage::{db_path, IdentityRow};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Peer {
    name: String,
    id: String,
    public_key_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReceivedMessage {
    sender_id: String,
    timestamp: u64,
    message: String,
}

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
        if args[i] == "--identity" {
            i += 1;
            if i < args.len() {
                identity_flag = Some(args[i].clone());
            }
        } else {
            filtered_args.push(args[i].clone());
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
        "export-key" => export_key(&command_args, identity_flag.as_deref()),
        "import-key" => import_key(&command_args, identity_flag.as_deref()),
        "rotate-key" => rotate_identity(identity_flag.as_deref()),
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
         export-key [--public|--private]\n\
         import-key <public_key_hex> <private_key_hex>\n\
         rotate-key\n\
         \n\
         Options:\n\
         --identity <id>   Select identity (short ID prefix)\n\
         \n\
         Environment:\n\
         TENET_HOME       defaults to .tenet\n\
         TENET_RELAY_URL  provides a relay URL default for send/sync\n\
         TENET_IDENTITY   select identity by short ID prefix"
    );
}

const DEFAULT_TTL_SECONDS: u64 = 3600;
const CLI_HPKE_INFO: &[u8] = b"tenet-cli";
const CLI_PAYLOAD_AAD: &[u8] = b"tenet-cli";

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

/// Resolve identity and return (keypair, identity_dir, stored_relay_url).
/// Also logs identity selection info to stderr.
fn resolve_and_log(
    explicit_id: Option<&str>,
) -> Result<(StoredKeypair, PathBuf, Option<String>), Box<dyn Error>> {
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
        resolved.identity_dir,
        resolved.stored_relay_url,
    ))
}

fn peers_path(identity_dir: &Path) -> PathBuf {
    identity_dir.join("peers.json")
}

fn outbox_path(identity_dir: &Path) -> PathBuf {
    identity_dir.join("outbox.jsonl")
}

fn inbox_path(identity_dir: &Path) -> PathBuf {
    identity_dir.join("inbox.jsonl")
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
            let storage = tenet::storage::Storage::open(&db)?;
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

fn load_peers(identity_dir: &Path) -> Result<Vec<Peer>, Box<dyn Error>> {
    let path = peers_path(identity_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

fn save_peers(identity_dir: &Path, peers: &[Peer]) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string_pretty(peers)?;
    fs::write(peers_path(identity_dir), json)?;
    Ok(())
}

fn add_peer(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("add-peer requires <name> <public_key_hex>".into());
    }

    let (_keypair, identity_dir, _) = resolve_and_log(explicit_id)?;

    let name = args[0].clone();
    let public_key_hex = args[1].clone();
    let public_key_bytes = hex::decode(&public_key_hex)?;
    let id = derive_user_id_from_public_key(&public_key_bytes);

    let mut peers = load_peers(&identity_dir)?;
    if let Some(existing) = peers.iter_mut().find(|peer| peer.name == name) {
        existing.public_key_hex = public_key_hex;
        existing.id = id.clone();
    } else {
        peers.push(Peer {
            name: name.clone(),
            id: id.clone(),
            public_key_hex,
        });
    }

    save_peers(&identity_dir, &peers)?;
    println!("peer saved: {} ({})", name, id);
    Ok(())
}

fn send_message(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override = env::var("TENET_RELAY_URL").ok();
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

    let (identity, identity_dir, stored_relay) = resolve_and_log(explicit_id)?;

    // Priority: --relay > TENET_RELAY_URL > stored relay in identity DB
    let relay_url = relay_url_override
        .or(stored_relay)
        .ok_or("relay URL required (use --relay, TENET_RELAY_URL, or store one with init)")?;

    let peer_name = peer_name.ok_or("send requires <peer_name>")?;
    let message = message_parts.join(" ");
    if message.trim().is_empty() {
        return Err("send requires a message".into());
    }

    let peers = load_peers(&identity_dir)?;
    let peer = peers
        .iter()
        .find(|peer| peer.name == peer_name)
        .ok_or("peer not found")?;

    let client = build_relay_client(identity, &relay_url);
    let envelope = client.send_message(&peer.id, &peer.public_key_hex, &message)?;
    append_json_line(&identity_dir, outbox_path(&identity_dir), &envelope)?;

    println!(
        "sent message {} to {}",
        envelope.header.message_id.0, peer.name
    );
    Ok(())
}

fn sync_messages(args: &[String], explicit_id: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut relay_url_override = env::var("TENET_RELAY_URL").ok();
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

    let (identity, identity_dir, stored_relay) = resolve_and_log(explicit_id)?;

    // Priority: --relay > TENET_RELAY_URL > stored relay in identity DB
    let relay_url = relay_url_override
        .or(stored_relay)
        .ok_or("relay URL required (use --relay, TENET_RELAY_URL, or store one with init)")?;

    let mut client = build_relay_client(identity, &relay_url);
    let outcome = client.sync_inbox(None)?;
    if outcome.fetched == 0 {
        println!("no envelopes available");
        return Ok(());
    }

    for error in &outcome.errors {
        eprintln!("failed to decrypt envelope: {error}");
    }

    let mut received = 0;
    for message in outcome.messages {
        println!("from {}: {}", message.sender_id, message.body);
        let record = ReceivedMessage {
            sender_id: message.sender_id,
            timestamp: message.timestamp,
            message: message.body,
        };
        append_json_line(&identity_dir, inbox_path(&identity_dir), &record)?;
        received += 1;
    }

    println!("synced {} envelopes", received);
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

    // Create identity directory and store in the new multi-identity layout
    let short_id: String = id.chars().take(12).collect();
    let identity_dir = tenet::identity::identities_dir(&dir).join(&short_id);
    fs::create_dir_all(&identity_dir)?;

    let db = db_path(&identity_dir);
    let storage = tenet::storage::Storage::open(&db)?;

    if storage.get_identity()?.is_none() {
        let row = IdentityRow::from(&keypair);
        storage.insert_identity(&row)?;
    }

    // If this is the only identity, set as default
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

    let (old_keypair, identity_dir, _) = resolve_and_log(explicit_id)?;

    // Generate a new keypair
    let new_keypair = generate_keypair();

    // Store the new keypair in a new identity directory
    let new_short_id: String = new_keypair.id.chars().take(12).collect();
    let new_identity_dir = tenet::identity::identities_dir(&dir).join(&new_short_id);
    fs::create_dir_all(&new_identity_dir)?;

    let db = db_path(&new_identity_dir);
    let storage = tenet::storage::Storage::open(&db)?;
    let row = IdentityRow::from(&new_keypair);
    storage.insert_identity(&row)?;

    // Copy peers to the new identity directory
    let old_peers = peers_path(&identity_dir);
    if old_peers.exists() {
        fs::copy(&old_peers, peers_path(&new_identity_dir))?;
    }

    // Update default to the new identity
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

fn append_json_line<T: Serialize>(
    identity_dir: &Path,
    path: PathBuf,
    value: &T,
) -> Result<(), Box<dyn Error>> {
    ensure_dir(identity_dir)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let json = serde_json::to_string(value)?;
    writeln!(file, "{}", json)?;
    Ok(())
}

fn validate_key_len(bytes: &[u8], label: &str) -> Result<(), Box<dyn Error>> {
    if bytes.len() != 32 {
        return Err(format!("{label} key must be 32 bytes").into());
    }
    Ok(())
}
