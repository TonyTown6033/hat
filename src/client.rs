use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, cursor::MoveTo};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write, stdout};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

// Semantic category of a chat line, used to pick its color.
#[derive(Clone, Copy)]
enum MsgKind {
    Normal,
    User,
    System,
    Warn,
    Error,
}

struct Message {
    time: String,
    user: String,
    text: String,
    kind: MsgKind,
}

struct PendingDownload {
    name: String,
    save_path: PathBuf,
}

struct Download {
    name: String,
    size: u64,
    received: u64,
    save_path: PathBuf,
    file: File,
}

impl Download {
    fn start(name: &str, size: u64, save_path: &Path) -> io::Result<Self> {
        Ok(Self {
            name: name.to_owned(),
            size,
            received: 0,
            save_path: save_path.to_owned(),
            file: File::create(save_path)?,
        })
    }

    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let remaining = (self.size - self.received) as usize;
        let take = remaining.min(data.len());
        self.file.write_all(&data[..take])?;
        self.received += take as u64;
        Ok(take)
    }

    fn is_complete(&self) -> bool {
        self.received == self.size
    }
}

enum UploadEvent {
    Failed { name: String, reason: String },
}

// C-style command struct: a name, a description, and a function pointer.
struct Command {
    name: &'static str,
    description: &'static str,
    run: fn(&mut Ctx, &[&str]),
}

// State shared between the main loop and command handlers.
struct Ctx {
    stream: Option<TcpStream>,
    chat: Vec<Message>,
    stop: bool,
    user: String,
    server: Option<String>,
    started_at: Instant,
    upload_tx: mpsc::Sender<UploadEvent>,
    pending_download: Option<PendingDownload>,
    download: Option<Download>,
}

impl Ctx {
    // Push a line with a semantic kind so the renderer can color it.
    fn msg(&mut self, kind: MsgKind, text: impl Into<String>) {
        self.msg_from("system", kind, text);
    }

    fn system_msg(&mut self, kind: MsgKind, text: impl Into<String>) {
        self.msg(kind, text);
    }

    // Push a line attributed to an explicit user (e.g. an incoming message).
    fn msg_from(&mut self, user: impl Into<String>, kind: MsgKind, text: impl Into<String>) {
        self.chat.push(Message {
            time: beijing_clock(),
            user: user.into(),
            text: text.into(),
            kind,
        });
    }

    fn disconnect(&mut self) {
        if let Some(download) = self.download.take() {
            let _ = fs::remove_file(&download.save_path);
        }
        self.pending_download = None;
        self.stream = None;
        self.server = None;
    }
}

fn cmd_connect(ctx: &mut Ctx, args: &[&str]) {
    if args.len() < 2 {
        ctx.msg(MsgKind::Warn, "usage: /connect <ip> <port> [token]");
        return;
    }

    if ctx.stream.is_some() {
        ctx.msg(MsgKind::Warn, "You already connected");
        return;
    }

    let address = format!("{}:{}", args[0], args[1]);
    match TcpStream::connect(&address) {
        Ok(mut stream) => {
            if let Err(e) = stream.set_nonblocking(true) {
                ctx.msg(MsgKind::Error, e.to_string());
                return;
            }
            // The server expects the token as the first line.
            if let Some(token) = args.get(2)
                && let Err(e) = stream.write_all(format!("{token}\n").as_bytes())
            {
                ctx.msg(MsgKind::Error, e.to_string());
                return;
            }
            ctx.msg(MsgKind::System, format!("connected to {address}"));
            ctx.stream = Some(stream);
            ctx.server = Some(address);
        }
        Err(e) => {
            ctx.msg(
                MsgKind::Error,
                format!("could not connect to {address}: {e}"),
            );
        }
    }
}

fn cmd_help(ctx: &mut Ctx, _args: &[&str]) {
    for cmd in COMMANDS {
        ctx.msg(
            MsgKind::System,
            format!("/{} - {}", cmd.name, cmd.description),
        );
    }
}

