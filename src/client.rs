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

// C-style command struct: a name, a description, and a function pointer.
struct Command {
    name: &'static str,
    description: &'static str,
    run: fn(&mut Ctx, &[&str]),
}

// State shared between the main loop and command handlers.
struct Ctx {
    stream: Option<TcpStream>,
    chat: Vec<String>,
    stop: bool,
}

fn cmd_connect(ctx: &mut Ctx, args: &[&str]) {
    if args.len() < 2 {
        ctx.chat.push("usage: /connect <ip> <port>".to_string());
        return;
    }

    if ctx.stream.is_some() {
        ctx.chat.push("You already connected".to_string());
        return;
    }

    let address = format!("{}:{}", args[0], args[1]);
    match TcpStream::connect(&address) {
        Ok(stream) => {
            if let Err(e) = stream.set_nonblocking(true) {
                ctx.chat.push(e.to_string());
                return;
            }
            ctx.chat.push(format!("connected to {address}"));
            ctx.stream = Some(stream);
        }
        Err(e) => {
            ctx.chat
                .push(format!("could not connect to {address}: {e}"));
        }
    }
}

fn cmd_help(ctx: &mut Ctx, _args: &[&str]) {
    for cmd in COMMANDS {
        ctx.chat
            .push(format!("/{} - {}", cmd.name, cmd.description));
    }
}

fn cmd_quit(ctx: &mut Ctx, _args: &[&str]) {
    ctx.stop = true;
}

fn cmd_disconnect(ctx: &mut Ctx, _args: &[&str]) {
    if let Some(stream) = ctx.stream.as_mut() {
        let _ = stream.shutdown(Shutdown::Write).map_err(|err| {
            ctx.chat
                .push(format!("Failed to shutdown the connection : {err}"));
        });
    } else {
        ctx.chat.push("You are not login".to_string());
    }
}

// Static command table, like an array of structs in C.
const COMMANDS: &[Command] = &[
    Command {
        name: "connect",
        description: "connect to a server: /connect <ip> <port>",
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
            None => ctx.chat.push(format!("unknown command: /{name}")),
        }
    } else {
        match ctx.stream.as_mut() {
            Some(stream) => {
                if let Err(e) = stream.write(input.as_bytes()) {
                    ctx.chat.push(e.to_string());
                }
            }
            None => {
                ctx.chat
                    .push("not connected, use /connect <ip> <port>".to_string());
            }
        }
    }
}

fn chat_window(stdout: &mut impl Write, chat: &[String], boundary: Rect) -> io::Result<()> {
    let n = chat.len();
    let size = n.checked_sub(boundary.h).unwrap_or(0);
    for (dy, line) in chat.iter().skip(size).enumerate() {
        let bytes = line.as_bytes();
        stdout
            .queue(MoveTo(boundary.x as u16, (boundary.y + dy) as u16))?
            .write(bytes.get(0..boundary.w).unwrap_or(bytes))?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let _ = terminal::enable_raw_mode()?;
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

    let mut buffer = [0; 64];

    while !ctx.stop {
        while poll(Duration::ZERO).unwrap() {
            match read()? {
                Event::Resize(width, height) => {
                    w = width;
                    h = height;
                    bar = barchar.repeat(w as usize);
                }
                Event::Paste(data) => {
                    prompt.push_str(&data);
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
                        ctx.chat.push(prompt.clone());
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
                    ctx.chat.push("disconnected".to_string());
                    disconnected = true;
                }
                Ok(n) => {
                    ctx.chat.push(from_utf8(&buffer[0..n]).unwrap().to_string());
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => {
                    ctx.chat.push(e.to_string());
                    disconnected = true;
                }
            }
        }
        if disconnected {
            ctx.stream = None;
        }

        stdout.queue(Clear(ClearType::All))?;

        let _ = chat_window(
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
        stdout.write(bar.as_bytes())?;
        stdout.queue(MoveTo(0, h - 1))?;
        stdout.write(prompt.as_bytes())?;
        stdout.flush()?;
        sleep(Duration::from_millis(33));
    }
    let _ = terminal::disable_raw_mode()?;
    Ok(())
}
