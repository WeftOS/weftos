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

/// The last second `iso_date` can write: 9999-12-31T23:59:59Z. A later one needs a fifth digit of year.
pub const MAX_ISO_SECS: u64 = 253_402_300_799;

/// `YYYY-MM-DDTHH:MM:SS.000Z` (ISO 8601, what the Object Lock retain-until header takes).
/// Callers check against [`MAX_ISO_SECS`] first; a later time is clamped to it here.
pub fn iso_date(unix_s: u64) -> String {
    let unix_s = unix_s.min(MAX_ISO_SECS);
    let a = amz_date(unix_s);
    format!("{}-{}-{}T{}:{}:{}.000Z", &a[..4], &a[4..6], &a[6..8], &a[9..11], &a[11..13], &a[13..15])
}

/// Seconds since the epoch of an ISO 8601 UTC time (`YYYY-MM-DDTHH:MM:SS`, optional fraction,
/// `Z` or `+00:00`). Anything else (another offset, a malformed date) is `None`.
pub fn parse_iso(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00")).unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let time = time.split('.').next()?;
    let num = |t: &str| t.parse::<u64>().ok();
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let mut t = time.split(':');
    let (hh, mm, ss) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    if d.next().is_some() || t.next().is_some() || !(1..=12).contains(&m) || day == 0 || hh > 23 || mm > 59 || ss > 60 || !(1970..=9999).contains(&y) {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let in_month = match m {
        2 => 28 + u64::from(leap),
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > in_month {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
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

/// The signed headers (`;`-joined names) and the canonical request for `r` signed at `amz_date`.
pub fn canonical_request(r: &Req, amz_date: &str) -> (String, String) {
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
    (signed, canon)
}

/// The `authorization` header value for a request signed at `amz_date`.
pub fn authorization(r: &Req, access: &str, secret: &str, region: &str, amz_date: &str) -> String {
    let date = &amz_date[..8];
    let (signed, canon) = canonical_request(r, amz_date);
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
    fn iso_dates_parse_back_to_the_instant_they_were_made_from() {
        for t in [0u64, 86_399, 951_782_400, 1_700_000_000, 1_793_000_123, 4_102_444_800] {
            assert_eq!(parse_iso(&iso_date(t)), Some(t), "{t}");
        }
        assert_eq!(parse_iso("2026-11-08T09:49:44+00:00"), parse_iso("2026-11-08T09:49:44.000Z"));
        assert_eq!(parse_iso("9999-12-31T23:59:59Z"), Some(MAX_ISO_SECS));
        assert_eq!(iso_date(MAX_ISO_SECS + 1), iso_date(MAX_ISO_SECS), "clamped, never a five-digit year");
        assert!(parse_iso("2024-02-29T00:00:00Z").is_some() && parse_iso("2000-02-29T00:00:00Z").is_some());
        for bad in ["", "2026-11-08", "2026-13-08T00:00:00Z", "2026-11-08T25:00:00Z", "1969-12-31T23:59:59Z", "2026-11-08T09:49:44+02:00", "x",
            "2026-02-31T00:00:00Z", "2025-02-29T00:00:00Z", "1900-02-29T00:00:00Z", "2026-04-31T00:00:00Z", "10000-01-01T00:00:00Z", "100000000000000000-01-01T00:00:00Z"] {
            assert_eq!(parse_iso(bad), None, "{bad}");
        }
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

    // The four worked examples of "Signature Calculations for the Authorization Header" in the
    // Amazon S3 API reference (AWS's published canonical requests and signatures; the credentials
    // are AWS's documentation example keys, not real ones).
    const DOC_ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const DOC_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const DOC_DATE: &str = "20130524T000000Z";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn doc_check(r: &Req, canon: &str, signed: &str, sig: &str) {
        let (s, c) = canonical_request(r, DOC_DATE);
        assert_eq!(c, canon, "canonical request");
        assert_eq!(s, signed);
        let auth = authorization(r, DOC_ACCESS, DOC_SECRET, "us-east-1", DOC_DATE);
        assert_eq!(auth, format!("AWS4-HMAC-SHA256 Credential={DOC_ACCESS}/20130524/us-east-1/s3/aws4_request, SignedHeaders={signed}, Signature={sig}"));
    }

    #[test]
    fn aws_documented_get_object_with_a_range_header() {
        let extra = [("range".to_owned(), "bytes=0-9".to_owned())];
        let r = Req { method: "GET", path: "/test.txt", query: &[], host: "examplebucket.s3.amazonaws.com", payload_sha256: EMPTY, extra: &extra };
        let canon = format!("GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\nx-amz-content-sha256:{EMPTY}\nx-amz-date:{DOC_DATE}\n\nhost;range;x-amz-content-sha256;x-amz-date\n{EMPTY}");
        doc_check(&r, &canon, "host;range;x-amz-content-sha256;x-amz-date", "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    #[test]
    fn aws_documented_put_object_with_a_dollar_in_the_key_and_extra_headers() {
        let body = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(body, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let extra = [("date".to_owned(), "Fri, 24 May 2013 00:00:00 GMT".to_owned()), ("x-amz-storage-class".to_owned(), "REDUCED_REDUNDANCY".to_owned())];
        let r = Req { method: "PUT", path: "/test$file.text", query: &[], host: "examplebucket.s3.amazonaws.com", payload_sha256: &body, extra: &extra };
        let canon = format!("PUT\n/test%24file.text\n\ndate:Fri, 24 May 2013 00:00:00 GMT\nhost:examplebucket.s3.amazonaws.com\nx-amz-content-sha256:{body}\nx-amz-date:{DOC_DATE}\nx-amz-storage-class:REDUCED_REDUNDANCY\n\ndate;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class\n{body}");
        doc_check(&r, &canon, "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class", "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    #[test]
    fn aws_documented_get_bucket_lifecycle_with_an_empty_query_value() {
        let q = [("lifecycle".to_owned(), String::new())];
        let r = Req { method: "GET", path: "/", query: &q, host: "examplebucket.s3.amazonaws.com", payload_sha256: EMPTY, extra: &[] };
        let canon = format!("GET\n/\nlifecycle=\nhost:examplebucket.s3.amazonaws.com\nx-amz-content-sha256:{EMPTY}\nx-amz-date:{DOC_DATE}\n\nhost;x-amz-content-sha256;x-amz-date\n{EMPTY}");
        doc_check(&r, &canon, "host;x-amz-content-sha256;x-amz-date", "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
    }

    #[test]
    fn aws_documented_list_objects_with_two_sorted_query_parameters() {
        // Given out of order: the canonical query sorts them.
        let q = [("prefix".to_owned(), "J".to_owned()), ("max-keys".to_owned(), "2".to_owned())];
        let r = Req { method: "GET", path: "/", query: &q, host: "examplebucket.s3.amazonaws.com", payload_sha256: EMPTY, extra: &[] };
        let canon = format!("GET\n/\nmax-keys=2&prefix=J\nhost:examplebucket.s3.amazonaws.com\nx-amz-content-sha256:{EMPTY}\nx-amz-date:{DOC_DATE}\n\nhost;x-amz-content-sha256;x-amz-date\n{EMPTY}");
        doc_check(&r, &canon, "host;x-amz-content-sha256;x-amz-date", "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }
}
