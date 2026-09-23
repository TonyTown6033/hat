use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const LISTEN_ADDRESS: &str = "0.0.0.0:6969";
const TOKEN_BYTES: usize = 16;
const MAX_LINE_BYTES: usize = 4 * 1024;
const MIN_MESSAGE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RATE_LIMIT_VIOLATIONS: u32 = 10;
const BAN_DURATION: Duration = Duration::from_secs(10 * 60);
const HISTORY_LIMIT: usize = 1000;
const HISTORY_ON_CONNECT: usize = 10;
const UPLOADS_DIR: &str = "uploads";
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
const MAX_FILENAME_BYTES: usize = 255;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_SIZE: usize = 64 * 1024;

struct FileMeta {
    size: u64,
}

struct Client {
    nickname: String,
    last_message_at: Option<Instant>,
    rate_limit_violations: u32,
    outbound: Sender<Outbound>,
}

// Messages the per-connection writer thread serializes onto the socket.
enum Outbound {
    Line(String),
    File {
        name: String,
        size: u64,
        path: PathBuf,
    },
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
    FileUploaded {
        from: SocketAddr,
        name: String,
        size: u64,
        temp_path: PathBuf,
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

fn sanitize_filename(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_FILENAME_BYTES {
        return None;
    }
    if name.starts_with('.') {
        return None;
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return None;
    }
    Some(name.to_owned())
}

fn uploads_dir() -> PathBuf {
    PathBuf::from(UPLOADS_DIR)
}

fn load_files(uploads: &Path) -> HashMap<String, FileMeta> {
    let mut files = HashMap::new();
    let Ok(entries) = fs::read_dir(uploads) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".part") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        files.insert(name.to_owned(), FileMeta { size: meta.len() });
    }
    files
}

fn send_file(mut stream: &TcpStream, name: &str, size: u64, path: &Path) -> io::Result<()> {
    writeln!(stream, "FILE {name} {size}")?;
    let mut file = File::open(path)?;
    let mut buffer = [0u8; CHUNK_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let want = (remaining as usize).min(CHUNK_SIZE);
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file is shorter than expected",
            ));
        }
        stream.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    Ok(())
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
    let uploads = uploads_dir();
    let _ = fs::create_dir_all(&uploads);
    let mut files = load_files(&uploads);

    let mut clients = HashMap::<SocketAddr, Client>::new();
    let mut banned_until = HashMap::<IpAddr, Instant>::new();
    let mut message_history = VecDeque::<String>::new();

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
                for message in message_history.iter().rev().take(HISTORY_ON_CONNECT).rev() {
                    let _ = outbound.send(Outbound::Line(message.clone()));
                }
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
            ServerEvent::FileUploaded {
                from,
                name,
                size,
                temp_path,
            } => {
                let Some(client) = clients.get(&from) else {
                    let _ = fs::remove_file(&temp_path);
                    continue;
                };

                if files.contains_key(&name) {
                    let _ = fs::remove_file(&temp_path);
                    send_to(client, "ERR file already exists");
                    continue;
                }

                let final_path = uploads.join(&name);
                if let Err(error) = fs::rename(&temp_path, &final_path) {
                    let _ = fs::remove_file(&temp_path);
                    send_to(client, &format!("ERR could not store file: {error}"));
                    continue;
                }

                files.insert(name.clone(), FileMeta { size });
                let nickname = client.nickname.clone();
                send_to(client, &format!("OK {name}"));
                broadcast(
                    &clients,
                    from,
                    &format!("SYS {nickname} uploaded {name} ({size} bytes)"),
                );
            }
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

                if !clients.contains_key(&address) {
                    continue;
                }

                let (command, payload) = line.split_once(' ').unwrap_or((&line, ""));
                match command {
                    "MSG" => {
                        let text = payload.trim();
                        if text.is_empty() {
                            if let Some(client) = clients.get(&address) {
                                send_to(client, "ERR message cannot be empty");
                            }
                            continue;
                        }
                        let nickname = clients[&address].nickname.clone();
                        let message = format!("MSG {nickname} {text}");
                        println!("{address} ({nickname}): {text}");
                        message_history.push_back(message.clone());
                        if message_history.len() > HISTORY_LIMIT {
                            message_history.pop_front();
                        }
                        broadcast(&clients, address, &message);
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
                                send_to(client, &format!("ERR {reason}"));
                            }
                            continue;
                        }

                        let old_nickname = clients[&address].nickname.clone();
                        clients.get_mut(&address).unwrap().nickname = requested.to_owned();
                        if let Some(client) = clients.get(&address) {
                            send_to(client, &format!("YOU {requested}"));
                        }
                        broadcast(
                            &clients,
                            address,
                            &format!("NICK {old_nickname} {requested}"),
                        );
                    }
                    "GET" => {
                        let name = payload.trim();
                        let Some(meta) = files.get(name) else {
                            if let Some(client) = clients.get(&address) {
                                send_to(client, "ERR file not found");
                            }
                            continue;
                        };
                        let path = uploads.join(name);
                        if let Some(client) = clients.get(&address) {
                            let _ = client.outbound.send(Outbound::File {
                                name: name.to_owned(),
                                size: meta.size,
                                path,
                            });
                        }
                    }
                    "LS" => {
                        let mut entries: Vec<String> = files
                            .iter()
                            .map(|(name, meta)| format!("{name}:{}", meta.size))
                            .collect();
                        entries.sort();
                        let line = format!("FILES {}", entries.join(","));
                        if let Some(client) = clients.get(&address) {
                            send_to(client, &line);
                        }
                    }
                    _ => {
                        if let Some(client) = clients.get(&address) {
                            send_to(client, "ERR unknown protocol command");
                        }
                    }
                }
            }
        }
    }
}

fn receive_upload(
    stream: &TcpStream,
    reader: &mut BufReader<TcpStream>,
    address: SocketAddr,
    rest: &str,
    events: &Sender<ServerEvent>,
) -> io::Result<()> {
    let reject = |stream: &TcpStream, message: &str| -> io::Error {
        let _ = send_line(stream, message);
        io::Error::new(io::ErrorKind::InvalidInput, "invalid upload request")
    };

    let Some((name, size_text)) = rest.rsplit_once(' ') else {
        return Err(reject(stream, "ERR usage: PUT <name> <size>"));
    };
    let Some(name) = sanitize_filename(name) else {
        return Err(reject(stream, "ERR invalid file name"));
    };
    let Ok(size) = size_text.parse::<u64>() else {
        return Err(reject(stream, "ERR invalid file size"));
    };
    if size > MAX_FILE_SIZE {
        return Err(reject(stream, "ERR file too large (max 16MB)"));
    }

    let temp_path = uploads_dir().join(format!("{name}.{}.part", address.port()));
    let mut file = File::create(&temp_path)?;

    stream.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
    let mut limited = (&mut *reader).take(size);
    let copied = io::copy(&mut limited, &mut file);
    stream.set_read_timeout(None)?;
    drop(file);

    let copied = copied?;
    if copied != size {
        let _ = fs::remove_file(&temp_path);
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "upload ended early",
        ));
    }

    events
        .send(ServerEvent::FileUploaded {
            from: address,
            name,
            size,
            temp_path,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "server stopped"))
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
                Outbound::File { name, size, path } => {
                    send_file(&writer_stream, &name, size, &path)
                }
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
                if let Some(rest) = line.strip_prefix("PUT ") {
                    if receive_upload(&stream, &mut reader, address, rest, &events).is_err() {
                        break;
                    }
                } else if events
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
