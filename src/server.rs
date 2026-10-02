use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const LISTEN_ADDRESS: &str = "0.0.0.0:6969";
const TOKEN_BYTES: usize = 16;
const MAX_LINE_BYTES: usize = 4 * 1024;
const MAX_NICKNAME_CHARS: usize = 16;
const MIN_MESSAGE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RATE_LIMIT_VIOLATIONS: u32 = 10;
const BAN_DURATION: Duration = Duration::from_secs(10 * 60);

struct Client {
    nickname: String,
    last_message_at: Option<Instant>,
    rate_limit_violations: u32,
    outbound: Sender<Outbound>,
}

// Messages the per-connection writer thread serializes onto the socket.
enum Outbound {
    Line(String),
    Shutdown,
}

enum ServerEvent {
    Connected {
        address: SocketAddr,
        outbound: Sender<Outbound>,
    },
    Disconnected {
        address: SocketAddr,
    },
    LineReceived {
        address: SocketAddr,
        line: String,
    },
}

fn send_line(mut stream: &TcpStream, line: &str) -> io::Result<()> {
    writeln!(stream, "{line}")
}

fn send_to(client: &Client, line: &str) {
    let _ = client.outbound.send(Outbound::Line(line.to_owned()));
}

fn broadcast(clients: &HashMap<SocketAddr, Client>, except: SocketAddr, line: &str) {
    for (address, client) in clients {
        if *address != except {
            send_to(client, line);
        }
    }
}

// Validate a requested nickname. Returns a human-readable reason on failure.
fn nickname_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("nickname cannot be empty");
    }
    if name.chars().count() > MAX_NICKNAME_CHARS {
        return Some("nickname is too long (maximum: 16 characters)");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Some("nickname may only contain letters, numbers, '_' and '-'");
    }
    None
}

