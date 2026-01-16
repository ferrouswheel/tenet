use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use tenet_crypto::crypto::{
    decrypt_payload, derive_user_id_from_public_key, encrypt_payload, generate_content_key,
    generate_keypair, load_keypair, rotate_keypair, store_keypair, wrap_content_key,
    KeyRotation, StoredKeypair, WrappedKey,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Peer {
    name: String,
    id: String,
    public_key_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    sender_id: String,
    recipient_id: String,
    timestamp: u64,
    content_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WrappedKeyData {
    enc_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedPayload {
    nonce_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    message_id: String,
    header: Header,
    wrapped_key: WrappedKeyData,
    payload: EncryptedPayload,
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
        "tenet-crypto commands:\n\
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

    let timestamp = current_timestamp()?;
    let header = Header {
        sender_id: identity.id.clone(),
        recipient_id: peer.id.clone(),
        timestamp,
        content_type: "text/plain".to_string(),
    };
    let aad = serde_json::to_vec(&header)?;

    let content_key = generate_content_key();
    let (nonce, ciphertext) = encrypt_payload(&content_key, message.as_bytes(), &aad, None)?;
    let wrapped = wrap_content_key(
        &hex::decode(&peer.public_key_hex)?,
        &content_key,
        b"tenet-cli",
        None,
    )?;

    let payload = EncryptedPayload {
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(ciphertext),
    };

    let message_id = build_message_id(&header, &payload)?;

    let envelope = Envelope {
        message_id,
        header,
        wrapped_key: WrappedKeyData {
            enc_hex: hex::encode(wrapped.enc),
            ciphertext_hex: hex::encode(wrapped.ciphertext),
        },
        payload,
    };

    let body = serde_json::to_string(&envelope)?;
    append_json_line(outbox_path(), &envelope)?;

    let response = ureq::post(&format!("{}/envelopes", relay_url))
        .set("Content-Type", "application/json")
        .send_string(&body)?;

    if response.status() >= 400 {
        return Err(format!("relay returned status {}", response.status()).into());
    }

    println!("sent message {} to {}", envelope.message_id, peer.name);
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
    let response = ureq::get(&format!("{}/inbox/{}", relay_url, identity.id)).call()?;
    let body = response.into_string()?;
    if body.trim().is_empty() {
        println!("no envelopes available");
        return Ok(());
    }

    let envelopes: Vec<Envelope> = serde_json::from_str(&body)?;
    let mut received = 0;

    for envelope in envelopes {
        if envelope.header.recipient_id != identity.id {
            continue;
        }

        let plaintext = decrypt_envelope(&identity, &envelope)?;
        let message = String::from_utf8_lossy(&plaintext);
        println!("from {}: {}", envelope.header.sender_id, message);

        let record = ReceivedMessage {
            sender_id: envelope.header.sender_id.clone(),
            timestamp: envelope.header.timestamp,
            message: message.to_string(),
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
    let identity = StoredKeypair {
        id,
        public_key_hex,
        private_key_hex,
    };

    store_keypair(&identity_path(), &identity)?;
    println!("identity imported: {}", identity.id);
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

fn decrypt_envelope(identity: &StoredKeypair, envelope: &Envelope) -> Result<Vec<u8>, Box<dyn Error>> {
    let aad = serde_json::to_vec(&envelope.header)?;
    let wrapped = WrappedKey {
        enc: hex::decode(&envelope.wrapped_key.enc_hex)?,
        ciphertext: hex::decode(&envelope.wrapped_key.ciphertext_hex)?,
    };

    let recipient_private_key = hex::decode(&identity.private_key_hex)?;
    let content_key =
        tenet_crypto::crypto::unwrap_content_key(&recipient_private_key, &wrapped, b"tenet-cli")?;

    let nonce = hex::decode(&envelope.payload.nonce_hex)?;
    let ciphertext = hex::decode(&envelope.payload.ciphertext_hex)?;
    Ok(decrypt_payload(&content_key, &nonce, &ciphertext, &aad)?)
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

fn build_message_id(header: &Header, payload: &EncryptedPayload) -> Result<String, Box<dyn Error>> {
    let mut bytes = serde_json::to_vec(header)?;
    bytes.extend_from_slice(payload.nonce_hex.as_bytes());
    bytes.extend_from_slice(payload.ciphertext_hex.as_bytes());
    Ok(derive_user_id_from_public_key(&bytes))
}

fn current_timestamp() -> Result<u64, Box<dyn Error>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(now.as_secs())
}

fn validate_key_len(bytes: &[u8], label: &str) -> Result<(), Box<dyn Error>> {
    if bytes.len() != 32 {
        return Err(format!("{label} key must be 32 bytes").into());
    }
    Ok(())
}
