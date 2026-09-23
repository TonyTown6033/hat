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
const MIN_MESSAGE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RATE_LIMIT_VIOLATIONS: u32 = 10;
const BAN_DURATION: Duration = Duration::from_secs(10 * 60);

struct Client {
    stream: TcpStream,
    nickname: String,
    last_message_at: Option<Instant>,
    rate_limit_violations: u32,
}

enum ServerEvent {
    Connected {
        address: SocketAddr,
        stream: TcpStream,
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

fn broadcast(clients: &HashMap<SocketAddr, Client>, except: SocketAddr, line: &str) {
    for (address, client) in clients {
        if *address != except
            && let Err(error) = send_line(&client.stream, line)
        {
            eprintln!("Could not send to {address}: {error}");
        }
    }
}

fn nickname_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("nickname cannot be empty");
    }
    if name.chars().count() > 16 {
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
            ServerEvent::Connected { address, stream } => {
                let now = Instant::now();
                banned_until.retain(|_, deadline| *deadline > now);

                if let Some(deadline) = banned_until.get(&address.ip()) {
                    let seconds = deadline.saturating_duration_since(now).as_secs() + 1;
                    let _ = send_line(
                        &stream,
                        &format!("ERR temporarily banned; try again in {seconds} seconds"),
                    );
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }

                // todo: user port will leak user info try use some self add number
                let nickname = format!("user-{}", address.port());
                let _ = send_line(&stream, &format!("YOU {nickname}"));
                broadcast(
                    &clients,
                    address,
                    &format!("SYS {nickname} joined the chat"),
                );
                println!("{address} connected as {nickname}");

                clients.insert(
                    address,
                    Client {
                        stream,
                        nickname,
                        last_message_at: None,
                        rate_limit_violations: 0,
                    },
                );
            }
            ServerEvent::Disconnected { address } => remove_client(&mut clients, address),
            ServerEvent::LineReceived { address, line } => {
                let now = Instant::now();
                let should_ban = {
                    let Some(client) = clients.get_mut(&address) else {
                        continue;
                    };
                    let too_quick = client
                        .last_message_at
                        .is_some_and(|last| now.duration_since(last) < MIN_MESSAGE_INTERVAL);
                    client.last_message_at = Some(now);
                    if too_quick {
                        client.rate_limit_violations += 1;
                        let _ =
                            send_line(&client.stream, "ERR you are sending messages too quickly");
                        client.rate_limit_violations >= MAX_RATE_LIMIT_VIOLATIONS
                    } else {
                        client.rate_limit_violations = 0;
                        false
                    }
                };
                if should_ban {
                    banned_until.insert(address.ip(), now + BAN_DURATION);
                    if let Some(client) = clients.get(&address) {
                        let _ = send_line(
                            &client.stream,
                            "ERR too many rapid messages; you are banned for 10 minutes",
                        );
                        let _ = client.stream.shutdown(Shutdown::Both);
                    }
                    remove_client(&mut clients, address);
                    continue;
                }
                // whats this???
                if !clients.contains_key(&address) {
                    continue;
                }

                let (command, payload) = line.split_once(' ').unwrap_or((&line, ""));
                match command {
                    "MSG" => {
                        let text = payload.trim();
                        if text.is_empty() {
                            if let Some(client) = clients.get(&address) {
                                let _ = send_line(&client.stream, "ERR message cannot be empty");
                            }
                            continue;
                        }
                        let nickname = clients[&address].nickname.clone();
                        // bug:leak info here
                        println!("{address} ({nickname}): {text}");
                        broadcast(&clients, address, &format!("MSG {nickname} {text}"));
                    }
                    "NICK" => {
                        let requested = payload.trim();
                        let taken = clients.iter().any(|(other_address, other)| {
                            *other_address != address
                                && other.nickname.eq_ignore_ascii_case(requested)
                        });
                        let error = nickname_error(requested)
                            .or(taken.then_some("nickname is already in use"));

                        if let Some(reason) = error {
                            if let Some(client) = clients.get(&address) {
                                let _ = send_line(&client.stream, &format!("ERR {reason}"));
                            }
                            continue;
                        }

                        let old_nickname = clients[&address].nickname.clone();
                        clients.get_mut(&address).unwrap().nickname = requested.to_owned();
                        let _ = send_line(&clients[&address].stream, &format!("YOU {requested}"));
                        broadcast(
                            &clients,
                            address,
                            &format!("NICK {old_nickname} {requested}"),
                        );
                    }
                    _ => {
                        let _ =
                            send_line(&clients[&address].stream, "ERR unknown protocol command");
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
    events
        .send(ServerEvent::Connected {
            address,
            stream: stream.try_clone()?,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "server stopped"))?;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_LINE_BYTES => {
                send_line(&stream, "ERR message is too long")?;
            }
            Ok(_) => {
                let line = line.trim_end_matches(['\r', '\n']);
                if !line.is_empty()
                    && events
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
    thread::spawn(|| run_server(event_receiver));

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
