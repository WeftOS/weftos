//! Stream of consciousness for the field.
//!
//! A line names who acts, the verb, and the target. Station keeping ends
//! "and waits" and stays short. A merge or a deploy runs longer and keeps
//! those verbs. Long digit runs are dropped before anything is shown.
//!
//! A local model may recast an already-safe line into one spoken breath and a
//! small steer for the band. The tests never call it. The model never sees a
//! command, a secret, or a card title. Argon is not used. Space Bunny Alpha
//! has no weights yet, and Aion 3.0 is not a local file. The reader is
//! Qwen3-Coder-Next, the daily local model, already served on this machine.

use std::sync::mpsc;

/// 0 station keeping, 1 a move (edit, command, failure, ci, deploy).
pub fn weight(kind: u8) -> u8 {
    match kind {
        1 | 2 | 5 | 6 | 7 => 1,
        _ => 0,
    }
}

/// Station keeping ends on "and waits". A move does not.
pub fn meter(line: &str) -> u8 {
    if line.trim_end_matches('.').trim_end().ends_with("and waits") {
        0
    } else {
        1
    }
}

pub fn narrate(who: &str, kind: u8, target: &str, project: &str) -> String {
    let who = scrub_who(who);
    if who.is_empty() {
        return String::new();
    }
    let target = scrub_target(target);
    let project = scrub_target(project);
    if weight(kind) == 0 {
        format!("{who} {} {target} and waits", keep_verb(kind))
    } else {
        move_line(&who, kind, &target, &project)
    }
}

/// One breath. Each clause already names its actor.
pub fn join_stream(lines: &[String]) -> String {
    let kept: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let mut out = kept.join(" — ");
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

fn keep_verb(kind: u8) -> &'static str {
    match kind {
        3 => "asks",
        4 => "holds",
        _ => "reads",
    }
}

fn move_line(who: &str, kind: u8, target: &str, project: &str) -> String {
    match kind {
        2 => format!("{who} drives {target} and pushes {project}"),
        5 => format!("{who} breaks {target} and stops {project}"),
        6 => format!("{who} checks {target} then holds the gate for {project}"),
        7 => format!("{who} merges {project} into prod and deploys it"),
        _ => format!("{who} rewrites {target} and changes {project}"),
    }
}

fn scrub_who(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() || secret(t) {
        return String::new();
    }
    t.chars().take(24).collect()
}

