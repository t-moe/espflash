//! Serial monitor utility
//!
//! While simple, this serial monitor does provide some nice features such as:
//!
//! - Keyboard shortcut for resetting the device (Ctrl-R)
//! - Decoding of function addresses in serial output
//!
//! While some serial monitors buffer output until a newline is encountered,
//! that is not the case here. With other monitors the output of a `print!()`
//! call are not displayed until `println!()` is subsequently called, where as
//! in our monitor the output is displayed immediately upon reading.

use std::{
    io::{stdout, ErrorKind, Write},
    time::Duration,
};

use crossterm::{
    event::{poll, read, Event, KeyCode, KeyEvent, KeyModifiers},
    style::{Color, Print, PrintStyledContent, Stylize},
    terminal::{disable_raw_mode, enable_raw_mode},
    QueueableCommand,
};
use lazy_static::lazy_static;
use log::error;
use miette::{IntoDiagnostic, Result};
use regex::Regex;

use self::{line_endings::normalized, symbols::Symbols};
use crate::{connection::reset_after_flash, interface::Interface};

mod line_endings;
mod symbols;

// Pattern to much a function address in serial output.
lazy_static! {
    static ref RE_FN_ADDR: Regex = Regex::new(r"0x[[:xdigit:]]{8}").unwrap();
}

/// Context for the serial monitor
#[derive(Default)]
struct SerialContext<'ctx> {
    symbols: Option<Symbols<'ctx>>,
    previous_frag: Option<String>,
    previous_line: Option<String>,
    incomplete_utf8_buffer: Vec<u8>,
}

impl<'ctx> SerialContext<'ctx> {
    fn new(symbols: Option<Symbols<'ctx>>) -> Self {
        Self {
            symbols,
            ..Self::default()
        }
    }

    fn process_utf8(&mut self, buff: &[u8]) -> String {
        let mut buffer = std::mem::take(&mut self.incomplete_utf8_buffer);
        buffer.extend(normalized(buff.iter().copied()));

        // look for longest slice that we can then lossily convert without introducing errors for
        // partial sequences (#457)
        let mut len = 0;

        loop {
            match std::str::from_utf8(&buffer[len..]) {
                // whole input is valid
                Ok(str) if len == 0 => return String::from(str),

                // input is valid after the last error, and we could ignore the last error, so
                // let's process the whole input
                Ok(_) => return String::from_utf8_lossy(&buffer).to_string(),

                // input has some errors. We can ignore invalid sequences and replace them later,
                // but we have to stop if we encounter an incomplete sequence.
                Err(e) => {
                    len += e.valid_up_to();
                    if let Some(error_len) = e.error_len() {
                        len += error_len;
                    } else {
                        // incomplete sequence. We split it off, save it for later
                        let (bytes, incomplete) = buffer.split_at(len);
                        self.incomplete_utf8_buffer = incomplete.to_vec();
                        return String::from_utf8_lossy(bytes).to_string();
                    }
                }
            }
        }
    }
}

/// Type that ensures that raw mode is disabled when dropped.
struct RawModeGuard;

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        enable_raw_mode().into_diagnostic()?;
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = disable_raw_mode() {
            error!("Failed to disable raw_mode: {:#}", e)
        }
    }
}

/// Open a serial monitor on the given interface
pub fn monitor(
    mut serial: Interface,
    elf: Option<&[u8]>,
    pid: u16,
    baud: u32,
) -> serialport::Result<()> {
    println!("Commands:");
    println!("    CTRL+R    Reset chip");
    println!("    CTRL+C    Exit");
    println!();

    // Explicitly set the baud rate when starting the serial monitor, to allow using
    // different rates for flashing.
    serial.serial_port_mut().set_baud_rate(baud)?;
    serial
        .serial_port_mut()
        .set_timeout(Duration::from_millis(5))?;

    // Load symbols from the ELF file (if provided) and initialize the context.
    let symbols = if let Some(bytes) = elf {
        Symbols::try_from(bytes).ok()
    } else {
        None
    };
    let mut ctx = SerialContext::new(symbols);

    // We are in raw mode until `_raw_mode` is dropped (ie. this function returns).
    let _raw_mode = RawModeGuard::new();

    let stdout = stdout();
    let mut stdout = stdout.lock();

    let mut buff = [0; 1024];
    loop {
        let read_count = match serial.serial_port_mut().read(&mut buff) {
            Ok(count) => Ok(count),
            Err(e) if e.kind() == ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            err => err,
        }?;

        if read_count > 0 {
            handle_serial(&mut ctx, &buff[0..read_count], &mut stdout);
        }

        if poll(Duration::from_secs(0))? {
            if let Event::Key(key) = read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c') => break,
                        KeyCode::Char('r') => {
                            reset_after_flash(&mut serial, pid)?;
                            continue;
                        }
                        _ => {}
                    }
                }

                if let Some(bytes) = handle_key_event(key) {
                    serial.serial_port_mut().write_all(&bytes)?;
                    serial.serial_port_mut().flush()?;
                }
            }
        }
    }

    Ok(())
}

