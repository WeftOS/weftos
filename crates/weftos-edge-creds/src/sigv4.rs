//! AWS Signature V4 for S3: header signing, query presigning, and the
//! recompute used by the verifier in [`crate::mock`]. Pure functions over
//! `hmac` and `sha2`; no I/O, no clock.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const SERVICE: &str = "s3";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Constant-time-ish equality for signatures.
pub fn sig_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// RFC 3986 percent-encoding as S3 wants it. `keep_slash` for paths.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` for unix seconds (proleptic Gregorian).
pub fn amz_dates(unix: u64) -> (String, String) {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let date = format!("{y:04}{m:02}{d:02}");
    let stamp = format!("{date}T{:02}{:02}{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60);
    (date, stamp)
}

/// Inverse of [`amz_dates`]' stamp: `YYYYMMDDTHHMMSSZ` to unix seconds.
pub fn parse_stamp(s: &str) -> Option<u64> {
    if s.len() != 16 || s.as_bytes()[8] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    let (hh, mm, ss) = (n(9..11)?, n(11..13)?, n(13..15)?);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

pub fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, SERVICE.as_bytes());
    hmac(&k, b"aws4_request")
}

pub fn scope(date: &str, region: &str) -> String {
    format!("{date}/{region}/{SERVICE}/aws4_request")
}

/// Everything the canonical request covers.
pub struct Canonical<'a> {
    pub method: &'a str,
    /// Absolute path, already the raw (unencoded) key path, e.g. `/n-abc/a b`.
    pub path: &'a str,
    /// Query pairs, raw (unencoded); sorted and encoded here.
    pub query: &'a [(String, String)],
    /// Headers to sign; names are lowercased and sorted here.
    pub headers: &'a [(String, String)],
    pub payload_hash: &'a str,
}

/// Returns `(canonical_request, signed_headers)`.
pub fn canonical_request(c: &Canonical<'_>) -> (String, String) {
    let mut q: Vec<(String, String)> =
        c.query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut h: Vec<(String, String)> = c
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    h.sort();
    let canon_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let req = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        c.method,
        uri_encode(c.path, true),
        query,
        canon_headers,
        signed,
        c.payload_hash
    );
    (req, signed)
}

pub fn sign(secret: &str, stamp: &str, region: &str, canonical: &str) -> String {
    let date = &stamp[..8];
    let sts = format!("{ALGORITHM}\n{stamp}\n{}\n{}", scope(date, region), sha256_hex(canonical.as_bytes()));
    hex(&hmac(&signing_key(secret, date, region), sts.as_bytes()))
}

/// The inputs to sign a request with header auth.
pub struct HeaderSigner<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
    pub region: &'a str,
    pub now: u64,
}

impl HeaderSigner<'_> {
    /// Returns the headers to add (`x-amz-date`, `x-amz-content-sha256`,
    /// `x-amz-security-token`, `authorization`). `host` and any extra headers
    /// in `base_headers` are signed too.
    pub fn sign(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        base_headers: &[(String, String)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        let (date, stamp) = amz_dates(self.now);
        let payload = sha256_hex(body);
        let mut headers: Vec<(String, String)> = base_headers.to_vec();
        headers.push(("x-amz-date".into(), stamp.clone()));
        headers.push(("x-amz-content-sha256".into(), payload.clone()));
        if let Some(t) = self.session_token {
            headers.push(("x-amz-security-token".into(), t.into()));
        }
        let (canon, signed) = canonical_request(&Canonical { method, path, query, headers: &headers, payload_hash: &payload });
        let sig = sign(self.secret_key, &stamp, self.region, &canon);
        headers.push((
            "authorization".into(),
            format!(
                "{ALGORITHM} Credential={}/{}, SignedHeaders={signed}, Signature={sig}",
                self.access_key,
                scope(&date, self.region)
            ),
        ));
        headers
    }
}

/// A query-presigned URL's parts.
pub struct Presigned {
    pub path: String,
    /// Final query pairs including `X-Amz-Signature`.
    pub query: Vec<(String, String)>,
}

/// Presign `method path` for `expires` seconds. Only `host` is signed, so the
/// URL is bound to method, bucket, key, credential and expiry.
pub fn presign(
    c: &HeaderSigner<'_>,
    host: &str,
    method: &str,
    path: &str,
    expires: u64,
) -> Presigned {
    let (date, stamp) = amz_dates(c.now);
    let mut query = vec![
        ("X-Amz-Algorithm".to_string(), ALGORITHM.to_string()),
        ("X-Amz-Credential".into(), format!("{}/{}", c.access_key, scope(&date, c.region))),
        ("X-Amz-Date".into(), stamp.clone()),
        ("X-Amz-Expires".into(), expires.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    if let Some(t) = c.session_token {
        query.push(("X-Amz-Security-Token".into(), t.into()));
    }
    let headers = [("host".to_string(), host.to_string())];
    let (canon, _) = canonical_request(&Canonical { method, path, query: &query, headers: &headers, payload_hash: UNSIGNED_PAYLOAD });
    query.push(("X-Amz-Signature".into(), sign(c.secret_key, &stamp, c.region, &canon)));
    Presigned { path: path.to_string(), query }
}

/// Render a presigned URL.
pub fn render_url(scheme: &str, host: &str, p: &Presigned) -> String {
    let q = p.query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect::<Vec<_>>().join("&");
    format!("{scheme}://{host}{}?{q}", uri_encode(&p.path, true))
}