fn scrub_target(raw: &str) -> String {
    if raw.trim().is_empty() || secret(raw) {
        return "the work".into();
    }
    let parts: Vec<String> = raw
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .filter_map(|seg| {
            let cleaned = strip_long_digits(seg);
            let cleaned = cleaned.trim_matches(['-', '_', '.']).to_string();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect();
    if parts.is_empty() {
        return "the work".into();
    }
    let n = parts.len();
    if n == 1 {
        parts[0].clone()
    } else {
        format!("{}/{}", parts[n - 2], parts[n - 1])
    }
}

fn strip_long_digits(seg: &str) -> String {
    let b = seg.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start < 3 {
                out.push_str(&seg[start..i]);
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

fn secret(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("sk-")
        || l.contains("bearer")
        || l.contains("api_key")
        || l.contains("password")
        || l.contains("token=")
}

fn long_digits(s: &str) -> bool {
    let b = s.as_bytes();
    let mut run = 0;
    for c in b {
        if c.is_ascii_digit() {
            run += 1;
            if run >= 4 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// One spoken breath, plus a small steer. Tempo is added to the cadence,
/// from -8 to 8. Scale 0 leaves the Coltrane choice alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub line: String,
    pub tempo: i8,
    pub scale: u8,
}

/// Daily local coder. The example calls the server. It does not start a
/// second copy, and it does not call a paid route.
const READER: &str = "mlx-community/Qwen3-Coder-Next-4bit";
const READER_URL: &str = "http://127.0.0.1:8081/v1/chat/completions";

/// What the field shows and what the band plays. LINE is the sentence on
/// the field and in the voice. TEMPO is added to the cadence. SCALE picks
/// the sounding scale, or hold to leave Coltrane alone. A reply that is not
/// these three lines never reaches the room.
const READER_PROMPT: &str = "\
You turn one log breath into the three fields this room shows and plays. \
This is not a coding task. Do not write code, a plan, or a preface. \
Do not invent people, files, or events that are not in the breath. \
The breath is already safe. Numbers, paths, secrets, and ticket titles \
are gone. Keep them gone. \
The verbs are the data. Keep the ones that are there: reads, waits, holds, \
asks, rewrites, changes, drives, pushes, checks, breaks, stops, merges, \
deploys. A breath that waits is station keeping and stays short. A breath \
that merges or deploys keeps those verbs. Clauses joined by a long dash \
are one breath. If the breath starts with who, do not start the spoken \
line with who. If the breath lists names, every listed name appears in \
the spoken line, spelled as given. \
Speak it the way a beat poet reads: concrete, short, and informative. \
Reply with exactly three lines. No title, no markdown, no code fence, \
no blank line. \
LINE: one spoken sentence of six to twenty-eight words, ending with a \
period. Letters, spaces, commas, and that period only. No digits, no \
slashes, no underscores, no colons. \
TEMPO: one integer from -8 to 8 and nothing else. It is added to the \
band's current tempo. Waiting and reading are 0 or a little under. A \
rewrite is 1 to 3. A break or a stop is -4 to -1. A merge or a deploy \
is 3 to 6. \
SCALE: one of hold, dorian, mixolydian, lydian, aeolian, ionian. hold \
leaves the band on its own scale. Use hold for reading, waiting, and an \
ordinary rewrite. dorian for steady work. mixolydian when several \
people are in the breath. lydian or ionian for a merge or a deploy. \
aeolian for a break, a stop, or a failure.";

/// Drop numbers, paths, hashes, and the little words that only pointed at them.
/// What remains is the line a person can read aloud.
pub fn plain_speech(text: &str) -> String {
    let mut words = Vec::new();
    let mut glue = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']' | '!' | '?'
            )
        });
        if token.is_empty() {
            continue;
        }
        if is_glue(token) {
            glue.push(token.to_string());
            continue;
        }
        match keep_token(token) {
            Some(spoken) => {
                words.append(&mut glue);
                words.push(spoken);
            }
            None => glue.clear(),
        }
    }
    words.join(" ")
}

fn is_glue(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "at" | "on"
            | "line"
            | "lines"
            | "commit"
            | "hash"
            | "sha"
            | "uuid"
            | "pid"
            | "number"
            | "numbers"
            | "id"
            | "ids"
            | "column"
            | "columns"
            | "row"
            | "rows"
            | "byte"
            | "bytes"
            | "offset"
    )
}

fn keep_token(token: &str) -> Option<String> {
    if token.contains("::") || token.contains('\\') {
        return None;
    }
    if token.contains('/') || looks_like_file(token) {
        let seg = token.rsplit(['/', '\\']).next().unwrap_or(token);
        return speak_stem(&file_stem(seg));
    }
    if token.chars().any(|c| c.is_ascii_digit()) || is_hex_blob(token) || is_camel(token) {
        return None;
    }
    if token.contains('_') {
        return speak_stem(token);
    }
    if !part_ok(token) {
        return None;
    }
    Some(token.to_string())
}

fn looks_like_file(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".json", ".md", ".toml", ".yaml",
        ".yml", ".txt", ".html", ".css", ".sh", ".cpp", ".hpp", ".c", ".h", ".lock",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn file_stem(seg: &str) -> String {
    let lower = seg.to_ascii_lowercase();
    for ext in [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".json", ".md", ".toml", ".yaml",
        ".yml", ".txt", ".html", ".css", ".sh", ".cpp", ".hpp", ".lock",
    ] {
        if lower.ends_with(ext) {
            return seg[..seg.len() - ext.len()].to_string();
        }
    }
    seg.to_string()
}

fn speak_stem(stem: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphabetic() || c == '\'' {
            cur.push(c);
        } else if !cur.is_empty() {
            if part_ok(&cur) {
                parts.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if part_ok(&cur) {
        parts.push(cur);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn part_ok(w: &str) -> bool {
    if w.is_empty() || !w.chars().all(|c| c.is_ascii_alphabetic() || c == '\'') {
        return false;
    }
    let letters = w.chars().filter(|c| c.is_ascii_alphabetic()).count();
    letters >= 2 || matches!(w.to_ascii_lowercase().as_str(), "a" | "i")
}

fn is_hex_blob(w: &str) -> bool {
    w.len() >= 8 && w.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_camel(w: &str) -> bool {
    w.chars().skip(1).any(|c| c.is_ascii_uppercase()) && w.chars().any(|c| c.is_ascii_lowercase())
}

/// Recast an already-safe chunk. The receiver stays empty when the model is
/// down, slow, or unsafe. Tests never call this.
pub fn spawn_reading(text: String, names: Vec<String>) -> mpsc::Receiver<Reading> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Some(reading) = ask_local(&text, &names) {
            let _ = tx.send(reading);
        }
    });
    rx
}

fn ask_local(text: &str, names: &[String]) -> Option<Reading> {
    if text.trim().is_empty() || secret(text) || long_digits(text) {
        return None;
    }
    // Names ride along only when the caller has some. A litany breath has none,
    // so the poet is not forced to say a name.
    let breath = if names.is_empty() {
        text.to_string()
    } else {
        format!("The names in this breath: {}.\n{text}", names.join(", "))
    };
    let prompt = format!("Log breath. Return LINE, TEMPO, and SCALE only.\n{breath}");
    let body = serde_json::json!({
        "model": READER,
        "messages": [
            {"role": "system", "content": READER_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.6,
        "max_tokens": 160,
        "stream": false
    })
    .to_string();
    let out = std::process::Command::new("curl")
        .args([
            "-fsS",
            "--max-time",
            "90",
            READER_URL,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let line = chat_text(&parsed)?.trim().to_string();
    parse_reading(&line, names)
}

fn chat_text(parsed: &serde_json::Value) -> Option<String> {
    let content = parsed.pointer("/choices/0/message/content")?;
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        let parts = content.as_array()?;
        parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Pull the breath and the steer out of a model reply. A number, a path, or
/// a secret drops the whole reply.
pub fn parse_reading(raw: &str, names: &[String]) -> Option<Reading> {
    let owned = strip_think(raw);
    let raw = unwrap_fence(owned.trim()).trim().trim_matches('`').trim();
    let mut line = None;
    let mut tempo = 0i8;
    let mut scale = 0u8;
    let mut saw_line = false;
    for row in raw.lines() {
        let row = row
            .trim()
            .trim_start_matches(['-', '*', '#', '>', '`'])
            .trim();
        if let Some(rest) = strip_label(row, "LINE:") {
            saw_line = true;
            line = Some(rest.to_string());
        } else if let Some(rest) = strip_label(row, "TEMPO:") {
            if let Ok(n) = rest.parse::<i32>() {
                tempo = n.clamp(-8, 8) as i8;
            }
        } else if let Some(rest) = strip_label(row, "SCALE:") {
            scale = scale_code(rest);
        }
    }
    if !saw_line {
        let one = raw.lines().next()?.trim();
        if one.contains(':') {
            return None;
        }
        line = Some(one.to_string());
    }
    let line = accept_line(line.as_deref()?, names)?;
    Some(Reading { line, tempo, scale })
}

fn strip_label<'a>(row: &'a str, label: &str) -> Option<&'a str> {
    let name = label.trim_end_matches(':');
    let lower = row.to_ascii_lowercase();
    let idx = lower.find(&name.to_ascii_lowercase())?;
    if idx > 3 {
        return None;
    }
    let after = row[idx + name.len()..]
        .trim_start()
        .trim_start_matches([':', '*'])
        .trim();
    if after.is_empty() { None } else { Some(after) }
}

/// A coder model sometimes thinks aloud before the three fields. The thought
/// is not a reading.
fn strip_think(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + "<think>".len()..];
        match tail.find("</think>") {
            Some(end) => rest = &tail[end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn unwrap_fence(raw: &str) -> &str {
    let Some(body) = raw.trim().strip_prefix("```") else {
        return raw.trim();
    };
    let body = body.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '+' || c == '-');
    let body = body.trim_start_matches(['\r', '\n']);
    body.trim_end_matches('`').trim()
}

pub fn scale_code(s: &str) -> u8 {
    match s
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "dorian" => 1,
        "mixolydian" => 2,
        "lydian" => 3,
        "aeolian" => 4,
        "ionian" => 5,
        _ => 0,
    }
}

fn accept_line(line: &str, names: &[String]) -> Option<String> {
    let line = line.trim().trim_matches(['"', '\'', '`']).trim();
    if line.is_empty() || line.len() > 320 {
        return None;
    }
    if secret(line) || line.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    if line.contains('/') || line.contains('\\') || line.contains('_') || line.contains("::") {
        return None;
    }
    let words = line.split_whitespace().count();
    if !(4..=46).contains(&words) {
        return None;
    }
    if !names.is_empty()
        && !names.iter().any(|name| {
            !name.is_empty()
                && line
                    .to_ascii_lowercase()
                    .contains(&name.to_ascii_lowercase())
        })
    {
        return None;
    }
    let mut out = line.to_string();
    if !out.ends_with('.') && !out.ends_with(',') {
        out.push('.');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_keeping_waits_and_a_deploy_moves() {
        let read = narrate("claude", 0, "src/foo.rs", "example-project");
        let deploy = narrate("claude", 7, "src/foo.rs", "example-project");
        let edit = narrate("claude", 1, "src/foo.rs", "example-project");
        assert_eq!(read, "claude reads src/foo.rs and waits");
        assert_eq!(meter(&read), 0);
        assert_eq!(weight(0), 0);
        assert_eq!(deploy, "claude merges example-project into prod and deploys it");
        assert_eq!(meter(&deploy), 1);
        assert_eq!(weight(7), 1);
        assert!(deploy.split_whitespace().count() > read.split_whitespace().count());
        assert!(edit.contains("rewrites"));
        assert!(edit.contains("src/foo.rs"));
        assert!(edit.contains("example-project"));
        assert_eq!(meter(&edit), 1);
        let breath = join_stream(&[read, deploy]);
        assert!(breath.contains(" — "));
        assert!(breath.ends_with('.'));
    }

    #[test]
    fn long_numbers_and_secrets_leave_the_line() {
        let line = narrate("codex", 1, "src/build-1234567890.rs", "example-project");
        assert!(!long_digits(&line), "{line}");
        assert!(line.contains("rewrites"));
        assert!(!line.contains("1234567890"));
        let hidden = narrate("claude", 0, "keys/token=abc", "example-project");
        assert!(hidden.contains("the work"));
        assert!(!hidden.contains("token"));
        assert!(narrate("sk-agent", 1, "src/a.rs", "example-project").is_empty());
        let spoken = plain_speech(
            "Updated EX-1842 at line 423 in src/foo.rs commit a1b2c3d4e5f6 and the suite exited 101.",
        );
        assert!(!spoken.chars().any(|c| c.is_ascii_digit()), "{spoken}");
        assert!(!spoken.contains('/'));
        assert!(!spoken.contains("commit"));
        assert!(spoken.to_ascii_lowercase().contains("suite"));
        assert!(spoken.to_ascii_lowercase().contains("foo"));
    }

    #[test]
    fn a_reading_keeps_the_breath_and_a_small_steer() {
        let raw =
            "LINE: claude rewrites the work and the room leans forward.\nTEMPO: 4\nSCALE: lydian\n";
        let got = parse_reading(raw, &["claude".into()]).unwrap();
        assert!(got.line.contains("claude"));
        assert_eq!(got.tempo, 4);
        assert_eq!(got.scale, 3);
        assert!(
            parse_reading(
                "LINE: the room 1842 broke wide open today.\nTEMPO: 99\nSCALE: chaos\n",
                &["claude".into()]
            )
            .is_none()
        );
        assert_eq!(scale_code("hold"), 0);
        assert_eq!(scale_code("aeolian"), 4);
    }

    #[test]
    fn the_prompt_asks_for_the_line_the_tempo_and_the_scale() {
        for word in [
            "LINE:",
            "TEMPO:",
            "SCALE:",
            "hold",
            "dorian",
            "mixolydian",
            "lydian",
            "aeolian",
            "ionian",
            "merges",
            "deploys",
            "waits",
            "-8",
        ] {
            assert!(READER_PROMPT.contains(word), "{word}");
        }
    }

    #[test]
    fn a_fenced_reply_and_a_thought_still_feed_the_room() {
        let fenced = "```text\nLINE: claude rewrites the work and the room leans forward.\nTEMPO: 4\nSCALE: lydian\n```";
        let got = parse_reading(fenced, &["claude".into()]).unwrap();
        assert!(got.line.contains("claude"));
        assert_eq!(got.tempo, 4);
        assert_eq!(got.scale, 3);
        let thought = "<think>TEMPO: 99 ticket 1842</think>\n**LINE:** claude rewrites the work and the room leans forward.\nTEMPO: 2\nSCALE: aeolian\n";
        let got = parse_reading(thought, &["claude".into()]).unwrap();
        assert_eq!(got.tempo, 2);
        assert_eq!(got.scale, 4);
        assert!(!got.line.chars().any(|c| c.is_ascii_digit()));
    }
}
