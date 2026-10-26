//! Literal phrase replacement for transcripts.
//!
//! This is the opposite of `keywords`: keywords *correct* what was heard into
//! the spelling the user wants, so they match fuzzily. Snippets *replace* what
//! was heard with something that was never said, so they must match exactly —
//! a fuzzy matcher here would rewrite ordinary sentences.
//!
//! Matching is case-insensitive, whole words only, and happens in a single
//! pass over the original text, longest trigger first. A single pass matters:
//! it means a replacement is never itself expanded by another snippet, and a
//! trigger that appears inside its own replacement cannot loop.
//!
//! One entry can hold several spoken alternatives separated by commas, so
//! `um, uh, er` all collapse to a single replacement.

use serde::{Deserialize, Serialize};

pub const MAX_SNIPPETS: usize = 200;
pub const MAX_TRIGGER_CHARS: usize = 80;
pub const MAX_REPLACEMENT_CHARS: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// What the user says, verbatim.
    pub trigger: String,
    /// What should appear in the transcript instead.
    pub replacement: String,
}

/// Lowercases ASCII only, so every character keeps its byte length and the
/// folded copy can be sliced at the same offsets as the original text.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii() { c.to_ascii_lowercase() } else { c })
        .collect()
}

fn at_start(haystack: &str, index: usize) -> bool {
    index == 0 || !haystack[..index]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
}

fn at_end(haystack: &str, index: usize) -> bool {
    !haystack[index..]
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric())
}

/// One stretch of the output, in the order it will be pasted.
///
/// Keeping the replacements apart from the transcript is what makes the cleanup
/// safe: spacing and punctuation only change where two pieces meet, so a
/// snippet cannot rewrite text it never matched. The previous version tidied
/// the whole transcript whenever any snippet with an empty replacement existed,
/// which turned "Wait... what" into "Wait. what".
#[derive(Clone, Copy)]
enum Piece<'a> {
    /// Verbatim transcript, never touched.
    Kept(&'a str),
    /// A replacement, kept exactly as the user typed it.
    Inserted(&'a str),
    /// Words a snippet deleted: the pieces either side have to be joined.
    Removed { started_upper: bool },
}

fn is_mark(c: char) -> bool {
    matches!(c, ',' | ';' | ':')
}

fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
}

/// Marks that never take a space in front of them.
fn closes(c: char) -> bool {
    matches!(
        c,
        ',' | ';' | ':' | '.' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '”' | '’'
    )
}

/// Marks that never take a space after them.
fn opens(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '"' | '\'' | '‘' | '“' | '-' | '/')
}

fn first_char(text: &str) -> char {
    text.chars().next().unwrap_or(' ')
}

/// Drops the leading mark and the spaces that followed it.
fn strip_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next();
    chars.as_str().trim_start_matches([' ', '\t']).to_string()
}

/// Gives a sentence its capital back. Only plain lowercase ASCII changes, so an
/// email address, a camelCase word or a path is left exactly as it was.
fn capitalise(text: &str, on: bool) -> String {
    if !on {
        return text.to_string();
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {
            let mut out = String::with_capacity(text.len());
            out.push(c.to_ascii_uppercase());
            out.push_str(chars.as_str());
            out
        }
        _ => text.to_string(),
    }
}

