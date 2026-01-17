use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::io::{self, Write};

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::generate_keypair;

const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_TTL_SECONDS: u64 = 3600;
const HPKE_INFO: &[u8] = b"tenet-hpke";
const PAYLOAD_AAD: &[u8] = b"tenet-toy-ui";
const PROMPT: &str = "\x1b[1;34mtenet-debugger>\x1b[0m ";

#[derive(Debug)]
struct ToyPeer {
    name: String,
    client: RelayClient,
}

impl ToyPeer {
    fn id(&self) -> &str {
        self.client.id()
    }
}

#[derive(Debug)]
struct ToyUiConfig {
    client_config: ClientConfig,
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
    let mut peers = spawn_peers(config.peers, &config.client_config);
    let peer_directory = build_peer_directory(&peers);

    println!(
        "Toy UI started with {} peers. Relay: {}",
        peers.len(),
        config.client_config.relay_url()
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

                if let Err(error) = handle_command(input, &mut peers, &peer_directory, &mut stdout)
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

fn parse_config() -> Result<ToyUiConfig, Box<dyn Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut peers = None;
    let mut relay_url =
        env::var("TENET_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());

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

    let client_config = ClientConfig::new(
        relay_url,
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: HPKE_INFO.to_vec(),
            payload_aad: PAYLOAD_AAD.to_vec(),
        },
    );

    Ok(ToyUiConfig {
        client_config,
        peers,
    })
}

fn spawn_peers(count: usize, client_config: &ClientConfig) -> Vec<ToyPeer> {
    (1..=count)
        .map(|index| {
            let keypair = generate_keypair();
            ToyPeer {
                name: format!("peer-{index}"),
                client: RelayClient::new(keypair, client_config.clone()),
            }
        })
        .collect()
}

fn build_peer_directory(peers: &[ToyPeer]) -> HashMap<String, (String, String)> {
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
    peers: &mut [ToyPeer],
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
            send_message(peers, from, to, message)?;
        }
        "sync" => {
            let target = parts.next();
            let limit = parts
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()?;
            sync_peers(peers, target, limit)?;
        }
        "feed" => {
            let target = parts.next();
            show_feed(peers, peer_directory, target, stdout)?;
        }
        _ => println!("Unknown command: {command}"),
    }

    Ok(())
}

fn list_peers(peers: &[ToyPeer], stdout: &mut io::Stdout) -> Result<(), Box<dyn Error>> {
    writeln!(stdout, "Peers:")?;
    for peer in peers {
        writeln!(
            stdout,
            "- {} (id: {}) [{}]",
            peer.name,
            peer.id(),
            if peer.client.is_online() {
                "online"
            } else {
                "offline"
            }
        )?;
    }
    Ok(())
}

fn toggle_peer(
    peers: &mut [ToyPeer],
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

fn send_message(
    peers: &mut [ToyPeer],
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

    println!(
        "sent message {} -> {} (message_id: {})",
        sender.name, to, envelope.header.message_id.0
    );
    Ok(())
}

fn sync_peers(
    peers: &mut [ToyPeer],
    target: Option<&str>,
    limit: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("sync requires <peer_name|all>")?;
    if target == "all" {
        for peer in peers.iter_mut() {
            if peer.client.is_online() {
                sync_peer(peer, limit)?;
            }
        }
        return Ok(());
    }

    let peer = find_peer_mut(peers, target)?;
    if !peer.client.is_online() {
        println!("{} is offline; cannot sync", peer.name);
        return Ok(());
    }
    sync_peer(peer, limit)?;
    Ok(())
}

fn show_feed(
    peers: &[ToyPeer],
    peer_directory: &HashMap<String, (String, String)>,
    target: Option<&str>,
    stdout: &mut io::Stdout,
) -> Result<(), Box<dyn Error>> {
    let target = target.ok_or("feed requires <peer_name>")?;
    let peer = find_peer(peers, target)?;
    writeln!(stdout, "Feed for {}:", peer.name)?;
    if peer.client.feed().is_empty() {
        writeln!(stdout, "(empty)")?;
        return Ok(());
    }

    for message in peer.client.feed() {
        let (from_name, _) = peer_directory_lookup(peer_directory, &message.sender_id);
        writeln!(
            stdout,
            "- [{}] {} ({}) -> {}",
            message.timestamp, from_name, message.sender_id, message.body
        )?;
    }
    Ok(())
}

fn find_peer<'a>(peers: &'a [ToyPeer], name: &str) -> Result<&'a ToyPeer, Box<dyn Error>> {
    peers
        .iter()
        .find(|peer| peer.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn find_peer_mut<'a>(
    peers: &'a mut [ToyPeer],
    name: &str,
) -> Result<&'a mut ToyPeer, Box<dyn Error>> {
    peers
        .iter_mut()
        .find(|peer| peer.name == name)
        .ok_or_else(|| format!("unknown peer: {name}").into())
}

fn sync_peer(peer: &mut ToyPeer, limit: Option<usize>) -> Result<(), Box<dyn Error>> {
    let outcome = peer.client.sync_inbox(limit)?;
    if outcome.fetched == 0 {
        println!("{} inbox is empty", peer.name);
        return Ok(());
    }

    for error in outcome.errors {
        println!("{} failed to decrypt message: {error}", peer.name);
    }

    println!("{} synced {} messages", peer.name, peer.client.feed().len());
    Ok(())
}

fn peer_directory_lookup(
    peer_directory: &HashMap<String, (String, String)>,
    sender_id: &str,
) -> (String, String) {
    peer_directory
        .get(sender_id)
        .cloned()
        .unwrap_or_else(|| ("unknown".to_string(), "".to_string()))
}