fn cmd_nickname(ctx: &mut Ctx, args: &[&str]) {
    if args.len() != 1 {
        ctx.msg(MsgKind::Warn, "usage: /nickname <name>");
        return;
    }

    match ctx.stream.as_mut() {
        Some(stream) => {
            let out = format!("NICK {}\n", args[0]);
            if let Err(e) = stream.write_all(out.as_bytes()) {
                ctx.msg(MsgKind::Error, e.to_string());
            }
        }
        None => ctx.msg(
            MsgKind::Warn,
            "not connected, use /connect <ip> <port> [token]",
        ),
    }
}

fn cmd_quit(ctx: &mut Ctx, _args: &[&str]) {
    ctx.stop = true;
}

fn cmd_disconnect(ctx: &mut Ctx, _args: &[&str]) {
    if let Some(stream) = ctx.stream.as_mut() {
        if let Err(err) = stream.shutdown(Shutdown::Both) {
            ctx.msg(
                MsgKind::Error,
                format!("Failed to shutdown the connection : {err}"),
            );
        }
        // The read loop notices EOF and clears the stream.
    } else {
        ctx.msg(MsgKind::Warn, "You are not login");
    }
}

fn validate_filename(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.len() > 255 {
        return Some("invalid file name length");
    }
    if name.starts_with('.') {
        return Some("file name cannot start with '.'");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return Some("file name may only contain letters, numbers, '.', '_' and '-'");
    }
    None
}

fn write_all_nonblocking(mut stream: &TcpStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match stream.write(data) {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "connection closed")),
            Ok(n) => data = &data[n..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => sleep(Duration::from_millis(5)),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn upload_file(stream: &TcpStream, name: &str, path: &Path, size: u64) -> io::Result<()> {
    let header = format!("PUT {name} {size}\n");
    write_all_nonblocking(stream, header.as_bytes())?;

    let mut file = File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        write_all_nonblocking(stream, &buffer[..read])?;
    }
    Ok(())
}

fn cmd_upload(ctx: &mut Ctx, args: &[&str]) {
    if args.len() != 1 {
        ctx.system_msg(MsgKind::Warn, "usage: /upload <file>");
        return;
    }
    let Some(stream) = ctx.stream.as_ref() else {
        ctx.system_msg(
            MsgKind::Warn,
            "not connected, use /connect <ip> <port> [token]",
        );
        return;
    };
    let path = PathBuf::from(args[0]);
    if !path.is_file() {
        ctx.system_msg(MsgKind::Error, format!("file not found: {}", args[0]));
        return;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        ctx.system_msg(MsgKind::Error, "invalid file path");
        return;
    };
    if let Some(reason) = validate_filename(name) {
        ctx.system_msg(MsgKind::Error, reason);
        return;
    }
    let Ok(meta) = fs::metadata(&path) else {
        ctx.system_msg(MsgKind::Error, "could not read file metadata");
        return;
    };
    if meta.len() > MAX_FILE_SIZE {
        ctx.system_msg(MsgKind::Error, "file too large (max 16MB)");
        return;
    }
    if ctx.pending_download.is_some() || ctx.download.is_some() {
        ctx.system_msg(MsgKind::Warn, "a file transfer is already in progress");
        return;
    }

    let Ok(stream) = stream.try_clone() else {
        ctx.system_msg(MsgKind::Error, "could not clone connection");
        return;
    };
    let tx = ctx.upload_tx.clone();
    let name_owned = name.to_owned();
    let size = meta.len();
    ctx.system_msg(MsgKind::System, format!("uploading {name} ({size} bytes)"));
    thread::spawn(move || {
        if let Err(error) = upload_file(&stream, &name_owned, &path, size) {
            let _ = tx.send(UploadEvent::Failed {
                name: name_owned,
                reason: error.to_string(),
            });
        }
    });
}

