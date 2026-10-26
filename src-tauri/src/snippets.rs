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

/// Cleanup after a deletion (`""` replacement): filler words rarely leave tidy
/// punctuation behind, so "so, um, I think" would otherwise become
/// "so, , I think". Collapses the doubled spaces and the orphaned punctuation.
fn tidy_deletions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c == ' ' {
            pending_space = true;
            continue;
        }
        if matches!(c, ',' | ';' | ':' | '.' | '!' | '?') {
            // No space belongs before punctuation; drop a dangling comma at the
            // very start, and never repeat the same mark twice.
            pending_space = false;
            if out.is_empty() {
                continue;
            }
            if out.ends_with(c) {
                continue;
            }
            // "I think, ." loses the comma that had nothing left to separate.
            if matches!(c, '.' | '!' | '?')
                && matches!(out.chars().next_back(), Some(',') | Some(';') | Some(':'))
            {
                out.pop();
                if out.is_empty() {
                    continue;
                }
            }
            out.push(c);
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out.trim().to_string()
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
    let mut deletes = false;
    for snippet in snippets {
        for trigger in alternatives(&snippet.trigger) {
            if snippet.replacement.is_empty() {
                deletes = true;
            }
            pairs.push((fold(&trigger), snippet.replacement.as_str()));
        }
    }
    if pairs.is_empty() {
        return text.to_string();
    }
    // Longest trigger wins, so "my work email" beats "my email".
    pairs.sort_by_key(|pair| std::cmp::Reverse(pair.0.len()));

    let haystack = fold(text);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;

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
                out.push_str(replacement);
                cursor = end;
            }
            None => {
                // Copy one whole character so a multi-byte scalar is never split.
                let Some(ch) = text[cursor..].chars().next() else {
                    break;
                };
                out.push(ch);
                cursor += ch.len_utf8();
            }
        }
    }

    // An empty replacement (deleting filler words) can leave doubled spaces and
    // stranded commas behind.
    if deletes {
        tidy_deletions(&out)
    } else {
        out
    }
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
        assert_eq!(apply("UM and Uh and er", &list), "and and");
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
