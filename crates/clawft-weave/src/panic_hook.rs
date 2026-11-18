//! Daemon panic hook.
//!
//! The workspace release profile sets `panic = "abort"`, so a panic ends the
//! process. This hook runs before the abort and records what panicked through
//! `tracing`, so the failure reaches the log (and the chain event layer).
//!
//! The std default hook is deliberately not chained: it prints the raw panic message to stderr,
//! which is the journald sink, and would defeat the redaction below. Instead the hook writes one
//! short redacted line to stderr first (so a panic is never silent, even when `tracing` is not
//! initialised) and then the full `tracing` event. Panics before `daemon::run` installs the hook
//! (argument parsing, tracing init, config load) get only the std default hook.

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::io::Write as _;
use std::panic::PanicHookInfo;
use std::sync::Once;

/// Longest panic message kept in the log record.
const MAX_MESSAGE_CHARS: usize = 512;

/// Key names whose value (the text after `=` / `:`, or the next word) is masked. Matched as whole
/// segments of the key (split on non-alphanumerics), so prose such as "key not found" and names
/// such as `monkey` are left alone.
const SECRET_WORDS: [&str; 8] = [
    "token",
    "secret",
    "password",
    "passwd",
    "key",
    "apikey",
    "bearer",
    "authorization",
];

thread_local! {
    /// Set while this thread is inside the hook; a second entry (a panic while logging a panic)
    /// skips `tracing` and writes the minimal stderr line only.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Install the daemon panic hook. Safe to call more than once; only the first call installs.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Production (release or dev daemon): never chain the default hook, see the module docs.
        // Under the test harness (`cfg(test)`), chain the previous hook so libtest still prints
        // and captures each test's panic message. Several lib tests run `daemon::run` in-process,
        // which installs this hook for the rest of the test binary.
        let prev = if CHAIN_PREVIOUS {
            Some(std::panic::take_hook())
        } else {
            None
        };
        std::panic::set_hook(Box::new(move |info| {
            log_panic(info);
            if let Some(prev) = &prev {
                prev(info);
            }
        }));
    });
}

/// Whether the hook chains the previously installed hook. True only under `cfg(test)`.
const CHAIN_PREVIOUS: bool = cfg!(test);

fn log_panic(info: &PanicHookInfo<'_>) {
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_owned());
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    let message = redact(&message);
    // Fallback first: it needs no machinery beyond stderr.
    // Not `eprintln!`: that panics on EPIPE/ENOSPC, which would skip the tracing line below.
    // Skipped under the test harness: the chained previous hook reports through libtest's capture.
    if !CHAIN_PREVIOUS {
        let _ = writeln!(
            std::io::stderr().lock(),
            "daemon panic at {location}: {message}"
        );
    }
    if IN_HOOK.with(|g| g.replace(true)) {
        return; // re-entered: do not touch tracing again
    }
    let thread = std::thread::current();
    let backtrace = Backtrace::force_capture();
    tracing::error!(
        target: "weaver::panic",
        thread = thread.name().unwrap_or("<unnamed>"),
        location = %location,
        message = %message,
        backtrace = %backtrace,
        "daemon panic; process will abort under panic=abort"
    );
    IN_HOOK.with(|g| g.set(false));
}

/// Cut a panic message to a safe length and mask anything that looks like a credential.
///
/// A panic message can carry a `Debug` dump of whatever failed, so the hook never logs it raw.
/// Masked: URL userinfo (`scheme://user:pass@host`), the value after a secret-named key plus `=`
/// or `:` (whatever its length), the credential after `Bearer`/`Basic`, long opaque tokens and
/// JWT-shaped dotted tokens. This is a heuristic: a short credential with no key name in front of
/// it (an AWS access key id, say) is not caught.
fn redact(message: &str) -> String {
    let message = redact_userinfo(message);
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for word in message.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let scheme = matches!(
            lower.trim_matches(|c: char| !c.is_ascii_alphabetic()),
            "bearer" | "basic"
        );
        let (head, tail) = match word.split_once(['=', ':']) {
            Some((h, t)) => (h, Some(t)),
            None => (word, None),
        };
        let secret_key = is_secret_key(head);
        if mask_next || opaque(word) {
            out.push("[redacted]".to_owned());
            // `Authorization: Bearer <x>`: the scheme word is masked and so is the value after it.
            mask_next = mask_next && scheme;
        } else if secret_key && tail.is_some_and(|t| !t.is_empty()) {
            out.push(format!("{head}=[redacted]"));
            // `Authorization=Bearer <x>`: the credential is the next word.
            mask_next = tail.is_some_and(|t| {
                let t = t.to_ascii_lowercase();
                t.trim_matches(|c: char| !c.is_ascii_alphabetic()) == "bearer"
                    || t.trim_matches(|c: char| !c.is_ascii_alphabetic()) == "basic"
            });
        } else {
            out.push(word.to_owned());
            // A bare `Bearer`/`Basic` or a `key:` / `token=` with the value in the next word.
            mask_next = scheme || (secret_key && tail == Some(""));
        }
    }
    let joined = out.join(" ");
    match joined.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((i, _)) => format!("{}...", &joined[..i]),
        None => joined,
    }
}