fn cmd_download(ctx: &mut Ctx, args: &[&str]) {
    if args.is_empty() || args.len() > 2 {
        ctx.system_msg(MsgKind::Warn, "usage: /download <name> [save-path]");
        return;
    }
    if ctx.download.is_some() || ctx.pending_download.is_some() {
        ctx.system_msg(MsgKind::Warn, "a file transfer is already in progress");
        return;
    }
    let name = args[0];
    let save_path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(name));
    if save_path.exists() {
        ctx.system_msg(
            MsgKind::Error,
            format!("file already exists: {}", save_path.display()),
        );
        return;
    }
    let Some(stream) = ctx.stream.as_mut() else {
        ctx.system_msg(
            MsgKind::Warn,
            "not connected, use /connect <ip> <port> [token]",
        );
        return;
    };
    ctx.pending_download = Some(PendingDownload {
        name: name.to_owned(),
        save_path,
    });
    if let Err(error) = stream.write_all(format!("GET {name}\n").as_bytes()) {
        ctx.pending_download = None;
        ctx.system_msg(MsgKind::Error, error.to_string());
    }
}

fn cmd_files(ctx: &mut Ctx, _args: &[&str]) {
    let Some(stream) = ctx.stream.as_mut() else {
        ctx.system_msg(
            MsgKind::Warn,
            "not connected, use /connect <ip> <port> [token]",
        );
        return;
    };
    if let Err(error) = stream.write_all(b"LS\n") {
        ctx.system_msg(MsgKind::Error, error.to_string());
    }
}

// Static command table, like an array of structs in C.
const COMMANDS: &[Command] = &[
    Command {
        name: "connect",
        description: "connect to a server: /connect <ip> <port> [token]",
        run: cmd_connect,
    },
    Command {
        name: "disconnect",
        description: "disconnect the server",
        run: cmd_disconnect,
    },
    Command {
        name: "help",
        description: "show this help",
        run: cmd_help,
    },
    Command {
        name: "quit",
        description: "quit the client",
        run: cmd_quit,
    },
    Command {
        name: "nickname",
        description: "change the user name",
        run: cmd_nickname,
    },
    Command {
        name: "upload",
        description: "upload a file: /upload <file>",
        run: cmd_upload,
    },
    Command {
        name: "download",
        description: "download a file: /download <name> [save-path]",
        run: cmd_download,
    },
    Command {
        name: "files",
        description: "list files on the server",
        run: cmd_files,
    },
];

fn complete_command(prompt: &str) -> String {
    let Some(rest) = prompt.strip_prefix('/') else {
        return prompt.to_string();
    };

    // Only complete the command name, not its arguments.
    if rest.contains(char::is_whitespace) {
        return prompt.to_string();
    }

    let matches: Vec<&str> = COMMANDS
        .iter()
        .map(|command| command.name)
        .filter(|name| name.starts_with(rest))
        .collect();

    if matches.len() == 1 {
        format!("/{}", matches[0])
    } else {
        prompt.to_string()
    }
}

fn handle_prompt(ctx: &mut Ctx, prompt: &str) {
    let input = prompt.trim();
    if input.is_empty() {
        return;
    }

    // Commands start with '/'.
    if let Some(rest) = input.strip_prefix('/') {
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();

        match COMMANDS.iter().find(|cmd| cmd.name == name) {
            Some(cmd) => (cmd.run)(ctx, &args),
            None => ctx.msg(MsgKind::Error, format!("unknown command: /{name}")),
        }
    } else {
        match ctx.stream.as_mut() {
            Some(stream) => {
                let out = format!("MSG {input}\n");
                if let Err(e) = stream.write_all(out.as_bytes()) {
                    ctx.msg(MsgKind::Error, e.to_string());
                }
            }
            None => {
                ctx.msg(
                    MsgKind::Warn,
                    "not connected, use /connect <ip> <port> [token]",
                );
            }
        }
    }
}

// Turn one server line into a chat entry.
fn handle_server_line(ctx: &mut Ctx, line: &str) {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    match kind {
        "MSG" => {
            let (nick, text) = rest.split_once(' ').unwrap_or((rest, ""));
            ctx.msg_from(nick, MsgKind::Normal, text);
        }
        "NICK" => {
            let (old, new) = rest.split_once(' ').unwrap_or((rest, ""));
            ctx.system_msg(MsgKind::Warn, format!("{old} is now known as {new}"));
        }
        "YOU" => {
            ctx.user = rest.to_string();
            ctx.system_msg(MsgKind::System, format!("you are now {rest}"));
        }
        "SYS" => ctx.system_msg(MsgKind::System, rest),
        "ERR" => ctx.system_msg(MsgKind::Error, rest),
        "OK" => ctx.system_msg(MsgKind::System, format!("uploaded {rest}")),
        "FILES" => {
            let rest = rest.trim();
            if rest.is_empty() {
                ctx.system_msg(MsgKind::System, "(no files)");
            } else {
                for entry in rest.split(',') {
                    if let Some((name, size)) = entry.rsplit_once(':') {
                        ctx.system_msg(MsgKind::System, format!("{name} ({size} bytes)"));
                    }
                }
            }
        }
        other => ctx.system_msg(MsgKind::Normal, other),
    }
}

