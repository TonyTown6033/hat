use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, cursor::MoveTo};
use std::io::{self, ErrorKind, Read, Write, stdout};
use std::net::{Shutdown, TcpStream};
use std::str::from_utf8;
use std::thread::sleep;
use std::time::Duration;

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

// C-style command representation used by the client dispatcher.
struct Command {
    name: &'static str,
    description: &'static str,
    run: fn(&mut Ctx, &[&str]),
}

struct Ctx {
    stream: Option<TcpStream>,
    chat: Vec<String>,
    stop: bool,
}

impl Ctx {
    fn message(&mut self, message: impl Into<String>) {
        self.chat.push(message.into());
    }
}

fn cmd_help(ctx: &mut Ctx, _args: &[&str]) {
    for command in COMMANDS {
        ctx.message(format!("/{} - {}", command.name, command.description));
    }
}

fn cmd_quit(ctx: &mut Ctx, _args: &[&str]) {
    ctx.stop = true;
}

fn cmd_disconnect(ctx: &mut Ctx, _args: &[&str]) {
    match ctx.stream.take() {
        Some(stream) => {
            if let Err(error) = stream.shutdown(Shutdown::Both) {
                ctx.message(format!("failed to disconnect: {error}"));
            } else {
                ctx.message("disconnected");
            }
        }
        None => ctx.message("not connected"),
    }
}

fn cmd_nickname(ctx: &mut Ctx, args: &[&str]) {
    if args.len() != 1 {
        ctx.message("usage: /nickname <name>");
        return;
    }
    match ctx.stream.as_mut() {
        Some(stream) => {
            if let Err(error) = stream.write_all(format!("NICK {}\n", args[0]).as_bytes()) {
                ctx.message(error.to_string());
            }
        }
        None => ctx.message("not connected"),
    }
}

const COMMANDS: &[Command] = &[
    Command {
        name: "help",
        description: "show this help",
        run: cmd_help,
    },
    Command {
        name: "quit",
        description: "exit the client",
        run: cmd_quit,
    },
    Command {
        name: "disconnect",
        description: "disconnect from the server",
        run: cmd_disconnect,
    },
    Command {
        name: "connect",
        description: "connect to server",
        run: cmd_connect,
    },
    Command {
        name: "nickname",
        description: "change your nickname",
        run: cmd_nickname,
    },
];

fn handle_prompt(ctx: &mut Ctx, prompt: &str) {
    let input = prompt.trim();
    if input.is_empty() {
        return;
    }

    if let Some(rest) = input.strip_prefix('/') {
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let args: Vec<&str> = parts.collect();
        match COMMANDS.iter().find(|command| command.name == name) {
            Some(command) => (command.run)(ctx, &args),
            None => ctx.message(format!("unknown command: /{name}")),
        }
        return;
    }

    match ctx.stream.as_mut() {
        Some(stream) => {
            // Regular input is a chat message on the line protocol.
            if let Err(error) = stream.write_all(format!("MSG {input}\n").as_bytes()) {
                ctx.message(error.to_string());
            }
        }
        None => ctx.message("not connected"),
    }
}

// Turn one server line into a chat entry.
fn handle_server_line(ctx: &mut Ctx, line: &str) {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    match kind {
        "MSG" => {
            let (nick, text) = rest.split_once(' ').unwrap_or((rest, ""));
            ctx.message(format!("<{nick}> {text}"));
        }
        "NICK" => {
            let (old, new) = rest.split_once(' ').unwrap_or((rest, ""));
            ctx.message(format!("{old} is now known as {new}"));
        }
        "YOU" => ctx.message(format!("you are now {rest}")),
        "SYS" => ctx.message(rest),
        "ERR" => ctx.message(format!("error: {rest}")),
        _ => ctx.message(line.to_owned()),
    }
}

fn chat_window(stdout: &mut impl Write, chat: &[String], boundary: Rect) -> io::Result<()> {
    let n = chat.len();
    let size = n.checked_sub(boundary.h).unwrap_or(0);
    for (dy, line) in chat.iter().skip(size).enumerate() {
        let bytes = line.as_bytes();
        stdout
            .queue(MoveTo(boundary.x as u16, (boundary.y + dy) as u16))?
            .write_all(bytes.get(0..boundary.w).unwrap_or(bytes))?;
    }
    Ok(())
}

