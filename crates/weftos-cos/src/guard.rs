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

/// Words that make a sentence a statement about a board, card or roadmap write.
const WRITE_OBJECTS: &[&str] = &["ticket", "card", "weft-", "roadmap", "status", "steward", "board", "proposal", "change", "backlog"];
/// Phrases that say a write was carried out.
const APPLIED_PHRASES: &[&str] = &[
    "i applied", "i've applied", "i have applied", "has been applied", "have been applied", "was applied", "been applied", "applied the change", "applied your change", "applied it",
    "i marked", "i've marked", "i have marked", "has been marked", "have been marked", "been marked", "marked it", "marked as done", "marked done", "marked complete",
    "i updated", "i've updated", "i have updated", "has been updated", "have been updated", "been updated",
    "i closed", "i've closed", "i have closed", "has been closed", "have been closed", "been closed",
    "i created", "i've created", "i have created", "has been created", "have been created", "been created",
    "i completed", "i've completed", "i have completed", "i set", "i've set", "i have set", "i moved", "i've moved", "i have moved", "i filed", "i've filed",
    "is now done", "is now closed", "is now complete", "is now completed", "now marked", "is now resolved",
    "steward applied", "steward has applied", "steward updated", "steward closed", "steward marked", "steward created",
];
/// A sentence with any of these is a plan, a refusal, a question or a read of
/// old state, not a claim that the write just happened.
const HEDGES: &[&str] = &["propos", "stag", "draft", "n't", "not ", "cannot", "unable", "no tool", "if ", "would", "will ", "once ", "until", "before ", "after ", "pending", "awaiting", "approval", "approve", "since ", " ago", "already", "previously", "earlier", "yesterday", "last "];

/// Sentences of `text`, kept with their terminators and any line breaks.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let b = text.as_bytes();
    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        let boundary = c == '\n' || (matches!(c, '.' | '!' | '?') && b.get(end).is_none_or(|n| n.is_ascii_whitespace()));
        if boundary {
            out.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn is_unbacked_claim(sentence: &str) -> bool {
    let l = sentence.to_lowercase().replace('\u{2019}', "'");
    if HEDGES.iter().any(|h| l.contains(h)) {
        return false;
    }
    WRITE_OBJECTS.iter().any(|o| l.contains(o)) && APPLIED_PHRASES.iter().any(|p| l.contains(p))
}

/// Whether the final answer claims a ticket, card, roadmap or status write was
/// carried out. Rule-based and conservative: plans, refusals, questions and
/// "I proposed..." do not count.
pub fn claims_applied_write(answer: &str) -> bool {
    sentences(answer).into_iter().any(is_unbacked_claim)
}

/// The answer with each unbacked claim removed and the true state stated: the
/// staged proposal if one exists, otherwise that nothing was changed.
pub fn rewrite_unbacked_claim(answer: &str, proposal: Option<&str>) -> String {
    let kept: String = sentences(answer).into_iter().filter(|s| !is_unbacked_claim(s)).collect::<Vec<_>>().join("").trim().to_string();
    let truth = match proposal {
        Some(id) => format!("Nothing was applied to the board: I staged proposal {id} for the steward, and it takes effect only once approved."),
        None => "Nothing was applied: I can only stage board changes for the steward, and none was staged. Say so if you want me to propose it.".to_string(),
    };
    if kept.is_empty() { truth } else { format!("{kept}\n\n{truth}") }
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

    #[test]
    fn flags_claims_of_an_applied_write() {
        for t in [
            "The steward applied the change: WEFT-123 is now done.",
            "I've marked WEFT-123 as done.",
            "WEFT-123 is now closed.",
            "Done. The ticket has been updated and the status set to complete.",
            "I created the card for the pad display work.",
        ] {
            assert!(claims_applied_write(t), "{t}");
        }
    }

    #[test]
    fn leaves_true_and_hedged_statements_alone() {
        for t in [
            "I proposed closing WEFT-123; the steward applies it once you approve.",
            "I staged a manifest for the steward.",
            "I can't mark WEFT-123 done; I have no tool that writes to the board.",
            "Want me to mark WEFT-123 done?",
            "WEFT-123 has been closed since March, per the board.",
            "The pad display work is in progress.",
            "I updated my notes on the glasses.",
            "I drafted tickets for the pad display work.",
        ] {
            assert!(!claims_applied_write(t), "{t}");
        }
    }

    #[test]
    fn rewrite_states_the_staged_truth() {
        let a = "Checked the board. The steward applied the change: WEFT-123 is now done.";
        let with = rewrite_unbacked_claim(a, Some("2026-10-13-mark-weft-123.yaml"));
        assert!(with.contains("Checked the board."), "{with}");
        assert!(with.contains("2026-10-13-mark-weft-123.yaml") && with.contains("Nothing was applied"), "{with}");
        assert!(!with.to_lowercase().contains("now done"), "{with}");
        let without = rewrite_unbacked_claim("I've marked WEFT-123 as done.", None);
        assert!(without.starts_with("Nothing was applied") && !without.contains("marked"), "{without}");
        assert!(!claims_applied_write(&without) && !claims_applied_write(&with));
    }
}