// Fixed palette, independent of the terminal's own color theme.
const BG: Color = Color::Rgb {
    r: 255,
    g: 255,
    b: 255,
};
const FG_NORMAL: Color = Color::Rgb { r: 0, g: 110, b: 0 };
const FG_USER: Color = Color::Rgb { r: 0, g: 0, b: 0 };
const FG_SYSTEM: Color = Color::Rgb {
    r: 0,
    g: 110,
    b: 130,
};
const FG_WARN: Color = Color::Rgb {
    r: 180,
    g: 110,
    b: 0,
};
const FG_ERROR: Color = Color::Rgb { r: 190, g: 0, b: 0 };
const FG_PROMPT: Color = Color::Rgb { r: 0, g: 0, b: 0 };
const WELCOME_ART: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣶⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣴⣦⡀⢠⣾⣿⡟⠀⠀⢀⣀⣀⣠⣤⣄⣀⣀⣀⣀⡀⠀⠀⠀⢸⣿⣦⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢀⣼⣿⣻⣿⣽⣿⣿⣿⣷⢶⡟⠉⠛⣋⣀⠀⠀⠁⠀⠉⠉⠹⢻⣶⣤⣸⣿⣿⣿⡀⣀⣤⣄⣀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⣠⣾⣿⣿⣿⣿⣿⣿⡟⢁⣥⠴⠃⠀⠀⠀⠀⠀⠀⠀⠈⠈⠀⠀⠁⠈⠉⠹⠿⣿⣿⣿⡟⠉⠹⢿⣧⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⣠⣾⣿⣿⣿⣿⣿⣿⣿⣫⡆⠀⠁⠀⠆⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣤⡀⠉⣿⣿⣦⣤⡀⠘⣿⡀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⢀⡾⠿⣿⣿⣿⣿⢛⣽⣿⡿⣿⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⣷⡀⢔⣿⣿⣿⣿⣦⣿⡇⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠈⢀⣾⣷⣿⢹⣷⣿⣿⣿⢹⡗⠀⢠⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡀⠀⠘⠀⠀⠀⠀⢸⣷⡆⠛⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀",
    "⠀⠀⠀⢀⣾⠏⣽⡏⣿⣿⣿⣿⡧⡟⠀⣴⡟⣾⠀⣠⣄⣄⡀⢀⣠⣴⣄⣄⡀⠀⠀⠀⠀⣀⣀⣾⠟⣷⢁⣿⣿⣿⣿⣿⣿⡇⠀⠀⠀⠀",
    "⠀⠀⠀⣸⣿⣼⡟⢸⣿⣿⣿⣿⠀⠀⠀⣿⣳⢃⢸⣇⣿⢩⣿⡏⣿⠉⠉⠙⣷⢀⠠⣤⣼⡏⣿⠛⠎⣿⣿⣿⣿⣷⠙⢿⣿⣷⠀⠀⠀⠀",
    "⠀⠀⢰⡿⢸⣿⠃⢸⣿⣿⣿⡇⠀⠀⢀⣿⠀⢸⡿⣿⣿⡟⠛⣿⡇⠀⠀⠀⣿⠘⣦⣿⠀⢣⣿⠀⠀⠈⣿⣿⣿⣿⡆⢘⠹⣿⣇⠀⠀⠀",
    "⠀⠀⢸⡇⣸⣿⠀⠘⠛⣿⣿⣿⠀⠀⢸⡟⣗⡾⠤⣿⡏⢻⠀⣽⡇⠀⠀⠀⣿⠀⣹⡟⣧⢸⣿⠀⠀⠀⢹⣿⣿⣿⣷⠸⡄⣿⢻⣇⠀⠀",
    "⠀⠀⠘⢧⡟⣿⠀⠀⠀⠈⢿⣿⠀⣠⣼⣴⠿⣷⣦⣤⣓⢮⣷⠀⠱⡀⠀⠘⢻⢰⢻⣀⣿⣻⢹⡃⠀⠀⠘⡟⠻⣿⣿⡄⡇⣿⠀⢹⡀⠀",
    "⠀⠀⠀⣸⣇⣸⡇⠀⠀⠀⡆⣿⣧⢠⢹⠉⢂⠘⣿⠿⣟⠳⠜⠗⠀⠀⡼⢴⣞⣻⣯⣍⣏⡁⣼⠁⠀⠀⠀⡇⡄⠀⠹⣷⡇⣿⠀⠀⡇⠀",
    "⠀⠀⠀⣿⠁⢹⡇⠀⠀⠀⡇⣿⣎⢯⣻⣧⠀⠓⠚⠛⠁⠀⠀⠀⠀⠀⠀⠋⢹⠿⣿⡭⢉⠿⠷⢦⡆⠀⣇⡇⣗⠀⠀⢹⡇⣿⠀⠀⠁⠀",
    "⠀⠀⠀⢻⡀⢸⠁⠀⠀⠀⢳⢻⣿⣷⣧⣯⡳⢤⣀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠙⠲⠖⢀⣼⡴⢿⠁⣰⣿⣄⣿⠀⠀⠘⠇⢻⠀⠀⠀⠀",
    "⠀⠀⠀⠈⢧⢸⠀⠀⠀⠀⠈⠈⠙⣿⣿⣿⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⣩⢋⣴⠏⣰⣿⣿⡄⣯⠀⠀⠀⠀⠈⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠈⢻⡆⠀⠀⠀⠀⠀⠀⢻⣿⣿⣿⣄⠀⠀⠀⣀⠴⠖⢤⡀⠀⠀⠀⠀⠀⣴⣿⣿⣟⣴⣿⣿⣿⣇⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⣇⠀⠀⠀⠀⠀⠀⡈⣿⣿⣿⣿⣷⣄⠀⠸⣄⣀⣠⠇⠀⠀⣀⣤⣾⣿⣿⣿⣿⣿⣿⣿⡟⣿⢹⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⣿⠀⠀⠀⠀⠀⠀⢱⢹⣿⣿⣿⣿⣧⣹⣦⣄⣀⣠⣴⣶⠿⢿⣿⣿⡟⡿⠟⠋⠉⠉⠙⠧⣿⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⡿⠀⠀⠀⠀⠀⠀⢸⡼⣿⣿⣿⣿⣿⡀⠀⠉⠛⠛⠋⠀⠐⢾⣿⠿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠙⣧⠀⠀⠀⣾⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⢸⠃⠀⠀⠀⠀⠀⠀⢸⡇⣿⣿⡿⣿⡿⠇⠀⠀⠀⣀⡤⠖⠚⠉⠀⠀⠀⠐⢦⣀⠀⠀⠀⠀⠀⠀⠘⡄⠀⠀⠀⢇⠀⠀⠀",
    "⠀⠀⠀⠀⢠⡏⠀⠀⠀⠀⠀⠀⢀⣸⣷⡿⠿⠓⠋⠐⠒⠒⢶⢞⡁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠑⢤⡀⠀⠀⠀⠀⢳⠀⠀⠀⠘⣆⠀⠀",
    "⠀⠀⠀⠀⢠⠏⣀⠀⠀⠀⠀⣠⠞⠉⠀⡠⠀⠀⠀⠀⢀⡰⠀⣚⡽⠚⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⢆⠀⠀⠀⢸⡀⠀⠀⠀⠘⢆⠀",
    "⠀⠀⠀⢰⣯⠞⠁⡏⠀⢀⡞⣡⡤⠖⠋⠒⠉⠭⠉⠉⠁⣦⡞⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢧⠀⠀⠈⡇⠀⠀⠀⠀⠈⢦",
    "⠀⠀⣰⣿⠋⠀⢸⡇⣠⠞⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀⢰⢹⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⡀⠀⢰⠃⠀⠀⠀⠀⠀⠀",
    "⣰⡹⠁⠀⠀⢈⡷⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⠸⡆⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⣧⠀⣼⠀⠀⠀⠀⠀⠀⠀",
    "⣿⠇⠀⠀⣠⡿⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣇⢹⡆⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⡟⢦⢸⡀⠀⠀⠀⠀⠀⠀",
    "⡿⠀⡠⢊⠇⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⣉⣡⣤⣤⡤⠤⢤⣤⣀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣾⠘⢇⠀⢷⠀⠀⠀⠀⠀⠀",
    "⣧⠞⠀⡜⠀⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⡤⠖⠛⠉⠁⠀⠀⠀⠀⠀⠀⠀⠈⠉⠓⠦⢤⣀⣀⣀⠀⣰⠃⠀⠸⡄⣸⠀⠀⠀⠀⠀⠀",
    "⠃⠀⢸⢣⠃⢻⡆⠀⠀⠀⠀⠀⠀⢀⣴⡊⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠙⣍⣰⡏⠀⠀⠀⣷⠃⠀⠀⠀⠀⠀⠀",
    "⠀⠀⢸⡄⠀⠀⠻⣄⠀⠀⢀⣠⠾⠛⠉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⠃⠀⠀⢰⡟⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠨⡇⠀⠀⠐⣼⣷⠶⠿⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢘⡿⠀⠀⠀⠈⡇⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⡃⠀⠀⠀⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣸⠇⠀⠀⠀⠐⡇⠀⠀⠀⠀⠀⠀⠐",
    "⠀⠀⠀⠁⠀⠀⠀⣽⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⡟⠀⠀⠀⠀⢰⡇⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠸⣿⡆⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣿⠃⠀⠀⠀⠀⢨⡇⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⢰⡟⣷⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢿⣿⠀⠀⠀⠀⠀⡾⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠘⡗⢸⡆⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢿⡀⠀⠀⠀⣰⠇⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀遠 坂 凛 / RIN TOHSAKA⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

