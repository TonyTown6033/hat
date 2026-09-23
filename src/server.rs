use std::collections::HashMap;
use std::fmt::Write as OtherWrite;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::result;
use std::str::from_utf8;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, SystemTime};
use std::{fmt, usize};

const SAFE_MODE: bool = true;
const BAN_LIMIT: Duration = Duration::from_secs(60 * 10);
const MES_FREQ: Duration = Duration::from_secs(1);
const BAN_FREQ: u32 = 10;
const TOKEN_LEN: usize = 16;

type Result<T> = result::Result<T, ()>;

struct Sensitive<T>(T);

impl<T: fmt::Display> fmt::Display for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if SAFE_MODE {
            writeln!(f, "[REDACTED]")
        } else {
            writeln!(f, "{}", self.0)
        }
    }
}

struct Client {
    conn: Arc<TcpStream>,
    last_message: SystemTime,
    strike_count: u32,
    nick: String,
}

enum Messages {
    ClientConnected {
        author: Arc<TcpStream>,
    },
    ClientDisconnected {
        author_addr: SocketAddr,
    },
    NewMessage {
        author_addr: SocketAddr,
        line: String,
    },
}

// Validate a requested nickname. Returns the reason it is rejected.
fn nickname_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("nickname is empty");
    }
    if name.chars().count() > 16 {
        return Some("nickname too long (max 16)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some("nickname has invalid characters");
    }
    None
}

// Send a line to every client except `except`.
fn broadcast(clients: &HashMap<SocketAddr, Client>, except: SocketAddr, message: &str) {
    for (addr, client) in clients.iter() {
        if *addr != except {
            let _ = client.conn.as_ref().write_all(message.as_bytes());
        }
    }
}

fn server(messages_receiver: Receiver<Messages>) -> Result<()> {
    let mut clients = HashMap::<SocketAddr, Client>::new();
    let mut banned_mfs = HashMap::<IpAddr, SystemTime>::new();
    loop {
        let message = messages_receiver.recv().expect("Receiver is hang up !!");
        match message {
            Messages::ClientConnected { author } => {
                let author_address = author.peer_addr().expect("TODO: cache the ip addresss");
                let now = SystemTime::now();
                let banned_at = banned_mfs.remove(&author_address.ip());

                banned_at.and_then(|banned_at| {
                    let diff = now
                        .duration_since(banned_at)
                        .expect("TODO: deal with the time");

                    if diff >= BAN_LIMIT {
                        None
                    } else {
                        Some(banned_at)
                    }
                });

                if let Some(banned_at) = banned_at {
                    let diff = now
                        .duration_since(banned_at)
                        .expect("TODO: deal with the time");
                    banned_mfs.insert(author_address.ip(), banned_at);
                    let mut author = author.as_ref();
                    let _ = writeln!(
                        author,
                        "You are banned MFS, time left {} seconds ",
                        (BAN_LIMIT - diff).as_secs_f32()
                    );
                    let _ = author.shutdown(std::net::Shutdown::Both);
                } else {
                    let nick = format!("user#{}", author_address.port());
                    let _ = writeln!(author.as_ref(), "YOU {nick}");
                    clients.insert(
                        author_address,
                        Client {
                            conn: author.clone(),
                            last_message: now,
                            strike_count: 0,
                            nick,
                        },
                    );
                }
            }

            Messages::ClientDisconnected { author_addr } => {
                clients.remove(&author_addr);
            }
            Messages::NewMessage { author_addr, line } => {
                let now = SystemTime::now();

                // Rate limit first, then release the mutable borrow.
                let allowed = if let Some(author) = clients.get_mut(&author_addr) {
                    let freq = now.duration_since(author.last_message).expect("TIME STUFF");
                    if freq > MES_FREQ {
                        author.last_message = now;
                        true
                    } else {
                        author.strike_count += 1;
                        if author.strike_count >= BAN_FREQ {
                            println!("author {author_addr} was banned");
                            banned_mfs.insert(author_addr.ip(), now);
                            let _ = write!(author.conn.as_ref(), "You are banned MFs");
                            let _ = author.conn.shutdown(std::net::Shutdown::Both);
                        }
                        false
                    }
                } else {
                    false
                };
                if !allowed {
                    continue;
                }

                println!("author {} send {:?}", Sensitive(author_addr), line);
                let (kind, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));

                match kind {
                    "MSG" => {
                        let Some(nick) =
                            clients.get(&author_addr).map(|client| client.nick.clone())
                        else {
                            continue;
                        };
                        let out = format!("MSG {nick} {rest}\n");
                        broadcast(&clients, author_addr, &out);
                    }
                    "NICK" => {
                        let name = rest.trim();
                        let error = match nickname_error(name) {
                            Some(reason) => Some(reason),
                            None => {
                                let taken = clients.iter().any(|(addr, client)| {
                                    *addr != author_addr && client.nick.eq_ignore_ascii_case(name)
                                });
                                if taken {
                                    Some("nickname already taken")
                                } else {
                                    None
                                }
                            }
                        };

                        match error {
                            Some(reason) => {
                                if let Some(author) = clients.get(&author_addr) {
                                    let _ = writeln!(author.conn.as_ref(), "ERR {reason}");
                                }
                            }
                            None => {
                                let old = match clients.get_mut(&author_addr) {
                                    Some(author) => {
                                        std::mem::replace(&mut author.nick, name.to_string())
                                    }
                                    None => continue,
                                };
                                if let Some(author) = clients.get(&author_addr) {
                                    let _ = writeln!(author.conn.as_ref(), "YOU {name}");
                                }
                                let out = format!("NICK {old} {name}\n");
                                broadcast(&clients, author_addr, &out);
                            }
                        }
                    }
                    _ => {
                        if let Some(author) = clients.get(&author_addr) {
                            let _ = writeln!(author.conn.as_ref(), "ERR unknown command");
                        }
                    }
                }
            }
        }
    }
}

