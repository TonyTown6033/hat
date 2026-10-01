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
            if let Err(error) = stream.write_all(input.as_bytes()) {
                ctx.message(error.to_string());
            }
        }
        None => ctx.message("not connected"),
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

fn main() -> io::Result<()> {
    // Read the token printed by the server before entering raw mode.
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    let mut stream = TcpStream::connect("127.0.0.1:6969").expect("Can not connect to host");
    stream.set_nonblocking(true).expect("set block failed");
    stream.write_all(token.as_bytes())?;

    terminal::enable_raw_mode()?;
    let result = run_client(stream);
    let _ = terminal::disable_raw_mode();
    result
}

fn run_client(stream: TcpStream) -> io::Result<()> {
    let mut stdout = stdout();
    let (mut w, mut h) = terminal::size()?;
    let barchar = "─";
    let mut bar = barchar.repeat(w as usize);
    let mut ctx = Ctx {
        stream: Some(stream),
        chat: Vec::new(),
        stop: false,
    };
    let mut prompt = String::new();
    let mut buffer = [0; 64];

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
                }
                Ok(n) => match from_utf8(&buffer[..n]) {
                    Ok(message) => ctx.message(message),
                    Err(error) => ctx.message(format!("invalid server response: {error}")),
                },
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    ctx.message(error.to_string());
                    ctx.stream = None;
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
                h: (h.saturating_sub(2)) as usize,
            },
        )?;
        stdout.queue(MoveTo(0, h.saturating_sub(2)))?;
        stdout.write_all(bar.as_bytes())?;
        stdout.queue(MoveTo(0, h.saturating_sub(1)))?;
        stdout.write_all(prompt.as_bytes())?;
        stdout.flush()?;
        sleep(Duration::from_millis(33));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_table_contains_only_minimal_commands() {
        assert_eq!(
            COMMANDS
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec!["help", "quit", "disconnect"]
        );
    }

    #[test]
    fn unknown_command_is_reported() {
        let mut ctx = Ctx {
            stream: None,
            chat: Vec::new(),
            stop: false,
        };
        handle_prompt(&mut ctx, "/missing");
        assert_eq!(ctx.chat, vec!["unknown command: /missing"]);
    }
}