// A parsed client line. Unknown commands are reported so the caller can send
// an ERR without crashing the connection.
#[derive(Debug, PartialEq, Eq)]
enum ClientCommand<'a> {
    Message(&'a str),
    Nick(&'a str),
    Unknown(&'a str),
}

// Parse a single client line. Blank lines are ignored.
fn parse_client_line(line: &str) -> Option<ClientCommand<'_>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (command, payload) = line.split_once(' ').unwrap_or((line, ""));
    match command {
        "MSG" => Some(ClientCommand::Message(payload.trim())),
        "NICK" => Some(ClientCommand::Nick(payload.trim())),
        other => Some(ClientCommand::Unknown(other)),
    }
}

fn remove_client(clients: &mut HashMap<SocketAddr, Client>, address: SocketAddr) {
    if let Some(client) = clients.remove(&address) {
        println!("{address} ({}) disconnected", client.nickname);
        broadcast(
            clients,
            address,
            &format!("SYS {} left the chat", client.nickname),
        );
    }
}

fn run_server(events: Receiver<ServerEvent>) {
    let mut clients = HashMap::<SocketAddr, Client>::new();
    let mut banned_until = HashMap::<IpAddr, Instant>::new();

    while let Ok(event) = events.recv() {
        match event {
            ServerEvent::Connected { address, outbound } => {
                let now = Instant::now();
                banned_until.retain(|_, deadline| *deadline > now);

                if let Some(deadline) = banned_until.get(&address.ip()) {
                    let seconds = deadline.saturating_duration_since(now).as_secs() + 1;
                    let _ = outbound.send(Outbound::Line(format!(
                        "ERR temporarily banned; try again in {seconds} seconds"
                    )));
                    let _ = outbound.send(Outbound::Shutdown);
                    continue;
                }

                let nickname = format!("user-{}", address.port());
                let _ = outbound.send(Outbound::Line(format!("YOU {nickname}")));
                broadcast(
                    &clients,
                    address,
                    &format!("SYS {nickname} joined the chat"),
                );
                println!("{address} connected as {nickname}");

                clients.insert(
                    address,
                    Client {
                        nickname,
                        last_message_at: None,
                        rate_limit_violations: 0,
                        outbound,
                    },
                );
            }
            ServerEvent::Disconnected { address } => remove_client(&mut clients, address),
            ServerEvent::LineReceived { address, line } => {
                let now = Instant::now();
                let (too_quick, should_ban) = {
                    let Some(client) = clients.get_mut(&address) else {
                        continue;
                    };
                    let too_quick = client
                        .last_message_at
                        .is_some_and(|last| now.duration_since(last) < MIN_MESSAGE_INTERVAL);
                    client.last_message_at = Some(now);
                    if too_quick {
                        client.rate_limit_violations += 1;
                        send_to(client, "ERR you are sending messages too quickly");
                        (
                            true,
                            client.rate_limit_violations >= MAX_RATE_LIMIT_VIOLATIONS,
                        )
                    } else {
                        client.rate_limit_violations = 0;
                        (false, false)
                    }
                };

                if should_ban {
                    banned_until.insert(address.ip(), now + BAN_DURATION);
                    if let Some(client) = clients.get(&address) {
                        send_to(
                            client,
                            "ERR too many rapid messages; you are banned for 10 minutes",
                        );
                        let _ = client.outbound.send(Outbound::Shutdown);
                    }
                    remove_client(&mut clients, address);
                    continue;
                }

                if too_quick {
                    continue;
                }

                match parse_client_line(&line) {
                    None => {}
                    Some(ClientCommand::Message(text)) => {
                        if text.is_empty() {
                            if let Some(client) = clients.get(&address) {
                                send_to(client, "ERR message cannot be empty");
                            }
                            continue;
                        }
                        let nickname = clients[&address].nickname.clone();
                        let message = format!("MSG {nickname} {text}");
                        println!("{address} ({nickname}): {text}");
                        broadcast(&clients, address, &message);
                    }
                    Some(ClientCommand::Nick(requested)) => {
                        let requested = requested.to_owned();
                        let taken = clients.iter().any(|(other_address, other)| {
                            *other_address != address
                                && other.nickname.eq_ignore_ascii_case(&requested)
                        });
                        let error = nickname_error(&requested)
                            .or(taken.then_some("nickname is already in use"));

                        if let Some(reason) = error {
                            if let Some(client) = clients.get(&address) {
                                send_to(client, &format!("ERR {reason}"));
                            }
                            continue;
                        }

                        let old_nickname = clients[&address].nickname.clone();
                        clients.get_mut(&address).unwrap().nickname = requested.clone();
                        if let Some(client) = clients.get(&address) {
                            send_to(client, &format!("YOU {requested}"));
                        }
                        broadcast(
                            &clients,
                            address,
                            &format!("NICK {old_nickname} {requested}"),
                        );
                    }
                    Some(ClientCommand::Unknown(_)) => {
                        if let Some(client) = clients.get(&address) {
                            send_to(client, "ERR unknown protocol command");
                        }
                    }
                }
            }
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    address: SocketAddr,
    token: &str,
    events: Sender<ServerEvent>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    send_line(&stream, "SYS authenticating")?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut supplied_token = String::new();
    reader.read_line(&mut supplied_token)?;
    if supplied_token.trim_end_matches(['\r', '\n']) != token {
        send_line(&stream, "ERR invalid access token")?;
        stream.shutdown(Shutdown::Both)?;
        return Ok(());
    }

    stream.set_read_timeout(None)?;
    send_line(
        &stream,
        "SYS connected; type /help to see available commands",
    )?;

    let (outbound_tx, outbound_rx) = mpsc::channel::<Outbound>();
    events
        .send(ServerEvent::Connected {
            address,
            outbound: outbound_tx.clone(),
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "server stopped"))?;

    let writer_stream = stream.try_clone()?;
    thread::spawn(move || {
        for outbound in outbound_rx {
            let result = match outbound {
                Outbound::Line(line) => send_line(&writer_stream, &line),
                Outbound::Shutdown => {
                    let _ = writer_stream.shutdown(Shutdown::Both);
                    break;
                }
            };
            if result.is_err() {
                break;
            }
        }
    });
    drop(outbound_tx);

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_LINE_BYTES => {
                send_line(&stream, "ERR message is too long")?;
            }
            Ok(_) => {
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    continue;
                }
                if events
                    .send(ServerEvent::LineReceived {
                        address,
                        line: line.to_owned(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                eprintln!("Read error from {address}: {error}");
                break;
            }
        }
    }

    let _ = events.send(ServerEvent::Disconnected { address });
    Ok(())
}

fn generate_token() -> io::Result<String> {
    let mut random_bytes = [0; TOKEN_BYTES];
    getrandom::fill(&mut random_bytes).map_err(io::Error::other)?;

    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in random_bytes {
        write!(token, "{byte:02X}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

fn main() -> io::Result<()> {
    let token = generate_token()?;
    let listener = TcpListener::bind(LISTEN_ADDRESS)?;
    println!("Chat server listening on {LISTEN_ADDRESS}");
    println!("Access token: {token}");

    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || run_server(event_receiver));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let address = stream.peer_addr()?;
                let events = event_sender.clone();
                let token = token.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, address, &token, events) {
                        eprintln!("Connection error for {address}: {error}");
                    }
                });
            }
            Err(error) => eprintln!("Could not accept connection: {error}"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_command() {
        assert_eq!(
            parse_client_line("MSG hello world"),
            Some(ClientCommand::Message("hello world"))
        );
        assert_eq!(parse_client_line("MSG"), Some(ClientCommand::Message("")));
        assert_eq!(
            parse_client_line("MSG   padded  "),
            Some(ClientCommand::Message("padded"))
        );
    }

    #[test]
    fn parses_nick_command() {
        assert_eq!(
            parse_client_line("NICK alice"),
            Some(ClientCommand::Nick("alice"))
        );
        assert_eq!(parse_client_line("NICK"), Some(ClientCommand::Nick("")));
    }

    #[test]
    fn blank_lines_are_ignored() {
        assert_eq!(parse_client_line(""), None);
        assert_eq!(parse_client_line("   "), None);
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert_eq!(
            parse_client_line("PUT file"),
            Some(ClientCommand::Unknown("PUT"))
        );
        assert_eq!(
            parse_client_line("hello"),
            Some(ClientCommand::Unknown("hello"))
        );
    }

    #[test]
    fn validates_nicknames() {
        assert!(nickname_error("bob").is_none());
        assert!(nickname_error("a-b_c1").is_none());
        assert!(nickname_error(&"a".repeat(MAX_NICKNAME_CHARS)).is_none());

        assert!(nickname_error("").is_some());
        assert!(nickname_error(&"a".repeat(MAX_NICKNAME_CHARS + 1)).is_some());
        assert!(nickname_error("bad name").is_some());
        assert!(nickname_error("böb").is_some());
        assert!(nickname_error("with\nnewline").is_some());
    }
}