/// Handles and writes the received serial data to the given output stream.
fn handle_serial(ctx: &mut SerialContext, buff: &[u8], out: &mut dyn Write) {
    let text = ctx.process_utf8(buff);

    // Split the text into lines, storing the last of which separately if it is
    // incomplete (ie. does not end with '\n') because these need special handling.
    let mut lines = text.lines().collect::<Vec<_>>();
    let incomplete = if text.ends_with('\n') {
        None
    } else {
        lines.pop()
    };

    // Iterate through all *complete* lines (ie. those ending with '\n') ...
    for line in lines {
        // ... and print the line.
        out.queue(Print(line)).ok();

        // If there is a previous line fragment, that means that the current line must
        // be appended to it in order to form the complete line. Since we want to look
        // for function addresses in the *entire* previous line we combine these prior
        // to performing the symbol lookup(s).
        ctx.previous_line = if let Some(frag) = &ctx.previous_frag {
            Some(format!("{frag}{line}"))
        } else {
            Some(line.to_string())
        };

        // The previous fragment has been completed (by this current line).
        ctx.previous_frag = None;

        // If we have loaded some symbols...
        if let Some(symbols) = &ctx.symbols {
            // And there was previously a line printed to the terminal...
            if let Some(line) = &ctx.previous_line {
                // Check the previous line for function addresses. For each address found,
                // attempt to look up the associated function's name and location and write both
                // to the terminal.
                for matched in RE_FN_ADDR.find_iter(line).map(|m| m.as_str()) {
                    // Since our regular expression already confirms that this is a correctly
                    // formatted hex literal, we can (fairly) safely assume that it will parse
                    // successfully into an integer.
                    let addr = parse_int::parse::<u64>(matched).unwrap();

                    let name = symbols.get_name(addr).unwrap_or_else(|| "??".into());
                    let (file, line_num) =
                        if let Some((file, line_num)) = symbols.get_location(addr) {
                            (file, line_num.to_string())
                        } else {
                            ("??".into(), "??".into())
                        };

                    out.queue(PrintStyledContent(
                        format!("\r\n{matched} - {name}\r\n    at {file}:{line_num}")
                            .with(Color::Yellow),
                    ))
                    .unwrap();
                }
            }
        }

        // Remember to begin a new line after we have printed this one!
        out.write_all(b"\r\n").ok();
    }

    // If there is an incomplete line we will still print it. However, we will not
    // perform function name lookups or terminate it with a newline.
    if let Some(line) = incomplete {
        out.queue(Print(line)).ok();

        if let Some(frag) = &ctx.previous_frag {
            ctx.previous_frag = Some(format!("{frag}{line}"));
        } else {
            ctx.previous_frag = Some(line.to_string());
        }
    }

    // Don't forget to flush the writer!
    out.flush().ok();
}

