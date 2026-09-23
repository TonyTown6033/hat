use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, cursor::MoveTo};
use std::io::{self, ErrorKind, Read, Write, stdout};
use std::net::{Shutdown, TcpStream};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    let mut ctx = Ctx {
        stream: None,
        chat: Vec::new(),
        user: "you".to_string(),
        stop: false,
        server: None,
        started_at: Instant::now(),
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
                Event::Key(event) => match event.code {
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
                },

                _ => {}
            }
        }

        // Read from the server if we are connected.
        let mut disconnected = false;
        if let Some(stream) = ctx.stream.as_mut() {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    disconnected = true;
                }
                Ok(n) => {
                    pending.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => {
                    ctx.msg(MsgKind::Error, e.to_string());
                    disconnected = true;
                }
            }
        }

        // Split the accumulated bytes into complete lines and parse them.
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            if !line.is_empty() {
                handle_server_line(&mut ctx, &line);
            }
        }

        if disconnected {
            pending.clear();
            ctx.msg(MsgKind::Warn, "disconnected");
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
