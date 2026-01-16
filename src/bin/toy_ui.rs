use std::collections::{HashMap, HashSet};
use std::env;
use std::error::Error;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use tenet_crypto::crypto::{
    decrypt_payload, encrypt_payload, generate_content_key, generate_keypair, unwrap_content_key,
    wrap_content_key, StoredKeypair, WrappedKey,
};
use tenet_crypto::protocol::{ContentId, Envelope, Header, Payload, ProtocolVersion};

const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_TTL_SECONDS: u64 = 3600;
const ENVELOPE_CONTENT_TYPE: &str = "application/json;type=tenet.encrypted";
const HPKE_INFO: &[u8] = b"tenet-hpke";
const PAYLOAD_AAD: &[u8] = b"tenet-toy-ui";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrappedKeyPayload {
    enc_b64: String,
    ciphertext_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedPayload {
    nonce_b64: String,
    ciphertext_b64: String,
    wrapped_key: WrappedKeyPayload,
}

#[derive(Debug, Clone)]
struct FeedMessage {
    message_id: String,
    from_name: String,
    from_id: String,
    timestamp: u64,
    body: String,
}

#[derive(Debug)]
struct PeerState {
    name: String,
    keypair: StoredKeypair,
    online: bool,
    feed: Vec<FeedMessage>,
    seen: HashSet<String>,
}

impl PeerState {
    fn id(&self) -> &str {
        &self.keypair.id
    }
}

#[derive(Debug)]
struct ToyUiConfig {
    relay_url: String,
    peers: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config = parse_config()?;
    let mut peers = spawn_peers(config.peers);
    let peer_directory = build_peer_directory(&peers);

    println!(
        "Toy UI started with {} peers. Relay: {}",
        peers.len(),
        config.relay_url
    );
    print_help();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if matches!(input, "quit" | "exit") {
            println!("Goodbye!");
            break;
        }

        handle_command(
            input,
            &config,
            &mut peers,
            &peer_directory,
            &mut stdout,
        )?;

        stdout.flush()?;
    }

    Ok(())
}