/// `scheme://user:pass@host` becomes `scheme://[redacted]@host`.
fn redact_userinfo(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(i) = rest.find("://") {
        let (before, after) = rest.split_at(i + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c == '/' || c == '@' || c.is_whitespace())
            .unwrap_or(after.len());
        if after[end..].starts_with('@') {
            out.push_str("[redacted]");
            rest = &after[end..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// True when a key name (the part before `=` / `:`) contains a secret word as a whole segment.
fn is_secret_key(head: &str) -> bool {
    if head.contains('/') {
        return false; // a path such as `src/key.rs:10`, not a key name
    }
    head.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|seg| SECRET_WORDS.contains(&seg))
}

/// A run of 32 or more key-like characters, or a dotted token (three or more base64url
/// segments, such as a JWT) of 24 or more characters. A `/`-separated path whose every segment
/// reads as a plain name is not a token.
fn opaque(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let key_like = |c: char| c.is_ascii_alphanumeric() || "_=-".contains(c);
    if w.len() >= 32 && w.chars().all(|c| key_like(c) || "+/.".contains(c)) && !readable_path(w) {
        return true;
    }
    let segments: Vec<&str> = w.split('.').collect();
    segments.len() >= 3
        && w.len() >= 24
        && segments
            .iter()
            .all(|s| !s.is_empty() && s.chars().all(key_like))
}

/// Two or more `/` separators and every segment a short plain name (no inner capitals, at most
/// four digits). Random base64 with a few slashes fails this.
fn readable_path(w: &str) -> bool {
    w.matches('/').count() >= 2
        && w.split('/').filter(|s| !s.is_empty()).all(|s| {
            s.len() <= 24
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
                && !s.chars().skip(1).any(|c| c.is_ascii_uppercase())
                && s.chars().filter(char::is_ascii_digit).count() <= 4
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn panic_in_named_thread(capture: &Capture, message: String) {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let joined = std::thread::Builder::new()
            .name("panic-hook-test".to_owned())
            .spawn(move || {
                // The hook runs on the panicking thread, so the thread-local default applies.
                tracing::subscriber::with_default(subscriber, || panic!("{message}"));
            })
            .expect("spawn test thread")
            .join();
        assert!(joined.is_err(), "the thread must have panicked");
    }

    #[test]
    fn a_panic_is_logged_with_message_location_thread_and_backtrace() {
        install();
        install(); // idempotent: the second call must not stack another hook
        let capture = Capture::default();
        panic_in_named_thread(&capture, "boom in the test".to_owned());
        let log = String::from_utf8(capture.0.lock().expect("capture lock poisoned").clone())
            .expect("log is utf-8");
        assert!(log.contains("daemon panic"), "{log}");
        assert!(log.contains("boom in the test"), "{log}");
        assert!(log.contains("panic-hook-test"), "{log}");
        assert!(log.contains("panic_hook.rs"), "{log}");
        assert!(log.contains("backtrace"), "{log}");
        assert_eq!(
            log.matches("daemon panic").count(),
            1,
            "hook installed twice: {log}"
        );
    }

    #[test]
    fn secrets_in_a_panic_message_are_not_logged() {
        install();
        let capture = Capture::default();
        let key = "A".repeat(40);
        panic_in_named_thread(
            &capture,
            format!("bad auth: token=hunter2 password: swordfish bearer {key} for node"),
        );
        let log = String::from_utf8(capture.0.lock().expect("capture lock poisoned").clone())
            .expect("log is utf-8");
        assert!(log.contains("[redacted]"), "{log}");
        assert!(!log.contains("hunter2"), "{log}");
        assert!(!log.contains("swordfish"), "{log}");
        assert!(!log.contains(&key), "{log}");
    }

    #[test]
    fn a_jwt_without_a_keyword_is_redacted() {
        let jwt = format!("eyJhbGciOiJIUzI1NiJ9.{}.{}", "e".repeat(40), "s".repeat(43));
        let out = redact(&format!("rejected {jwt} for node"));
        assert!(!out.contains("eyJhbGci"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
        assert!(
            out.contains("rejected") && out.contains("for node"),
            "{out}"
        );
        // A short three-segment dotted token is also masked; a plain filename is not.
        assert!(!redact("eyJhbGciOiJIUzI1NiJ9.e30.abcdefghij").contains("eyJ"));
        assert_eq!(redact("see src/main.rs now"), "see src/main.rs now");
        // Two long segments still fall under the 32-char rule.
        assert!(!redact(&format!("x {}.{}", "a".repeat(20), "b".repeat(20))).contains("aaaa"));
    }

    #[test]
    fn the_test_build_chains_the_previous_hook_so_libtest_reports_panics() {
        // Production must not chain (the reviewed behaviour); the test build must.
        const { assert!(CHAIN_PREVIOUS, "cfg(test) must chain the previous hook") };
        install();
        // A panic on another thread after install still goes through the chained default hook,
        // which writes to libtest's captured output rather than raw stderr; the thread must
        // simply unwind normally.
        assert!(std::thread::spawn(|| panic!("chained")).join().is_err());
    }

    #[test]
    fn a_reentered_hook_skips_tracing_and_does_not_hang() {
        install();
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let joined = std::thread::spawn(move || {
            // Pretend this thread is already inside the hook.
            IN_HOOK.with(|g| g.set(true));
            tracing::subscriber::with_default(subscriber, || panic!("nested boom"));
        })
        .join();
        assert!(joined.is_err());
        let log = String::from_utf8(capture.0.lock().expect("capture lock poisoned").clone())
            .expect("log is utf-8");
        assert!(
            !log.contains("daemon panic"),
            "tracing was used on re-entry: {log}"
        );
    }

    #[test]
    fn redact_truncates_long_messages_on_a_char_boundary() {
        let long = "é ".repeat(2000);
        let out = redact(&long);
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS + 3);
    }

    #[test]
    fn url_userinfo_is_masked() {
        let out = redact("connect postgres://admin:s3cret@db.internal/app failed");
        assert!(!out.contains("s3cret") && !out.contains("admin"), "{out}");
        assert!(
            out.contains("postgres://[redacted]@db.internal/app"),
            "{out}"
        );
        assert_eq!(
            redact("GET https://host/path ok"),
            "GET https://host/path ok"
        );
    }

    #[test]
    fn bearer_and_authorization_credentials_are_masked_at_any_length() {
        for msg in [
            "Authorization=Bearer abc ok",
            "Authorization: Bearer abc ok",
            "sent Bearer abc ok",
            "Authorization: Basic dXNlcg== ok",
        ] {
            let out = redact(msg);
            assert!(
                !out.contains("abc") && !out.contains("dXNlcg"),
                "{msg} -> {out}"
            );
            assert!(out.ends_with("ok"), "{msg} -> {out}");
        }
    }

    #[test]
    fn short_values_after_a_secret_key_are_masked() {
        for msg in [
            "api_key=abc123",
            "\"api_key\": \"abc123\"",
            "token: x1",
            "PASSWORD=pw",
        ] {
            let out = redact(msg);
            assert!(
                !out.contains("abc123") && !out.contains("x1") && !out.contains("=pw"),
                "{msg} -> {out}"
            );
            assert!(out.contains("[redacted]"), "{msg} -> {out}");
        }
    }

    #[test]
    fn secret_words_in_prose_do_not_mask_the_next_word() {
        assert_eq!(redact("key not found in map"), "key not found in map");
        assert_eq!(redact("monkey token expired"), "monkey token expired");
        assert_eq!(
            redact("at src/key.rs:10:5 failed"),
            "at src/key.rs:10:5 failed"
        );
    }

    #[test]
    fn readable_long_paths_survive_but_opaque_runs_do_not() {
        let path = "/var/lib/weftos/projects/abc/store.db";
        assert!(path.len() >= 32);
        assert_eq!(
            redact(&format!("open {path} failed")),
            format!("open {path} failed")
        );
        let opaque = "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD1fH/3jL5nP7r";
        assert!(redact(&format!("x {opaque}")).contains("[redacted]"));
        assert!(!redact(&format!("x {}", "A".repeat(40))).contains("AAAA"));
    }
}