fn kind_color(kind: MsgKind) -> Color {
    match kind {
        MsgKind::Normal => FG_NORMAL,
        MsgKind::User => FG_USER,
        MsgKind::System => FG_SYSTEM,
        MsgKind::Warn => FG_WARN,
        MsgKind::Error => FG_ERROR,
    }
}

fn draw_welcome(stdout: &mut impl Write, width: usize, height: usize) -> io::Result<()> {
    let art_height = WELCOME_ART.len().min(height.saturating_sub(2));
    let y = height.saturating_sub(art_height + 2) / 2;

    for (row, art) in WELCOME_ART.iter().take(art_height).enumerate() {
        let line: String = art.chars().take(width).collect();
        let x = width.saturating_sub(line.chars().count()) / 2;
        stdout
            .queue(MoveTo(x as u16, (y + row) as u16))?
            .queue(SetForegroundColor(FG_SYSTEM))?
            .write_all(line.as_bytes())?;
    }

    if height > 1 {
        let hint = "Press any key to enter HAT";
        let line: String = hint.chars().take(width).collect();
        let x = width.saturating_sub(line.chars().count()) / 2;
        stdout
            .queue(MoveTo(x as u16, height.saturating_sub(1) as u16))?
            .queue(SetForegroundColor(FG_ERROR))?
            .write_all(line.as_bytes())?;
    }
    Ok(())
}

