//! Standard-alphabet base64 for the JSON carrier. One implementation
//! for the pad, the daemon and the emulator, so the three cannot drift.

use alloc::string::String;
use alloc::vec::Vec;

const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let (a, b, c) = (chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(c & 63) as usize] as char } else { '=' });
    }
    out
}

fn val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict decode: only alphabet characters, then at most two `=` pad
/// bytes at the end. Anything else is `None`, never a partial result.
pub fn decode(input: &[u8]) -> Option<Vec<u8>> {
    let end = input.iter().rposition(|&c| c != b'=').map_or(0, |i| i + 1);
    if input.len() - end > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(end / 4 * 3 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in &input[..end] {
        acc = (acc << 6) | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The base64 value of member `key` (`"key":"<b64>"`) in carrier JSON.
pub fn extract_member(text: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let at = text.windows(key.len()).position(|w| w == key)? + key.len();
    let mut i = at;
    while i < text.len() && !is_b64(text[i]) {
        // Stop at a closing brace so a missing value cannot run on into
        // a later member.
        if text[i] == b'}' {
            return None;
        }
        i += 1;
    }
    let start = i;
    while i < text.len() && (is_b64(text[i]) || text[i] == b'=') {
        i += 1;
    }
    if start == i {
        return None;
    }
    decode(&text[start..i])
}

fn is_b64(c: u8) -> bool {
    val(c).is_some()
}