/// Joins the two sides of an edit, which is the only place text is cleaned up.
///
/// `capitalize` means the words that were removed began a sentence, so the word
/// that now starts it needs the capital the model never wrote — it was following
/// a filler word, not a full stop.
fn join(left: &str, right: &str, capitalize: bool) -> String {
    let mut head = left.trim_end_matches([' ', '\t']).to_string();
    let tail = right.trim_start_matches([' ', '\t']);

    if head.is_empty() {
        // The transcript now starts where a deleted phrase did, and the phrase
        // was attached to its mark: "Um, I think" must not read ", I think".
        let mut tail = tail.to_string();
        let mark = first_char(&tail);
        if is_mark(mark) || is_sentence_end(mark) {
            tail = tail
                .trim_start_matches(mark)
                .trim_start_matches([' ', '\t'])
                .to_string();
        }
        return capitalise(&tail, capitalize);
    }

    if tail.is_empty() {
        return head;
    }

    let last = head.chars().next_back().unwrap_or(' ');
    let starts_sentence = is_sentence_end(last) || last == '\n';

    // A comma with nothing left to separate after a full stop: deleting "Um,"
    // from "Fine. Um, that's it." would otherwise read "Fine., that's it."
    if starts_sentence && is_mark(first_char(tail)) {
        return join(&head, &strip_first(tail), capitalize);
    }
    // "I think, ." — the mark on this side has nothing left to separate either.
    if is_mark(last) && is_sentence_end(first_char(tail)) {
        head.pop();
        return join(&head, tail, capitalize);
    }
    // The removed words sat between two of the same mark; one will do.
    if last == first_char(tail) && is_mark(first_char(tail)) {
        return join(&head, &strip_first(tail), capitalize);
    }

    let tail = capitalise(tail, capitalize && starts_sentence);
    let last = head.chars().next_back().unwrap_or(' ');
    let first = first_char(&tail);
    // A newline already separates the two sides, and a space after one would
    // indent the line.
    if first == '\n' || last == '\n' || closes(first) || opens(last) {
        format!("{head}{tail}")
    } else {
        format!("{head} {tail}")
    }
}

