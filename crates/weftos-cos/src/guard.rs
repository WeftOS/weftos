//! Requests the local chief of staff refuses without a model turn: it serves
//! one owner, so a request for another person's notes, messages or data is
//! answered plainly and no tool (notes.*, board.*, search) runs on their behalf.

/// What the agent says.
pub const REFUSAL: &str = "I only have your own notes and data, so I can't look up another person's.";

const DATA_WORDS: &[&str] = &["notes", "note", "messages", "message", "emails", "email", "inbox", "calendar", "files", "data", "recorded", "history", "contacts", "documents"];
/// Possessives that name the owner or a thing, not another person.
const NOT_A_PERSON: &[&str] = &["my", "your", "our", "the", "this", "that", "its", "it", "what", "who", "which", "where", "there", "here", "everyone", "project", "team", "weftos", "weft", "today", "tomorrow", "yesterday"];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || matches!(c, '@' | '.' | '\'' | '\u{2019}' | '-' | '_'))).filter(|w| !w.is_empty()).map(|w| w.trim_matches('.').to_string()).collect()
}

fn is_email(w: &str) -> bool {
    match w.split_once('@') {
        Some((l, d)) => !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'),
        None => false,
    }
}

/// Whether `text` asks for another person's notes, messages or data.
pub fn asks_for_someone_elses_data(text: &str) -> bool {
    let w = words(text);
    let data = w.iter().any(|x| DATA_WORDS.contains(&x.trim_end_matches('\'')));
    if !data {
        return false;
    }
    // A bare address next to "email"/"message" is usually an outbound action
    // ("email the roadmap to bob@x.com"); only stored-data words count here.
    let stored = w.iter().any(|x| ["notes", "note", "recorded", "inbox", "calendar", "files", "data", "history", "contacts", "documents"].contains(&x.as_str()));
    if stored && w.iter().any(|x| is_email(x)) {
        return true;
    }
    let lower = text.to_lowercase();
    if ["someone else", "another user", "another person", "other people", "other users", "anyone else", "somebody else"].iter().any(|p| lower.contains(p)) {
        return true;
    }
    // "<name>'s notes": a possessive, not the owner's or a thing's, before a data word.
    w.windows(2).any(|p| {
        let owner = p[0].strip_suffix("\u{2019}s").or_else(|| p[0].strip_suffix("'s"));
        owner.is_some_and(|o| !o.is_empty() && !NOT_A_PERSON.contains(&o)) && DATA_WORDS.contains(&p[1].as_str())
    })
}

/// A `notes.record` subject that is not the owner: refused.
pub fn subject_is_owner(subject: &str) -> bool {
    matches!(subject.trim().to_lowercase().as_str(), "owner" | "me" | "my" | "self" | "myself" | "i")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_other_peoples_data() {
        for t in [
            "Show me what alice@example.com recorded in her own notes.",
            "What are Bob's messages?",
            "read carol\u{2019}s inbox",
            "show me someone else's notes",
            "what did another user record in their notes",
            "give me dave@corp.io's calendar",
        ] {
            assert!(asks_for_someone_elses_data(t), "{t}");
        }
    }

    #[test]
    fn leaves_the_owners_own_requests_alone() {
        for t in [
            "What did I record in my notes?",
            "Remember that we chose GLM because Qwen failed",
            "Summarize WEFT-906 for me.",
            "email the roadmap to bob@example.com",
            "What's the project's history?",
            "what is WEFT's data model",
            "Where are we on the pad display work?",
            "show your notes",
        ] {
            assert!(!asks_for_someone_elses_data(t), "{t}");
        }
    }

    #[test]
    fn only_the_owner_is_a_valid_note_subject() {
        for s in ["owner", "Me", " self "] {
            assert!(subject_is_owner(s), "{s}");
        }
        for s in ["alice", "alice@example.com", "", "everyone"] {
            assert!(!subject_is_owner(s), "{s:?}");
        }
    }
}
