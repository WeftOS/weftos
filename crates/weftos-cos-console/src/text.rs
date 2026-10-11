//! Word wrapping and truncation for the built-in monospaced fonts.
//! Widths are counted in glyph cells, so one `char` is one cell.

/// Greedy word-wrap of `text` to at most `cols` cells per line.
///
/// - `\n` starts a new paragraph; an empty paragraph yields one blank
///   line, so blank lines in agent replies survive.
/// - Runs of whitespace collapse to one space.
/// - A word longer than `cols` is broken hard at the cell limit.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut cur: Vec<char> = Vec::new();
        let mut emitted = false;
        for word in para.split_whitespace() {
            let mut w: Vec<char> = word.chars().collect();
            while w.len() > cols {
                if !cur.is_empty() {
                    out.push(cur.drain(..).collect());
                }
                out.push(w.drain(..cols).collect());
                emitted = true;
            }
            if w.is_empty() {
                continue;
            }
            if cur.is_empty() {
                cur = w;
            } else if cur.len() + 1 + w.len() <= cols {
                cur.push(' ');
                cur.extend(w);
            } else {
                out.push(cur.drain(..).collect());
                emitted = true;
                cur = w;
            }
        }
        if !cur.is_empty() || !emitted {
            out.push(cur.into_iter().collect());
        }
    }
    out
}

/// Wrap with a prefix on the first line and another on continuation
/// lines (the chat log's `> ` for user lines). Both prefixes must be
/// the same width.
pub fn wrap_prefixed(text: &str, cols: usize, first: &str, rest: &str) -> Vec<String> {
    let pw = first.chars().count();
    debug_assert_eq!(pw, rest.chars().count(), "prefixes must share a width");
    let inner = cols.saturating_sub(pw).max(1);
    wrap(text, inner)
        .into_iter()
        .enumerate()
        .map(|(i, l)| format!("{}{}", if i == 0 { first } else { rest }, l))
        .collect()
}

/// Clip to `cols` cells, ending in `..` when something was dropped.
pub fn truncate(s: &str, cols: usize) -> String {
    let n = s.chars().count();
    if n <= cols {
        return s.to_string();
    }
    if cols <= 2 {
        return s.chars().take(cols).collect();
    }
    let mut t: String = s.chars().take(cols - 2).collect();
    t.push_str("..");
    t
}

/// Pad or clip to exactly `cols` cells (left-aligned).
pub fn fit(s: &str, cols: usize) -> String {
    let mut t = truncate(s, cols);
    let n = t.chars().count();
    t.extend(std::iter::repeat_n(' ', cols.saturating_sub(n)));
    t
}

/// Centre `s` in `cols` cells.
pub fn center(s: &str, cols: usize) -> String {
    let n = s.chars().count();
    if n >= cols {
        return truncate(s, cols);
    }
    let left = (cols - n) / 2;
    let mut t: String = std::iter::repeat_n(' ', left).collect();
    t.push_str(s);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrap("the quick brown fox", 9), vec!["the quick", "brown fox"]);
    }

    #[test]
    fn breaks_long_words_hard() {
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a abcdefghij b", 4), vec!["a", "abcd", "efgh", "ij b"]);
    }

    #[test]
    fn keeps_blank_paragraphs() {
        assert_eq!(wrap("one\n\ntwo", 10), vec!["one", "", "two"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn prefix_keeps_width() {
        let l = wrap_prefixed("hello world again", 9, "> ", "  ");
        assert_eq!(l, vec!["> hello", "  world", "  again"]);
        assert!(l.iter().all(|s| s.chars().count() <= 9));
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("abcdefgh", 5), "abc..");
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(center("ab", 6), "  ab");
    }
}