/// Assembles the pieces, cleaning up only where they meet.
fn render(pieces: &[Piece<'_>]) -> String {
    let mut out = String::new();
    let mut capitalize = false;
    for piece in pieces {
        match piece {
            Piece::Removed { started_upper } => capitalize |= *started_upper,
            Piece::Kept(text) | Piece::Inserted(text) => {
                out = join(&out, text, capitalize);
                capitalize = false;
            }
        }
    }
    out.trim_end_matches([' ', '\t']).to_string()
}

/// Splits one entry's trigger field into the individual spoken phrases its
/// commas separate. `" um , uh,er "` yields `["um", "uh", "er"]`.
fn alternatives(trigger: &str) -> Vec<String> {
    trigger
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The canonical form of an entry's trigger field, used for storing and for
/// spotting duplicates: alternatives trimmed and joined with `", "`.
fn normalize_trigger(trigger: &str) -> String {
    alternatives(trigger).join(", ")
}

/// Replaces every whole-word occurrence of each trigger with its replacement.
pub fn apply(text: &str, snippets: &[Snippet]) -> String {
    let mut pairs: Vec<(String, &str)> = Vec::new();
    for snippet in snippets {
        for trigger in alternatives(&snippet.trigger) {
            pairs.push((fold(&trigger), snippet.replacement.as_str()));
        }
    }
    if pairs.is_empty() {
        return text.to_string();
    }
    // Longest trigger wins, so "my work email" beats "my email".
    pairs.sort_by_key(|pair| std::cmp::Reverse(pair.0.len()));

    let haystack = fold(text);
    let mut pieces: Vec<Piece> = Vec::new();
    let mut cursor = 0usize;
    let mut kept_from = 0usize;

    while cursor < text.len() {
        let mut hit: Option<(usize, &str)> = None;
        for (trigger, replacement) in &pairs {
            if !haystack[cursor..].starts_with(trigger.as_str()) {
                continue;
            }
            let end = cursor + trigger.len();
            if !at_start(&haystack, cursor) || !at_end(&haystack, end) {
                continue;
            }
            hit = Some((end, replacement));
            break;
        }

        match hit {
            Some((end, replacement)) => {
                if kept_from < cursor {
                    pieces.push(Piece::Kept(&text[kept_from..cursor]));
                }
                if replacement.is_empty() {
                    pieces.push(Piece::Removed {
                        started_upper: text[cursor..end]
                            .chars()
                            .next()
                            .is_some_and(char::is_uppercase),
                    });
                } else {
                    pieces.push(Piece::Inserted(replacement));
                }
                cursor = end;
                kept_from = end;
            }
            None => {
                // Step one whole character so a multi-byte scalar is never split.
                let Some(ch) = text[cursor..].chars().next() else {
                    break;
                };
                cursor += ch.len_utf8();
            }
        }
    }

    // Nothing matched: hand back the transcript byte for byte rather than a
    // re-rendered copy of it.
    if pieces.is_empty() {
        return text.to_string();
    }
    if kept_from < text.len() {
        pieces.push(Piece::Kept(&text[kept_from..]));
    }
    render(&pieces)
}

/// Trims, drops unusable entries, removes case-insensitive duplicate triggers
/// and caps the list. Returns what was actually kept.
pub fn clean(snippets: Vec<Snippet>) -> Vec<Snippet> {
    let mut kept: Vec<Snippet> = Vec::new();
    for snippet in snippets {
        let trigger = normalize_trigger(&snippet.trigger);
        let replacement = snippet.replacement.trim().to_string();
        if trigger.is_empty() || trigger.chars().count() > MAX_TRIGGER_CHARS {
            continue;
        }
        if replacement.chars().count() > MAX_REPLACEMENT_CHARS {
            continue;
        }
        if kept
            .iter()
            .any(|existing| existing.trigger.eq_ignore_ascii_case(&trigger))
        {
            continue;
        }
        kept.push(Snippet {
            trigger,
            replacement,
        });
        if kept.len() >= MAX_SNIPPETS {
            break;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(trigger: &str, replacement: &str) -> Snippet {
        Snippet {
            trigger: trigger.to_string(),
            replacement: replacement.to_string(),
        }
    }

    fn snippets() -> Vec<Snippet> {
        vec![
            snippet("my email", "zach@example.com"),
            snippet("my work email", "zach@company.com"),
            snippet("sig", "Best,\nZach"),
        ]
    }

    #[test]
    fn expands_a_phrase() {
        assert_eq!(
            apply("send it to my email please", &snippets()),
            "send it to zach@example.com please"
        );
    }

    #[test]
    fn ignores_case() {
        assert_eq!(
            apply("Send it to My Email please", &snippets()),
            "Send it to zach@example.com please"
        );
        assert_eq!(
            apply("SEND IT TO MY EMAIL", &snippets()),
            "SEND IT TO zach@example.com"
        );
    }

    #[test]
    fn prefers_the_longest_trigger() {
        assert_eq!(
            apply("use my work email", &snippets()),
            "use zach@company.com"
        );
    }

    #[test]
    fn never_matches_inside_a_longer_word() {
        // "sig" must not fire inside "signal", and "my email" not inside "my emails".
        assert_eq!(apply("the signal is my emails", &snippets()), "the signal is my emails");
    }

    #[test]
    fn keeps_surrounding_punctuation() {
        assert_eq!(
            apply("write to my email.", &snippets()),
            "write to zach@example.com."
        );
        assert_eq!(
            apply("(my email)", &snippets()),
            "(zach@example.com)"
        );
    }

    #[test]
    fn does_not_expand_its_own_output() {
        // The replacement contains another trigger; it must be left alone.
        let list = vec![
            snippet("shortcut", "my email"),
            snippet("my email", "zach@example.com"),
        ];
        assert_eq!(apply("use shortcut", &list), "use my email");
    }

    #[test]
    fn empty_replacement_deletes_the_phrase() {
        let list = vec![snippet("um", "")];
        assert_eq!(apply("so um I think", &list), "so I think");
    }

    #[test]
    fn one_entry_holds_several_alternatives() {
        let list = vec![snippet("um, uh, er", "")];
        assert_eq!(
            apply("so um, uh, er, I think", &list),
            "so, I think"
        );
    }

    #[test]
    fn alternatives_are_trimmed_and_ignore_case() {
        let list = vec![snippet("  um ,UH,  er ", "")];
        // "UM" began the sentence, so the word that now starts it gets its
        // capital back.
        assert_eq!(apply("UM and Uh and er", &list), "And and");
    }

    #[test]
    fn a_filler_snippet_leaves_the_rest_of_the_transcript_alone() {
        // Cleanup used to run over the whole transcript as soon as any snippet
        // with an empty replacement existed, so punctuation it never touched
        // was rewritten: "Wait... what?" came back as "Wait. what?".
        let list = vec![snippet("um", "")];
        assert_eq!(apply("Wait... what?", &list), "Wait... what?");
        assert_eq!(apply("No!! Really?", &list), "No!! Really?");
        assert_eq!(apply("e.g.  this", &list), "e.g.  this");
    }

    #[test]
    fn an_ellipsis_survives_a_deletion_next_to_it() {
        let list = vec![snippet("um", "")];
        assert_eq!(apply("Well, um... anyway", &list), "Well... anyway");
    }

    #[test]
    fn deleting_a_sentence_start_restores_the_capital() {
        let list = vec![snippet("I mean, I guess", "")];
        assert_eq!(
            apply("I mean, I guess that's fine.", &list),
            "That's fine."
        );
        assert_eq!(
            apply("Fine. I mean, I guess that's it.", &list),
            "Fine. That's it."
        );
    }

    #[test]
    fn a_removed_filler_does_not_capitalise_the_next_word() {
        // "so" was lowercase because a comma preceded it, and a lowercase
        // filler is not a sentence start.
        let list = vec![snippet("um", "")];
        assert_eq!(apply("um so I think", &list), "so I think");
    }

    #[test]
    fn very_filler_heavy_dictation_reads_like_prose() {
        let list = vec![snippet("um, uh", "")];
        assert_eq!(
            apply("So, um, I was thinking. Uh, maybe we go.", &list),
            "So, I was thinking. Maybe we go."
        );
    }

    #[test]
    fn a_replacement_keeps_the_casing_it_was_typed_with() {
        let list = vec![snippet("my handle", "zachOnGithub")];
        assert_eq!(apply("my handle is me", &list), "zachOnGithub is me");
    }

    #[test]
    fn multi_paragraph_text_keeps_its_line_breaks() {
        let list = vec![snippet("um", ""), snippet("my email", "zach@example.com")];
        assert_eq!(
            apply("first line\nmy email here", &list),
            "first line\nzach@example.com here"
        );
        assert_eq!(
            apply("first line\num second line", &list),
            "first line\nsecond line"
        );
    }

    #[test]
    fn a_deletion_beside_a_replacement_joins_cleanly() {
        let list = vec![snippet("um", ""), snippet("my email", "zach@example.com")];
        assert_eq!(
            apply("um my email please", &list),
            "zach@example.com please"
        );
    }

    #[test]
    fn an_alternative_expands_to_the_shared_replacement() {
        let list = vec![snippet("my email, my address", "zach@example.com")];
        assert_eq!(
            apply("send it to my address or my email", &list),
            "send it to zach@example.com or zach@example.com"
        );
    }

    #[test]
    fn the_longest_alternative_wins_across_entries() {
        let list = vec![
            snippet("email, address", "zach@example.com"),
            snippet("work address", "zach@company.com"),
        ];
        assert_eq!(
            apply("my work address", &list),
            "my zach@company.com"
        );
    }

    #[test]
    fn deleting_filler_words_cleans_up_punctuation() {
        let list = vec![snippet("um, uh", "")];
        assert_eq!(apply("so, um, I think, uh.", &list), "so, I think.");
        assert_eq!(apply("um hello uh", &list), "hello");
    }

    #[test]
    fn clean_normalizes_the_alternative_spacing() {
        let list = clean(vec![snippet("um ,  uh,er", ""), snippet("UM, UH, ER", "x")]);
        assert_eq!(list, vec![snippet("um, uh, er", "")]);
    }

    #[test]
    fn treats_punctuation_literally() {
        let list = vec![snippet("a.b", "alpha beta")];
        assert_eq!(apply("a.b and axb", &list), "alpha beta and axb");
    }

    #[test]
    fn leaves_unrelated_text_alone() {
        let text = "the quick brown fox jumps over the lazy dog";
        assert_eq!(apply(text, &snippets()), text);
    }

    #[test]
    fn handles_multibyte_text() {
        let text = "café my email naïve";
        assert_eq!(
            apply(text, &snippets()),
            "café zach@example.com naïve"
        );
    }

    #[test]
    fn clean_drops_blank_and_duplicate_entries() {
        let list = clean(vec![
            snippet("  my email ", " zach@example.com "),
            snippet("MY EMAIL", "other@example.com"),
            snippet("   ", "x"),
            snippet("sig", "Best"),
        ]);
        assert_eq!(
            list,
            vec![
                snippet("my email", "zach@example.com"),
                snippet("sig", "Best"),
            ]
        );
    }
}
