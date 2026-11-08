//! Object integrity checks for the RustFS backend, with no new dependency.
//!
//! What the backend promises (ADR-126 S1, "integrity"):
//! - **PUT**: the SigV4 payload hash (`x-amz-content-sha256`) is signed, so RustFS rejects a body
//!   that was changed in flight, and the put also sends `x-amz-checksum-sha256`, so the object is
//!   *stored with* its SHA-256 and can be checked on every later read.
//! - **GET**: the request asks for the checksum (`x-amz-checksum-mode: ENABLED`). The body must
//!   match `Content-Length` when the response has one, and then, in this order: the SHA-256 the
//!   store returned, else (for an object stored without a checksum) the single-part ETag, which
//!   is the MD5 of the content. An object with neither (a multipart ETag `<hex>-<n>` is not a
//!   hash of the content) is checked for length only; that is reported as [`Check::Unverified`],
//!   never as verified.
//! - **COPY**: a `200` whose body is an `<Error>` is an error; the body must be a
//!   `CopyObjectResult`.

use crate::sigv4::{hex, sha256_hex};

/// What a read was checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The store's SHA-256 matched.
    Sha256,
    /// The single-part ETag (MD5 of the content) matched.
    Md5Etag,
    /// Nothing to compare the content with beyond its length.
    Unverified,
}

/// Standard base64 with padding (RFC 4648).
pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

/// The value of `x-amz-checksum-sha256` for `data`: base64 of the raw digest.
pub fn checksum_sha256(data: &[u8]) -> String {
    let hexed = sha256_hex(data);
    let raw: Vec<u8> = (0..hexed.len()).step_by(2).filter_map(|i| u8::from_str_radix(&hexed[i..i + 2], 16).ok()).collect();
    base64(&raw)
}

/// MD5 (RFC 1321). Only for comparing a single-part ETag; never for anything secret or signed.
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21];
    let k: Vec<u32> = (0..64).map(|i| ((f64::from(i + 1)).sin().abs() * 4_294_967_296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = Vec::with_capacity(16);
    for v in [a0, b0, c0, d0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    hex(&out)
}

/// Check a GET body: against `content_length` (when the response had one), then against the
/// store's SHA-256 (`checksum`, base64) or the single-part `etag`. `Err` says what did not match.
pub fn verify_body(body: &[u8], content_length: Option<u64>, checksum: Option<&str>, etag: Option<&str>) -> Result<Check, String> {
    if let Some(n) = content_length
        && n != body.len() as u64
    {
        return Err(format!("integrity: Content-Length {n} but {} bytes were read", body.len()));
    }
    if let Some(want) = checksum {
        let got = checksum_sha256(body);
        return if got == want { Ok(Check::Sha256) } else { Err(format!("integrity: x-amz-checksum-sha256 {want} but the body hashes to {got}")) };
    }
    if let Some(e) = etag.map(|e| e.trim_matches('"').to_ascii_lowercase())
        && e.len() == 32
        && e.bytes().all(|c| c.is_ascii_hexdigit())
    {
        let got = md5_hex(body);
        return if got == e { Ok(Check::Md5Etag) } else { Err(format!("integrity: ETag {e} but the body's MD5 is {got}")) };
    }
    Ok(Check::Unverified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn md5_matches_the_rfc_1321_vectors_and_a_long_input() {
        for (i, o) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            ("12345678901234567890123456789012345678901234567890123456789012345678901234567890", "57edf4a22be3c955ac49da2e2107b67a"),
        ] {
            assert_eq!(md5_hex(i.as_bytes()), o, "{i:?}");
        }
        assert_eq!(md5_hex(b"hello"), "5d41402abc4b2a76b9719d911017c592", "the ETag RustFS 1.0.4 gives for these five bytes");
    }

    #[test]
    fn the_sha256_checksum_is_base64_of_the_digest() {
        // SHA-256("") = e3b0c442...b855 -> base64 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=
        assert_eq!(checksum_sha256(b""), "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
    }

    #[test]
    fn a_body_is_checked_by_length_then_checksum_then_etag_and_otherwise_reported_unverified() {
        let b = b"hello";
        let ck = checksum_sha256(b);
        assert_eq!(verify_body(b, Some(5), Some(&ck), Some("\"deadbeef\"")), Ok(Check::Sha256), "the checksum decides when there is one");
        assert_eq!(verify_body(b, Some(5), None, Some("\"5d41402abc4b2a76b9719d911017c592\"")), Ok(Check::Md5Etag));
        assert_eq!(verify_body(b, None, None, Some("5d41402abc4b2a76b9719d911017c592-3")), Ok(Check::Unverified), "a multipart ETag proves nothing about the content");
        assert_eq!(verify_body(b, None, None, None), Ok(Check::Unverified));
        assert!(verify_body(b"hellp", Some(5), Some(&ck), None).unwrap_err().contains("checksum"));
        assert!(verify_body(b"hellp", None, None, Some("5d41402abc4b2a76b9719d911017c592")).unwrap_err().contains("MD5"));
        assert!(verify_body(b"hell", Some(5), None, None).unwrap_err().contains("Content-Length"));
    }
}
