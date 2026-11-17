//! Daemon panic hook.
//!
//! The workspace release profile sets `panic = "abort"`, so a panic ends the
//! process. This hook runs before the abort and records what panicked through
//! `tracing`, so the failure reaches the log (and the chain event layer)
//! instead of only a raw stderr line. It then chains to the previous hook.

use std::backtrace::Backtrace;
use std::panic::PanicHookInfo;
use std::sync::Once;

/// Longest panic message kept in the log record.
const MAX_MESSAGE_CHARS: usize = 512;

/// Words that put the next token (or the text after `=` / `:`) in secret territory.
const SECRET_WORDS: [&str; 6] = ["token", "secret", "password", "passwd", "key", "bearer"];

/// Install the daemon panic hook. Safe to call more than once; only the first call installs.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log_panic(info);
            previous(info);
        }));
    });
}

fn log_panic(info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
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
    let backtrace = Backtrace::force_capture();
    tracing::error!(
        target: "weaver::panic",
        thread = thread.name().unwrap_or("<unnamed>"),
        location = %location,
        message = %redact(&message),
        backtrace = %backtrace,
        "daemon panic; process will abort under panic=abort"
    );
}

/// Cut a panic message to a safe length and mask anything that looks like a credential.
///
/// A panic message can carry a `Debug` dump of whatever failed, so the hook never logs it raw:
/// long opaque tokens and the value after a secret-looking word are replaced with `[redacted]`.
fn redact(message: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for word in message.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let is_secret_word = SECRET_WORDS.iter().any(|w| lower.contains(w));
        let (head, tail) = match word.split_once(['=', ':']) {
            Some((h, t)) => (h, Some(t)),
            None => (word, None),
        };
        if mask_next || opaque(word) {
            out.push("[redacted]".to_owned());
            mask_next = false;
        } else if is_secret_word && tail.is_some_and(|t| !t.is_empty()) {
            out.push(format!("{head}=[redacted]"));
        } else {
            out.push(word.to_owned());
            mask_next = is_secret_word;
        }
    }
    let joined = out.join(" ");
    match joined.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((i, _)) => format!("{}...", &joined[..i]),
        None => joined,
    }
}

/// A run of 32 or more key-like characters.
fn opaque(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    w.len() >= 32
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/_=-".contains(c))
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
    fn redact_truncates_long_messages_on_a_char_boundary() {
        let long = "é ".repeat(2000);
        let out = redact(&long);
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS + 3);
    }
}
