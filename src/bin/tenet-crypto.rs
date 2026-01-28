use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::{
    derive_user_id_from_public_key, generate_keypair, load_keypair, rotate_keypair, store_keypair,
    KeyRotation, StoredKeypair,
};

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
    let mut args = env::args().collect::<Vec<String>>();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    let command = args[1].clone();
    let command_args = args.split_off(2);

    match command.as_str() {
        "init" => init_identity(),
        "add-peer" => add_peer(&command_args),
        "send" => send_message(&command_args),
        "sync" => sync_messages(&command_args),
        "export-key" => export_key(&command_args),
        "import-key" => import_key(&command_args),
        "rotate-key" => rotate_identity(),
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
         Environment:\n\
         TENET_HOME defaults to .tenet\n\
         TENET_RELAY_URL provides a relay URL default for send/sync"
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

fn identity_path() -> PathBuf {
    data_dir().join("identity.json")
}

fn peers_path() -> PathBuf {
    data_dir().join("peers.json")
}

fn outbox_path() -> PathBuf {
    data_dir().join("outbox.jsonl")
}

fn inbox_path() -> PathBuf {
    data_dir().join("inbox.jsonl")
}

fn init_identity() -> Result<(), Box<dyn Error>> {
    let dir = data_dir();
    ensure_dir(&dir)?;
    let identity_file = identity_path();

    if identity_file.exists() {
        let identity = load_identity()?;
        println!("identity already exists: {}", identity.id);
        return Ok(());
    }

    let identity = generate_keypair();
    store_keypair(&identity_file, &identity)?;
    println!("identity created: {}", identity.id);
    Ok(())
}

fn load_identity() -> Result<StoredKeypair, Box<dyn Error>> {
    Ok(load_keypair(&identity_path())?)
}

fn load_peers() -> Result<Vec<Peer>, Box<dyn Error>> {
    let path = peers_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

fn save_peers(peers: &[Peer]) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string_pretty(peers)?;
    fs::write(peers_path(), json)?;
    Ok(())
}

fn add_peer(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("add-peer requires <name> <public_key_hex>".into());
    }

    ensure_dir(&data_dir())?;

    let name = args[0].clone();
    let public_key_hex = args[1].clone();
    let public_key_bytes = hex::decode(&public_key_hex)?;
    let id = derive_user_id_from_public_key(&public_key_bytes);

    let mut peers = load_peers()?;
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

    save_peers(&peers)?;
    println!("peer saved: {} ({})", name, id);
    Ok(())
}

fn send_message(args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut relay_url = env::var("TENET_RELAY_URL").ok();
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
                relay_url = Some(args[index].clone());
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

    let relay_url = relay_url.ok_or("relay URL required (use --relay or TENET_RELAY_URL)")?;
    let peer_name = peer_name.ok_or("send requires <peer_name>")?;
    let message = message_parts.join(" ");
    if message.trim().is_empty() {
        return Err("send requires a message".into());
    }

    let identity = load_identity()?;
    let peers = load_peers()?;
    let peer = peers
        .iter()
        .find(|peer| peer.name == peer_name)
        .ok_or("peer not found")?;

    let client = build_relay_client(identity, &relay_url);
    let envelope = client.send_message(&peer.id, &peer.public_key_hex, &message)?;
    append_json_line(outbox_path(), &envelope)?;

    println!(
        "sent message {} to {}",
        envelope.header.message_id.0, peer.name
    );
    Ok(())
}

fn sync_messages(args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut relay_url = env::var("TENET_RELAY_URL").ok();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--relay" => {
                index += 1;
                if index >= args.len() {
                    return Err("--relay requires a URL".into());
                }
                relay_url = Some(args[index].clone());
            }
            _ => {}
        }
        index += 1;
    }

    let relay_url = relay_url.ok_or("relay URL required (use --relay or TENET_RELAY_URL)")?;
    let identity = load_identity()?;
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
        append_json_line(inbox_path(), &record)?;
        received += 1;
    }

    println!("synced {} envelopes", received);
    Ok(())
}

fn export_key(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() > 1 {
        return Err("export-key accepts at most one flag".into());
    }

    let identity = load_identity()?;
    match args.first().map(String::as_str) {
        None => println!("{}", serde_json::to_string_pretty(&identity)?),
        Some("--public") => println!("{}", identity.public_key_hex),
        Some("--private") => println!("{}", identity.private_key_hex),
        Some(flag) => return Err(format!("unknown flag: {}", flag).into()),
    }

    Ok(())
}

fn import_key(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("import-key requires <public_key_hex> <private_key_hex>".into());
    }

    ensure_dir(&data_dir())?;

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

    let identity = StoredKeypair {
        id,
        public_key_hex,
        private_key_hex,
        signing_public_key_hex: hex::encode(verifying_key.to_bytes()),
        signing_private_key_hex: hex::encode(signing_key.to_bytes()),
    };

    store_keypair(&identity_path(), &identity)?;
    println!("identity imported: {}", identity.id);
    println!("note: new Ed25519 signing keys were generated");
    Ok(())
}

fn rotate_identity() -> Result<(), Box<dyn Error>> {
    ensure_dir(&data_dir())?;
    let KeyRotation {
        previous_id,
        new_id,
    } = rotate_keypair(&identity_path())?;
    println!(
        "identity rotated from {} to {} (share the new public key with peers)",
        previous_id, new_id
    );
    Ok(())
}

fn append_json_line<T: Serialize>(path: PathBuf, value: &T) -> Result<(), Box<dyn Error>> {
    ensure_dir(&data_dir())?;
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