fn parse_config() -> Result<ToyUiConfig, Box<dyn Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut peers = None;
    let mut relay_url = env::var("TENET_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--peers" => {
                let value = args.next().ok_or("--peers requires a value")?;
                peers = Some(value.parse::<usize>()?);
            }
            "--relay" => {
                let value = args.next().ok_or("--relay requires a value")?;
                relay_url = value;
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

    let peers = peers.unwrap_or(3).max(1);

    Ok(ToyUiConfig { relay_url, peers })
}

fn spawn_peers(count: usize) -> Vec<PeerState> {
    (1..=count)
        .map(|index| PeerState {
            name: format!("peer-{index}"),
            keypair: generate_keypair(),
            online: true,
            feed: Vec::new(),
            seen: HashSet::new(),
        })
        .collect()
}

fn build_peer_directory(peers: &[PeerState]) -> HashMap<String, (String, String)> {
    peers
        .iter()
        .map(|peer| {
            (
                peer.keypair.id.clone(),
                (peer.name.clone(), peer.keypair.public_key_hex.clone()),
            )
        })
        .collect()
}

fn print_help() {
    println!(
        "Commands:\n\
         help\n\
         peers\n\
         online <peer_name>\n\
         offline <peer_name>\n\
         send <from_peer> <to_peer> <message>\n\
         sync <peer_name|all> [limit]\n\
         feed <peer_name>\n\
         exit | quit\n"
    );
}

fn handle_command(
    input: &str,
    config: &ToyUiConfig,
    peers: &mut [PeerState],
    peer_directory: &HashMap<String, (String, String)>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let mut parts = input.split_whitespace();
    let command = parts.next().unwrap_or("");

    match command {
        "help" => print_help(),
        "peers" => list_peers(peers, stdout)?,
        "online" => toggle_peer(peers, parts.next(), true)?,
        "offline" => toggle_peer(peers, parts.next(), false)?,
        "send" => {
            let from = parts.next();
            let to = parts.next();
            let message = parts.collect::<Vec<_>>().join(" ");
            send_message(config, peers, from, to, message)?;
        }
        "sync" => {
            let target = parts.next();
            let limit = parts.next().map(|value| value.parse::<usize>()).transpose()?;
            sync_peers(config, peers, peer_directory, target, limit)?;
        }
        "feed" => {
            let target = parts.next();
            show_feed(peers, target, stdout)?;
        }
        _ => println!("Unknown command: {command}"),
    }

    Ok(())
}

fn list_peers(peers: &[PeerState], stdout: &mut io::Stdout) -> Result<(), Box<dyn Error>> {
    writeln!(stdout, "Peers:")?;
    for peer in peers {
        writeln!(
            stdout,
            "- {} (id: {}) [{}]",
            peer.name,
            peer.id(),
            if peer.online { "online" } else { "offline" }
        )?;
    }
    Ok(())
}

fn toggle_peer(
    peers: &mut [PeerState],
    name: Option<&str>,
    online: bool,
) -> Result<(), Box<dyn Error>> {
    let name = name.ok_or("peer name required")?;
    let peer = find_peer_mut(peers, name)?;
    peer.online = online;
    println!(
        "{} is now {}",
        peer.name,
        if peer.online { "online" } else { "offline" }
    );
    Ok(())
}

fn send_message(
    config: &ToyUiConfig,
    peers: &mut [PeerState],
    from: Option<&str>,
    to: Option<&str>,
    message: String,
) -> Result<(), Box<dyn Error>> {
    let from = from.ok_or("send requires <from_peer>")?;
    let to = to.ok_or("send requires <to_peer>")?;
    if message.trim().is_empty() {
        return Err("send requires a message".into());
    }

    let (recipient_id, recipient_public_key_hex) = {
        let recipient = find_peer(peers, to)?;
        (recipient.keypair.id.clone(), recipient.keypair.public_key_hex.clone())
    };

    let sender = find_peer_mut(peers, from)?;
    if !sender.online {
        println!("{} is offline; cannot send", sender.name);
        return Ok(());
    }

    let envelope = build_envelope(sender, &recipient_id, &recipient_public_key_hex, &message)?;

    post_envelope(config, &envelope)?;

    println!(
        "sent message {} -> {} (message_id: {})",
        sender.name,
        to,
        envelope.header.message_id.0
    );
    Ok(())
}

fn sync_peers(
    config: &ToyUiConfig,
    peers: &mut [PeerState],
    peer_directory: &HashMap<String, (String, String)>,
    target: Option<&str>,
    limit: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("sync requires <peer_name|all>")?;
    if target == "all" {
        for peer in peers.iter_mut() {
            if peer.online {
                sync_peer(config, peer, peer_directory, limit)?;
            }
        }
        return Ok(());
    }

    let peer = find_peer_mut(peers, target)?;
    if !peer.online {
        println!("{} is offline; cannot sync", peer.name);
        return Ok(());
    }
    sync_peer(config, peer, peer_directory, limit)?;
    Ok(())
}

fn show_feed(
    peers: &[PeerState],
    target: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("feed requires <peer_name>")?;
    let peer = find_peer(peers, target)?;
    writeln!(stdout, "Feed for {}:", peer.name)?;
    if peer.feed.is_empty() {
        writeln!(stdout, "(empty)")?;
        return Ok(());
    }

    for message in &peer.feed {
        writeln!(
            stdout,
            "- [{}] {} ({}) -> {}",
            message.timestamp,
            message.from_name,
            message.from_id,
            message.body
        )?;
    }
    Ok(())
}

fn find_peer<'a>(peers: &'a [PeerState], name: &str) -> Result<&'a PeerState, Box<dyn Error>> {
    peers
        .iter()
        .find(|peer| peer.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn find_peer_mut<'a>(
    peers: &'a mut [PeerState],
    name: &str,
) -> Result<&'a mut PeerState, Box<dyn Error>> {
    peers
        .iter_mut()
        .find(|peer| peer.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn build_envelope(
    sender: &PeerState,
    recipient_id: &str,
    recipient_public_key_hex: &str,
    message: &str,
) -> Result<Envelope, Box<dyn Error>> {
    let content_key = generate_content_key();
    let (nonce, ciphertext) = encrypt_payload(&content_key, message.as_bytes(), PAYLOAD_AAD, None)?;
    let recipient_public_key_bytes = hex::decode(recipient_public_key_hex)?;
    let wrapped = wrap_content_key(&recipient_public_key_bytes, &content_key, HPKE_INFO, None)?;

    let encrypted_payload = EncryptedPayload {
        nonce_b64: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext_b64: URL_SAFE_NO_PAD.encode(ciphertext),
        wrapped_key: WrappedKeyPayload {
            enc_b64: URL_SAFE_NO_PAD.encode(&wrapped.enc),
            ciphertext_b64: URL_SAFE_NO_PAD.encode(&wrapped.ciphertext),
        },
    };

    let payload_body = serde_json::to_string(&encrypted_payload)?;
    let payload_id = ContentId::from_bytes(payload_body.as_bytes());
    let payload = Payload {
        id: payload_id,
        content_type: ENVELOPE_CONTENT_TYPE.to_string(),
        body: payload_body,
        attachments: Vec::new(),
    };

    let message_id = ContentId::from_value(&payload)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs();

    let mut header = Header {
        sender_id: sender.keypair.id.clone(),
        recipient_id: recipient_id.to_string(),
        timestamp,
        message_id,
        ttl_seconds: DEFAULT_TTL_SECONDS,
        payload_size: payload.body.len() as u64,
        signature: None,
    };

    let signature = header.expected_signature(ProtocolVersion::V1)?;
    header.signature = Some(signature);

    Ok(Envelope {
        version: ProtocolVersion::V1,
        header,
        payload,
    })
}

fn post_envelope(config: &ToyUiConfig, envelope: &Envelope) -> Result<(), Box<dyn Error>> {
    let url = format!("{}/envelopes", config.relay_url.trim_end_matches('/'));
    let response = ureq::post(&url).send_json(serde_json::to_value(envelope)?);

    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => Err(format!("relay error: {code}").into()),
        Err(err) => Err(err.into()),
    }
}

