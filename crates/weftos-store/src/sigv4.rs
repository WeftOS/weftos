//! AWS Signature V4 for the RustFS S3 and admin endpoints (service `s3`, path-style).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Lowercase hex of SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    // HMAC accepts any key length, so `new_from_slice` cannot fail.
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// Percent-encode as SigV4 wants: only `A-Za-z0-9-_.~` stay; `/` stays when `keep_slash`.
pub fn encode(s: &str, keep_slash: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') || (keep_slash && c == b'/') {
            o.push(c as char);
        } else {
            o.push_str(&format!("%{c:02X}"));
        }
    }
    o
}

/// The signing key for a date (`YYYYMMDD`), region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// `YYYYMMDDTHHMMSSZ` for a unix time in seconds.
pub fn amz_date(unix_s: u64) -> String {
    let (days, rem) = ((unix_s / 86_400) as i64, unix_s % 86_400);
    // Civil date from days since 1970-01-01 (Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` (ISO 8601, what the Object Lock retain-until header takes).
pub fn iso_date(unix_s: u64) -> String {
    let a = amz_date(unix_s);
    format!("{}-{}-{}T{}:{}:{}.000Z", &a[..4], &a[4..6], &a[6..8], &a[9..11], &a[11..13], &a[13..15])
}

/// What to sign.
pub struct Req<'a> {
    pub method: &'a str,
    /// Unencoded path, starting with `/`.
    pub path: &'a str,
    /// Unencoded query pairs.
    pub query: &'a [(String, String)],
    /// `host` or `host:port`, as the HTTP client will send it.
    pub host: &'a str,
    pub payload_sha256: &'a str,
    /// Further headers the client will send (lowercase names). All are signed.
    pub extra: &'a [(String, String)],
}

/// Canonical query string: encoded, sorted pairs.
pub fn canonical_query(q: &[(String, String)]) -> String {
    let mut v: Vec<(String, String)> = q.iter().map(|(k, x)| (encode(k, false), encode(x, false))).collect();
    v.sort();
    v.iter().map(|(k, x)| format!("{k}={x}")).collect::<Vec<_>>().join("&")
}

/// The `authorization` header value for a request signed at `amz_date`.
pub fn authorization(r: &Req, access: &str, secret: &str, region: &str, amz_date: &str) -> String {
    let date = &amz_date[..8];
    let mut hs: Vec<(String, String)> = vec![
        ("host".into(), r.host.to_owned()),
        ("x-amz-content-sha256".into(), r.payload_sha256.to_owned()),
        ("x-amz-date".into(), amz_date.to_owned()),
    ];
    hs.extend(r.extra.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned())));
    hs.sort();
    let signed = hs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let heads: String = hs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canon = format!("{}\n{}\n{}\n{heads}\n{signed}\n{}", r.method, encode(r.path, true), canonical_query(r.query), r.payload_sha256);
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canon.as_bytes()));
    let sig = hex(&hmac(&signing_key(secret, date, region, "s3"), to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_matches_the_aws_documented_value() {
        let k = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex(&k), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    #[test]
    fn date_formats_known_instants() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_699_358_400), "20231107T120000Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_date(1_699_358_400), "2023-11-07T12:00:00.000Z");
        assert_eq!(iso_date(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn encoding_and_canonical_query() {
        assert_eq!(encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(encode("a/b", false), "a%2Fb");
        let q = [("prefix".to_owned(), "a/b c".to_owned()), ("list-type".to_owned(), "2".to_owned())];
        assert_eq!(canonical_query(&q), "list-type=2&prefix=a%2Fb%20c");
    }

    #[test]
    fn empty_payload_hash_is_the_well_known_constant() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