fn cmd_connect(ctx: &mut Ctx, args: &[&str]) {
    if let Some(_stream) = ctx.stream.as_mut() {
        ctx.message("You already connected ");
        return;
    }
    // args is ip port [token]
    if args.len() < 2 {
        ctx.message("/connect <ip> <port> [token]");
        return;
    }
    let addr = format!("{}:{}", args[0], args[1]);
    let mut stream = match TcpStream::connect(&addr) {
        Ok(stream) => stream,
        Err(err) => {
            ctx.message(format!("failed to connect to {} : {}", addr, err));
            return;
        }
    };
    // The server expects the access token as the first line.
    if let Some(token) = args.get(2)
        && let Err(error) = stream.write_all(format!("{token}\n").as_bytes())
    {
        ctx.message(error.to_string());
        return;
    }
    if let Err(err) = stream.set_nonblocking(true) {
        ctx.message(format!("failed to set noblock to {} : {}", addr, err));
        return;
    }
    ctx.stream = Some(stream);
}

fn main() -> io::Result<()> {
    // Read the token printed by the server before entering raw mode.

    terminal::enable_raw_mode()?;
    let result = run_client();
    let _ = terminal::disable_raw_mode();
    result
}

fn run_client() -> io::Result<()> {
    let mut stdout = stdout();
    let (mut w, mut h) = terminal::size()?;
    let barchar = "─";
    let mut bar = barchar.repeat(w as usize);
    let mut ctx = Ctx {
        stream: None,
        chat: Vec::new(),
        stop: false,
    };
    let mut prompt = String::new();
    let mut pending = String::new();
    let mut buffer = [0; 4096];

    while !ctx.stop {
        while poll(Duration::ZERO).unwrap_or(false) {
            match read()? {
                Event::Resize(width, height) => {
                    w = width;
                    h = height;
                    bar = barchar.repeat(w as usize);
                }
                Event::Paste(data) => prompt.push_str(&data),
                Event::Key(event) => match event.code {
                    KeyCode::Char(code) => {
                        if event.modifiers.contains(KeyModifiers::CONTROL) && code == 'c' {
                            ctx.stop = true;
                        } else {
                            prompt.push(code);
                        }
                    }
                    KeyCode::Esc => prompt.clear(),
                    KeyCode::Backspace => {
                        prompt.pop();
                    }
                    KeyCode::Enter => {
                        let line = prompt.clone();
                        ctx.chat.push(line.clone());
                        handle_prompt(&mut ctx, &line);
                        prompt.clear();
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        if let Some(stream) = ctx.stream.as_mut() {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    ctx.message("disconnected");
                    ctx.stream = None;
                    pending.clear();
                }
                Ok(n) => match from_utf8(&buffer[..n]) {
                    Ok(text) => {
                        // The protocol is line-based, so buffer partial reads.
                        pending.push_str(text);
                        while let Some(newline) = pending.find('\n') {
                            let line: String = pending.drain(..=newline).collect();
                            handle_server_line(&mut ctx, line.trim_end_matches(['\r', '\n']));
                        }
                    }
                    Err(error) => ctx.message(format!("invalid server response: {error}")),
                },
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    ctx.message(error.to_string());
                    ctx.stream = None;
                    pending.clear();
                }
            }
        }

        stdout.queue(Clear(ClearType::All))?;
        chat_window(
            &mut stdout,
            &ctx.chat,
            Rect {
                x: 0,
                y: 0,
                w: w as usize,
                h: (h - 2) as usize,
            },
        )?;
        stdout.queue(MoveTo(0, h - 2))?;
        stdout.write_all(bar.as_bytes())?;
        stdout.queue(MoveTo(0, h - 1))?;
        stdout.write_all(prompt.as_bytes())?;
        stdout.flush()?;
        sleep(Duration::from_millis(33));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            stream: None,
            chat: Vec::new(),
            stop: false,
        }
    }

    #[test]
    fn command_table_contains_only_minimal_commands() {
        assert_eq!(
            COMMANDS
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec!["help", "quit", "disconnect", "connect", "nickname"]
        );
    }

    #[test]
    fn unknown_command_is_reported() {
        let mut ctx = ctx();
        handle_prompt(&mut ctx, "/missing");
        assert_eq!(ctx.chat, vec!["unknown command: /missing"]);
    }

    #[test]
    fn server_message_is_rendered_with_nickname() {
        let mut ctx = ctx();
        handle_server_line(&mut ctx, "MSG alice hello there");
        assert_eq!(ctx.chat, vec!["<alice> hello there"]);
    }

    #[test]
    fn server_protocol_lines_are_rendered() {
        let mut ctx = ctx();
        handle_server_line(&mut ctx, "YOU user-1234");
        handle_server_line(&mut ctx, "NICK user-1234 alice");
        handle_server_line(&mut ctx, "ERR nickname is already in use");
        assert_eq!(
            ctx.chat,
            vec![
                "you are now user-1234",
                "user-1234 is now known as alice",
                "error: nickname is already in use",
            ]
        );
    }
}
