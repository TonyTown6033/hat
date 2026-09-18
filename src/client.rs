use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, cursor::MoveTo};
use std::io::{self, Write, stdout};
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
            .write_all(bytes.get(0..boundary.w).unwrap_or(bytes))?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let _ = terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    let (mut w, mut h) = terminal::size()?;
    let barchar = "─";
    let mut bar = barchar.repeat(w as usize);

    let mut chat = Vec::new();
    let mut prompt = String::new();
    let mut stop = false;

    while !stop {
        while poll(Duration::ZERO).unwrap() {
            match read()? {
                Event::Resize(width, height) => {
                    w = width;
                    h = height;
                    bar = barchar.repeat(w as usize);
                }
                Event::Key(event) => match event.code {
                    KeyCode::Char(code) => {
                        if event.modifiers.contains(KeyModifiers::CONTROL) && code == 'c' {
                            stop = true;
                        } else {
                            prompt.push(code);
                        }
                    }
                    KeyCode::Enter => {
                        chat.push(prompt.clone());
                        prompt.clear();
                    }
                    _ => {}
                },

                _ => {}
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
        stdout.write_all(bar.as_bytes())?;
        stdout.queue(MoveTo(0, h - 1))?;
        stdout.write_all(prompt.as_bytes())?;
        stdout.flush()?;
        sleep(Duration::from_millis(33));
    }
    let _ = terminal::disable_raw_mode()?;
    Ok(())
}