fn chat_window(stdout: &mut impl Write, chat: &[Message], boundary: Rect) -> io::Result<()> {
    let n = chat.len();
    let size = n.saturating_sub(boundary.h);
    for (dy, msg) in chat.iter().skip(size).enumerate() {
        let full = format!(
            "[{time}] <{user}> : {text}",
            time = msg.time,
            user = msg.user,
            text = msg.text
        );
        let line: String = full.chars().take(boundary.w).collect();
        stdout
            .queue(MoveTo(boundary.x as u16, (boundary.y + dy) as u16))?
            .queue(SetForegroundColor(kind_color(msg.kind)))?
            .write_all(line.as_bytes())?;
    }
    Ok(())
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn beijing_clock() -> String {
    // Beijing time is UTC+8 with no daylight saving time.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 8 * 3600;
    let day = secs % 86400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

// Build the status line: state, server, uptime and a Beijing clock.
fn status_bar(ctx: &Ctx, width: usize) -> (String, Color) {
    let online = ctx.stream.is_some();
    let (state, color) = if online {
        ("ONLINE", FG_NORMAL)
    } else {
        ("OFFLINE", FG_ERROR)
    };

    let server = ctx.server.as_deref().unwrap_or("not connected");
    let left = format!("[{state}] {server}");
    let right = format!(
        "up {} | {} CST",
        format_duration(ctx.started_at.elapsed()),
        beijing_clock()
    );

    let line = match width.checked_sub(left.chars().count() + right.chars().count()) {
        Some(gap) if gap > 0 => format!("{left}{}{right}", " ".repeat(gap)),
        _ => format!("{left} {right}"),
    };
    let line: String = line.chars().take(width).collect();
    (line, color)
}

fn main() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    stdout.queue(SetBackgroundColor(BG))?;
    stdout.flush()?;
    let (mut w, mut h) = terminal::size()?;
    let barchar = "─";
    let mut bar = barchar.repeat(w as usize);

    let (upload_tx, upload_rx) = mpsc::channel();
    let mut ctx = Ctx {
        stream: None,
        chat: Vec::new(),
        user: "you".to_string(),
        stop: false,
        server: None,
        started_at: Instant::now(),
        upload_tx,
        pending_download: None,
        download: None,
    };
    let mut prompt = String::new();
    let mut show_welcome = true;

    let mut buffer = [0; 64];
    let mut pending: Vec<u8> = Vec::new();

    while !ctx.stop {
        while poll(Duration::ZERO).unwrap() {
            match read()? {
                Event::Resize(width, height) => {
                    w = width;
                    h = height;
                    bar = barchar.repeat(w as usize);
                }
                Event::Paste(data) => {
                    if show_welcome {
                        show_welcome = false;
                    } else {
                        prompt.push_str(&data);
                    }
                }
                Event::Key(event) if show_welcome => {
                    if event.modifiers.contains(KeyModifiers::CONTROL)
                        && event.code == KeyCode::Char('c')
                    {
                        ctx.stop = true;
                    } else {
                        show_welcome = false;
                    }
                }
                Event::Key(event) => {
                    if event.is_press() {
                        match event.code {
                            KeyCode::Char(code) => {
                                if event.modifiers.contains(KeyModifiers::CONTROL) && code == 'c' {
                                    ctx.stop = true;
                                } else {
                                    prompt.push(code);
                                }
                            }
                            KeyCode::Esc => {
                                prompt.clear();
                            }
                            KeyCode::Enter => {
                                let line = prompt.clone();
                                ctx.msg_from(ctx.user.clone(), MsgKind::User, prompt.clone());
                                handle_prompt(&mut ctx, &line);
                                prompt.clear();
                            }
                            KeyCode::Tab => {
                                prompt = complete_command(&prompt);
                            }
                            KeyCode::Backspace => {
                                prompt.pop();
                            }

                            _ => {}
                        }
                    }
                }

                _ => {}
            }
        }

        while let Ok(event) = upload_rx.try_recv() {
            match event {
                UploadEvent::Failed { name, reason } => {
                    ctx.system_msg(MsgKind::Error, format!("upload of {name} failed: {reason}"));
                }
            }
        }

        // Read from the server if we are connected.
        let mut disconnected = false;
        let read_result = ctx.stream.as_mut().map(|stream| stream.read(&mut buffer));

        match read_result {
            None => {}
            Some(Ok(0)) => disconnected = true,
            Some(Ok(n)) => {
                let data = &buffer[..n];
                if ctx.download.is_some() {
                    let result = {
                        let download = ctx.download.as_mut().unwrap();
                        download.write(data)
                    };
                    match result {
                        Ok(consumed) => {
                            if consumed < data.len() {
                                pending.extend_from_slice(&data[consumed..]);
                            }
                        }
                        Err(error) => {
                            let download = ctx.download.take().unwrap();
                            let _ = fs::remove_file(&download.save_path);
                            ctx.pending_download = None;
                            ctx.system_msg(MsgKind::Error, format!("download failed: {error}"));
                        }
                    }
                } else {
                    pending.extend_from_slice(data);
                }
            }
            Some(Err(error)) if error.kind() == ErrorKind::WouldBlock => {}
            Some(Err(error)) => {
                ctx.system_msg(MsgKind::Error, error.to_string());
                disconnected = true;
            }
        }

        if ctx.download.as_ref().is_some_and(Download::is_complete) {
            let done = ctx.download.take().unwrap();
            ctx.pending_download = None;
            ctx.system_msg(
                MsgKind::System,
                format!("saved {} ({} bytes)", done.name, done.size),
            );
        }

        // Split the accumulated bytes into complete lines and parse them.
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            if line.is_empty() {
                continue;
            }

            if let Some(rest) = line.strip_prefix("FILE ") {
                let Some((name, size_text)) = rest.rsplit_once(' ') else {
                    ctx.system_msg(MsgKind::Error, "bad FILE header from server");
                    disconnected = true;
                    break;
                };
                let Ok(size) = size_text.parse::<u64>() else {
                    ctx.system_msg(MsgKind::Error, "bad file size from server");
                    disconnected = true;
                    break;
                };
                if size > MAX_FILE_SIZE {
                    ctx.system_msg(MsgKind::Error, "server sent an oversized file");
                    disconnected = true;
                    break;
                }

                let save_path = ctx
                    .pending_download
                    .as_ref()
                    .filter(|pending| pending.name == name)
                    .map(|pending| pending.save_path.clone())
                    .unwrap_or_else(|| PathBuf::from(name));

                match Download::start(name, size, &save_path) {
                    Ok(download) => {
                        ctx.download = Some(download);
                        ctx.pending_download = None;
                    }
                    Err(error) => {
                        ctx.system_msg(MsgKind::Error, format!("could not save file: {error}"));
                        disconnected = true;
                        break;
                    }
                }

                if !pending.is_empty() {
                    let result = {
                        let download = ctx.download.as_mut().unwrap();
                        download.write(&pending)
                    };
                    match result {
                        Ok(consumed) => {
                            pending.drain(..consumed);
                        }
                        Err(error) => {
                            let download = ctx.download.take().unwrap();
                            let _ = fs::remove_file(&download.save_path);
                            ctx.pending_download = None;
                            ctx.system_msg(MsgKind::Error, format!("download failed: {error}"));
                        }
                    };
                }
                break;
            }

            handle_server_line(&mut ctx, &line);
        }

        if disconnected {
            pending.clear();
            if ctx.download.is_some() {
                ctx.system_msg(MsgKind::Warn, "download aborted");
            }
            ctx.system_msg(MsgKind::Warn, "disconnected");
            ctx.disconnect();
        }

        stdout.queue(SetBackgroundColor(BG))?;
        stdout.queue(Clear(ClearType::All))?;

        if show_welcome {
            draw_welcome(&mut stdout, w as usize, h as usize)?;
            stdout.flush()?;
            sleep(Duration::from_millis(33));
            continue;
        }

        let chat_h = (h as usize).saturating_sub(3);
        chat_window(
            &mut stdout,
            &ctx.chat,
            Rect {
                x: 0,
                y: 0,
                w: w as usize,
                h: chat_h,
            },
        )?;

        if h >= 3 {
            stdout.queue(MoveTo(0, h - 3))?;
            stdout
                .queue(SetForegroundColor(FG_NORMAL))?
                .write_all(bar.as_bytes())?;
        }

        if h >= 2 {
            stdout.queue(MoveTo(0, h - 2))?;
            let (status, color) = status_bar(&ctx, w as usize);
            stdout
                .queue(SetForegroundColor(color))?
                .write_all(status.as_bytes())?;
        }

        if h >= 1 {
            stdout.queue(MoveTo(0, h - 1))?;
            stdout.queue(SetForegroundColor(FG_PROMPT))?;
            stdout.write_all(b"> ")?;
            stdout.write_all(prompt.as_bytes())?;
        }

        stdout.flush()?;
        sleep(Duration::from_millis(33));
    }
    stdout.queue(ResetColor)?;
    stdout.flush()?;
    terminal::disable_raw_mode()?;
    Ok(())
}