fn sync_peer(
    config: &ToyUiConfig,
    peer: &mut PeerState,
    peer_directory: &HashMap<String, (String, String)>,
    limit: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let url = if let Some(limit) = limit {
        format!(
            "{}/inbox/{}?limit={}",
            config.relay_url.trim_end_matches('/'),
            peer.id(),
            limit
        )
    } else {
        format!(
            "{}/inbox/{}",
            config.relay_url.trim_end_matches('/'),
            peer.id()
        )
    };

    let response = ureq::get(&url).call()?;
    let envelopes: Vec<Envelope> = response.into_json()?;

    if envelopes.is_empty() {
        println!("{} inbox is empty", peer.name);
        return Ok(());
    }

    for envelope in envelopes {
        if peer.seen.contains(&envelope.header.message_id.0) {
            continue;
        }
        match decrypt_envelope(peer, peer_directory, &envelope) {
            Ok(message) => {
                peer.seen.insert(envelope.header.message_id.0.clone());
                peer.feed.push(message);
            }
            Err(error) => {
                println!("{} failed to decrypt message: {error}", peer.name);
            }
        }
    }

    println!("{} synced {} messages", peer.name, peer.feed.len());
    Ok(())
}

fn decrypt_envelope(
    peer: &PeerState,
    peer_directory: &HashMap<String, (String, String)>,
    envelope: &Envelope,
) -> Result<FeedMessage, Box<dyn Error>> {
    let encrypted_payload: EncryptedPayload = serde_json::from_str(&envelope.payload.body)?;
    let wrapped = WrappedKey {
        enc: URL_SAFE_NO_PAD.decode(encrypted_payload.wrapped_key.enc_b64.as_bytes())?,
        ciphertext: URL_SAFE_NO_PAD.decode(encrypted_payload.wrapped_key.ciphertext_b64.as_bytes())?,
    };

    let private_key_bytes = hex::decode(&peer.keypair.private_key_hex)?;
    let content_key = unwrap_content_key(&private_key_bytes, &wrapped, HPKE_INFO)?;
    let nonce = URL_SAFE_NO_PAD.decode(encrypted_payload.nonce_b64.as_bytes())?;
    let ciphertext = URL_SAFE_NO_PAD.decode(encrypted_payload.ciphertext_b64.as_bytes())?;
    let plaintext = decrypt_payload(&content_key, &nonce, &ciphertext, PAYLOAD_AAD)?;

    let (from_name, _) = peer_directory
        .get(&envelope.header.sender_id)
        .cloned()
        .unwrap_or_else(|| ("unknown".to_string(), "".to_string()));

    let body = String::from_utf8(plaintext)?;

    Ok(FeedMessage {
        message_id: envelope.header.message_id.0.clone(),
        from_name,
        from_id: envelope.header.sender_id.clone(),
        timestamp: envelope.header.timestamp,
        body,
    })
}