// Converts key events from crossterm into appropriate character/escape
// sequences which are then sent over the serial connection.
//
// Adapted from: https://github.com/dhylands/serial-monitor
fn handle_key_event(key_event: KeyEvent) -> Option<Vec<u8>> {
    // The following escape sequences come from the MicroPython codebase.
    //
    //  Up      ESC [A
    //  Down    ESC [B
    //  Right   ESC [C
    //  Left    ESC [D
    //  Home    ESC [H  or ESC [1~
    //  End     ESC [F  or ESC [4~
    //  Del     ESC [3~
    //  Insert  ESC [2~

    let mut buf = [0; 4];

    let key_str: Option<&[u8]> = match key_event.code {
        KeyCode::Backspace => Some(b"\x08"),
        KeyCode::Enter => Some(b"\r"),
        KeyCode::Left => Some(b"\x1b[D"),
        KeyCode::Right => Some(b"\x1b[C"),
        KeyCode::Home => Some(b"\x1b[H"),
        KeyCode::End => Some(b"\x1b[F"),
        KeyCode::Up => Some(b"\x1b[A"),
        KeyCode::Down => Some(b"\x1b[B"),
        KeyCode::Tab => Some(b"\x09"),
        KeyCode::Delete => Some(b"\x1b[3~"),
        KeyCode::Insert => Some(b"\x1b[2~"),
        KeyCode::Esc => Some(b"\x1b"),
        KeyCode::Char(ch) => {
            if key_event.modifiers & KeyModifiers::CONTROL == KeyModifiers::CONTROL {
                buf[0] = ch as u8;

                if ch.is_ascii_lowercase() || (ch == ' ') {
                    buf[0] &= 0x1f;
                    Some(&buf[0..1])
                } else if ('4'..='7').contains(&ch) {
                    // crossterm returns Control-4 thru 7 for \x1c thru \x1f
                    buf[0] = (buf[0] + 8) & 0x1f;
                    Some(&buf[0..1])
                } else {
                    Some(ch.encode_utf8(&mut buf).as_bytes())
                }
            } else {
                Some(ch.encode_utf8(&mut buf).as_bytes())
            }
        }
        _ => None,
    };

    key_str.map(|slice| slice.into())
}

#[cfg(test)]
mod test {
    #[test]
    fn returns_valid_strings_immediately() {
        let mut ctx = super::SerialContext::default();
        let buff = b"Hello, world!";
        let text = ctx.process_utf8(buff);
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn does_not_repeat_valid_strings() {
        let mut ctx = super::SerialContext::default();
        let text = ctx.process_utf8(b"Hello, world!");
        assert_eq!(text, "Hello, world!");
        let text = ctx.process_utf8(b"Something else");
        assert_eq!(text, "Something else");
    }

    #[test]
    fn replaces_invalid_sequence() {
        let mut ctx = super::SerialContext::default();
        let text = ctx.process_utf8(b"Hello, \xFF world!");
        assert_eq!(text, "Hello, \u{FFFD} world!");
    }

    #[test]
    fn can_replace_unfinished_incomplete_sequence() {
        let mut ctx = super::SerialContext::default();
        let mut incomplete = Vec::from("Hello, ".as_bytes());
        let utf8 = "🙈".as_bytes();
        incomplete.extend_from_slice(&utf8[..utf8.len() - 1]);
        let text = ctx.process_utf8(&incomplete);
        assert_eq!(text, "Hello, ");

        let text = ctx.process_utf8(b" world!");
        assert_eq!(text, "\u{FFFD} world!");
    }

    #[test]
    fn can_merge_incomplete_sequence() {
        let mut ctx = super::SerialContext::default();
        let mut incomplete = Vec::from("Hello, ".as_bytes());
        let utf8 = "🙈".as_bytes();
        incomplete.extend_from_slice(&utf8[..utf8.len() - 1]);

        let text = ctx.process_utf8(&incomplete);
        assert_eq!(text, "Hello, ");

        let text = ctx.process_utf8(&utf8[utf8.len() - 1..]);
        assert_eq!(text, "🙈");
    }

    #[test]
    fn issue_457() {
        let mut ctx = super::SerialContext::default();
        let mut result = String::new();

        result.push_str(&ctx.process_utf8(&[0x48]));
        result.push_str(&ctx.process_utf8(&[0x65, 0x6C, 0x6C]));
        result.push_str(&ctx.process_utf8(&[
            0x6F, 0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, 0x21, 0x20, 0x77, 0x69, 0x74,
        ]));
        result.push_str(&ctx.process_utf8(&[
            0x68, 0x20, 0x55, 0x54, 0x46, 0x3A, 0x20, 0x77, 0x79, 0x73, 0x79,
        ]));
        result.push_str(&ctx.process_utf8(&[0xC5, 0x82, 0x61, 0x6D, 0x0A]));

        assert_eq!(result, "Hello world! with UTF: wysyłam\r\n");
    }
}