fn authorize(stream: &Arc<TcpStream>, author_address: &SocketAddr, token: &String) -> Result<()> {
    let _ = writeln!(stream.as_ref(), "SYS authenticating").map_err(|err| {
        eprintln!("ERROR: Could not passing message to {author_address}: {err}");
    });
    let mut buffer = [0u8; TOKEN_LEN * 2];
    let mut filled = 0;
    while filled < buffer.len() {
        let n = stream.as_ref().read(&mut buffer[filled..]).map_err(|err| {
            eprintln!("ERROR: Could not read message from {author_address}: {err}");
        })?;
        if n == 0 {
            eprintln!("ERROR: Client {author_address} disconnected during authorization");
            return Err(());
        }
        filled += n;
    }

    let buffer = from_utf8(&buffer).map_err(|err| {
        eprintln!("ERROR: illeagel utf8 token : {err}");
    })?;

    println!("get buffer {buffer}");

    if token != buffer {
        eprintln!("ERROR: Token not valid");
        return Err(());
    }

    Ok(())
}

fn client(stream: Arc<TcpStream>, message_sender: Sender<Messages>, token: String) -> Result<()> {
    let author_address = stream
        .peer_addr()
        .map_err(|err| eprintln!("can not get the peer adderess: {err}"))?;

    authorize(&stream, &author_address, &token).map_err(|()| {
        let _ = write!(stream.as_ref(), "Invalid Token ! ").map_err(|err| {
            eprintln!("ERROR: Invalid Token : {err} ");
        });
        let _ = stream.shutdown(std::net::Shutdown::Both).map_err(|err| {
            eprintln!("ERROR: Could not shutdown the connnection : {err} ");
        });
    })?;

    let _ = writeln!(stream.as_ref(), "SYS Welcome to the club, buddy!").map_err(|err| {
        eprintln!("ERROR: failed to send message to {author_address}: {err} ");
    });

    message_sender
        .send(Messages::ClientConnected {
            author: stream.clone(),
        })
        .map_err(|err| {
            eprintln!("Could not to connected to client : {err}");
        })?;

    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.as_ref().read(&mut chunk).map_err(|err| {
            eprintln!("Disconnected to client : {err}");
            let _ = message_sender.send(Messages::ClientDisconnected {
                author_addr: author_address,
            });
        })?;
        if n == 0 {
            let _ = message_sender
                .send(Messages::ClientDisconnected {
                    author_addr: author_address,
                })
                .map_err(|err| {
                    eprintln!("Could not send message to client : {err}");
                });
            break;
        }

        pending.extend_from_slice(&chunk[..n]);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            if line.is_empty() {
                continue;
            }
            message_sender
                .send(Messages::NewMessage {
                    author_addr: author_address,
                    line,
                })
                .map_err(|err| {
                    eprintln!("Disconnected to client : {err}");
                })?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut buffer = [0; TOKEN_LEN];
    let _ = getrandom::fill(&mut buffer).map_err(|err| {
        eprintln!("ERROR: Generate token failed : {err}");
    });

    let mut token = String::new();

    for x in buffer.iter() {
        let _ = write!(token, "{x:02X}");
    }

    println!("token is {token}");

    let address = "0.0.0.0:6969";
    let listener = TcpListener::bind(address).map_err(|err| {
        eprintln!(
            "can not bind to {} : {}",
            Sensitive(address),
            Sensitive(err)
        )
    })?;

    println!("INFO: Listening in {}", Sensitive(address));

    let (message_sender, message_receiver) = channel();

    thread::spawn(|| server(message_receiver));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let stream = Arc::new(stream);
                let sender = message_sender.clone();
                let token = token.clone();
                thread::spawn(|| client(stream, sender, token));
            }
            Err(e) => {
                eprintln!("disconnect with user : {}", Sensitive(e));
            }
        }
    }
    Ok(())
}
