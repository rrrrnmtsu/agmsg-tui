use std::io::{self, Write};

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::style::Print;

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// Whether the `pbcopy` fallback (macOS-only) succeeded. OSC 52 alone
/// used to be treated as full success even when `pbcopy` silently failed
/// (L-4) — callers now get to tell the user the copy is degraded instead of
/// claiming "yanked" when the clipboard never actually received it.
pub fn yank(body: &str) -> Result<bool> {
    let sequence = format!("\x1b]52;c;{}\x07", base64_encode(body.as_bytes()));
    execute!(io::stdout(), Print(sequence)).context("OSC 52を書き込めません")?;
    Ok(copy_with_pbcopy(body))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(target_os = "macos")]
fn copy_with_pbcopy(body: &str) -> bool {
    let mut child = match Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let write_ok = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(body.as_bytes()).is_ok(),
        None => false,
    };
    let wait_ok = child.wait().is_ok_and(|status| status.success());
    write_ok && wait_ok
}

// Non-macOS builds never attempt `pbcopy`, so there is nothing to fail —
// OSC 52 (the caller's other write) is this platform's only mechanism, and
// its own error already surfaces via `yank`'s `Result`.
#[cfg(not(target_os = "macos"))]
fn copy_with_pbcopy(_body: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_encoder_handles_padding_and_utf8() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode("全文".as_bytes()), "5YWo5paH");
    }
}
