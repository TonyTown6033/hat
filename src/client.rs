use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, cursor::MoveTo};
use std::io::{self, ErrorKind, Read, Write, stdout};
use std::net::TcpStream;
use std::str::from_utf8;
use std::thread::sleep;
use std::time::Duration;

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
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
    let mut stream = TcpStream::connect("127.0.0.1:6969").expect("Can not connect to host");
    let _ = stream.set_nonblocking(true).expect("set block failed ");
    let _ = terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    let (mut w, mut h) = terminal::size()?;
    let barchar = "─";
    let mut bar = barchar.repeat(w as usize);

    let mut chat = Vec::new();
    let mut prompt = String::new();
    let mut stop = false;

    let mut buffer = [0; 64];

    while !stop {
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
                            stop = true;
                        } else {
                            prompt.push(code);
                        }
                    }
                    KeyCode::Esc => {
                        prompt.clear();
                    }
                    KeyCode::Enter => {
                        stream.write(prompt.as_bytes())?;
                        chat.push(prompt.clone());
                        prompt.clear();
                    }
                    _ => {}
                },

                _ => {}
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                stop = true;
            }

            Ok(n) => {
                chat.push(from_utf8(&buffer[0..n]).unwrap().to_string());
            }

            Err(e) => {
                // 非阻塞读在没有数据时会返回 WouldBlock，
                // 这只是“暂时没数据”，应该继续循环而不是退出程序。
                if e.kind() != ErrorKind::WouldBlock {
                    chat.push(e.to_string());
                }
            }
        }

        stdout.queue(Clear(ClearType::All))?;

        let _ = chat_window(
            &mut stdout,
            &chat,
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
