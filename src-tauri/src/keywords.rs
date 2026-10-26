//! Fuzzy correction for names and terms in a transcript.
//!
//! Ported from Handy's custom-words matcher (Double Metaphone + Levenshtein,
//! 1–3 gram search) through the standalone `keywords` CLI. Runs in-process:
//! no subprocess, no model, a few milliseconds per dictation.

use once_cell::sync::Lazy;
use rphonetic::{DoubleMetaphone, Encoder};

// ---------------------------------------------------------------------------
// Matcher (ported from Handy src-tauri/src/audio_toolkit/text.rs)
// ---------------------------------------------------------------------------

static DOUBLE_METAPHONE: Lazy<DoubleMetaphone> =
    Lazy::new(|| DoubleMetaphone::new(Some(64)));

const MIN_PHONETIC_CODE_LEN: usize = 3;
const STRONG_PHONETIC_CODE_LEN: usize = 5;
const PHONETIC_SCORE_WEIGHT: f64 = 0.3;
const STRONG_PHONETIC_SCORE_WEIGHT: f64 = 0.2;
const DEFAULT_THRESHOLD: f64 = 0.18;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhoneticCode {
    primary: String,
    alternate: String,
}

impl PhoneticCode {
    fn encode(word: &str) -> Option<Self> {
        if !supports_phonetics(word) {
            return None;
        }
        let primary = DOUBLE_METAPHONE.encode(word);
        let alternate = DOUBLE_METAPHONE.encode_alternate(word);
        if primary.chars().count() < MIN_PHONETIC_CODE_LEN {
            return None;
        }
        Some(Self { primary, alternate })
    }

    fn matches(&self, other: &Self) -> bool {
        self.primary == other.primary
            || self.primary == other.alternate
            || self.alternate == other.primary
            || self.alternate == other.alternate
    }

    fn len(&self) -> usize {
        self.primary.chars().count()
    }
}

fn build_ngram(words: &[&str]) -> String {
    words
        .iter()
        .map(|w| build_match_key(w))
        .collect::<Vec<_>>()
        .concat()
}

fn build_match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

struct CustomWordMatchKey {
    word_index: usize,
    key: String,
    phonetic: Option<PhoneticCode>,
}

impl CustomWordMatchKey {
    fn new(word_index: usize, key: String) -> Self {
        let phonetic = PhoneticCode::encode(&key);
        Self {
            word_index,
            key,
            phonetic,
        }
    }
}

fn build_custom_word_match_keys(word: &str, word_index: usize) -> Vec<CustomWordMatchKey> {
    let primary_key = build_match_key(word);
    let mut keys = Vec::with_capacity(2);

    if is_supported_fuzzy_key(&primary_key) {
        keys.push(CustomWordMatchKey::new(word_index, primary_key.clone()));
    }

    if word.contains('&') {
        let expanded_key = build_match_key(&word.replace('&', " and "));
        if is_supported_fuzzy_key(&expanded_key) && expanded_key != primary_key {
            keys.push(CustomWordMatchKey::new(word_index, expanded_key));
        }
    }

    keys
}

fn is_supported_fuzzy_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())
}

fn supports_phonetics(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic())
}

/// Words that carry no content of their own. A phrase made only of these plus
/// one real word says nothing about the keyword it looks like.
fn is_function_word(word: &str) -> bool {
    matches!(
        build_match_key(word).as_str(),
        "the"
            | "a"
            | "an"
            | "is"
            | "are"
            | "was"
            | "of"
            | "and"
            | "or"
            | "to"
            | "it"
            | "in"
            | "on"
            | "at"
            | "for"
    )
}

fn find_best_match<'a>(
    candidate: &str,
    custom_words: &'a [String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
    phonetic_ok: bool,
) -> Option<(&'a String, f64)> {
    if !is_supported_fuzzy_key(candidate) || candidate.chars().count() > 50 {
        return None;
    }

    let mut best_match: Option<&String> = None;
    let mut best_score = f64::MAX;

    // A span of several words is glued into one blob before it gets here, so its
    // phonetic code is a consonant skeleton rather than a word's sound: "the pill
    // is" and "Typeless" both encode to TPLS. With the phonetic bonus that lands
    // under the threshold and rewrites speech the user never said, so the bonus
    // is reserved for single words and for keywords that carry function words of
    // their own ("Art of War" needs it for its own mishearings).
    let span_phonetic = PhoneticCode::encode(candidate);

    for custom_word_key in custom_word_match_keys {
        let phonetic_ok = phonetic_ok
            || custom_words[custom_word_key.word_index]
                .split_whitespace()
                .any(is_function_word);
        let phonetic_match = if phonetic_ok {
            match (&span_phonetic, &custom_word_key.phonetic) {
                (Some(candidate_code), Some(key_code)) => candidate_code.matches(key_code),
                _ => false,
            }
        } else {
            false
        };

        let candidate_len = candidate.chars().count();
        let custom_word_len = custom_word_key.key.chars().count();
        let len_diff = candidate_len.abs_diff(custom_word_len) as f64;
        let max_len = candidate_len.max(custom_word_len) as f64;
        let len_ratio = if phonetic_match { 0.5 } else { 0.25 };
        let max_allowed_diff = (max_len * len_ratio).max(2.0);
        if len_diff > max_allowed_diff {
            continue;
        }

        let levenshtein_dist = strsim::levenshtein(candidate, &custom_word_key.key);
        let levenshtein_score = if max_len > 0.0 {
            levenshtein_dist as f64 / max_len
        } else {
            1.0
        };

        let combined_score = if phonetic_match {
            let strong = span_phonetic
                .as_ref()
                .is_some_and(|code| code.len() >= STRONG_PHONETIC_CODE_LEN);
            let weight = if strong {
                STRONG_PHONETIC_SCORE_WEIGHT
            } else {
                PHONETIC_SCORE_WEIGHT
            };
            levenshtein_score * weight
        } else {
            levenshtein_score
        };

        if combined_score < threshold && combined_score < best_score {
            best_match = Some(&custom_words[custom_word_key.word_index]);
            best_score = combined_score;
        }
    }

    best_match.map(|m| (m, best_score))
}

pub fn apply_custom_words(text: &str, custom_words: &[String], threshold: f64) -> String {
    if custom_words.is_empty() {
        return text.to_string();
    }

    let custom_word_match_keys: Vec<CustomWordMatchKey> = custom_words
        .iter()
        .enumerate()
        .flat_map(|(index, word)| build_custom_word_match_keys(word, index))
        .collect();

    let words: Vec<&str> = text.split_whitespace().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let mut best_match: Option<(usize, &String, f64)> = None;

        for n in (1..=3).rev() {
            if i + n > words.len() {
                continue;
            }

            let ngram_words = &words[i..i + n];
            if ngram_words[..n.saturating_sub(1)]
                .iter()
                .any(|word| !extract_punctuation(word).1.is_empty())
            {
                continue;
            }
            let ngram = build_ngram(ngram_words);
            // Multi-word spans may only use the phonetic bonus when every word in
            // them carries content; see find_best_match.
            let phonetic_ok = n == 1 || !ngram_words.iter().any(|word| is_function_word(word));

            if let Some((replacement, score)) = find_best_match(
                &ngram,
                custom_words,
                &custom_word_match_keys,
                threshold,
                phonetic_ok,
            ) {
                let is_better = best_match
                    .as_ref()
                    .is_none_or(|(_, _, best_score)| score < *best_score);
                if is_better {
                    best_match = Some((n, replacement, score));
                }
            }
        }

        if let Some((n, replacement, _)) = best_match {
            let ngram_words = &words[i..i + n];
            let (prefix, _) = extract_punctuation(ngram_words[0]);
            let (_, suffix) = extract_punctuation(ngram_words[n - 1]);

            let corrected = preserve_case_pattern_for_span(ngram_words, replacement);

            result.push(format!("{}{}{}", prefix, corrected, suffix));
            i += n;
        } else {
            let rewritten = apply_custom_words_within_token(
                words[i],
                custom_words,
                &custom_word_match_keys,
                threshold,
            );
            result.push(rewritten.unwrap_or_else(|| words[i].to_string()));
            i += 1;
        }
    }

    result.join(" ")
}

fn has_intentional_case(replacement: &str) -> bool {
    let mut chars = replacement.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    chars.any(|c| c.is_uppercase()) && (first.is_lowercase() || chars.any(|c| c.is_lowercase()))
}

fn is_shouted(words: &[&str]) -> bool {
    let letters: usize = words
        .iter()
        .flat_map(|word| word.chars())
        .filter(|c| c.is_alphabetic())
        .count();

    letters > 1
        && words
            .iter()
            .flat_map(|word| word.chars())
            .filter(|c| c.is_alphabetic())
            .all(char::is_uppercase)
}

fn preserve_case_pattern_for_span(original: &[&str], replacement: &str) -> String {
    let first = original.first().copied().unwrap_or("");

    if is_shouted(original) {
        replacement.to_uppercase()
    } else if has_intentional_case(replacement) {
        replacement.to_string()
    } else if first.chars().next().is_some_and(|c| c.is_uppercase()) {
        let mut chars: Vec<char> = replacement.chars().collect();
        if let Some(first_char) = chars.get_mut(0) {
            *first_char = first_char.to_uppercase().next().unwrap_or(*first_char);
        }
        chars.into_iter().collect()
    } else {
        replacement.to_string()
    }
}

fn split_token_segments(token: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_is_alnum = true;

    for c in token.chars() {
        let is_alnum = c.is_alphanumeric();
        if is_alnum != current_is_alnum && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
            current_is_alnum = is_alnum;
        } else if current.is_empty() {
            current_is_alnum = is_alnum;
        }
        current.push(c);
    }

    if !current.is_empty() {
        segments.push(current);
    }

    if segments
        .first()
        .is_some_and(|segment| !segment.chars().next().is_some_and(char::is_alphanumeric))
    {
        segments.insert(0, String::new());
    }

    segments
}

fn apply_custom_words_within_token(
    token: &str,
    custom_words: &[String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
) -> Option<String> {
    let mut segments = split_token_segments(token);
    if segments.len() < 3 {
        return None;
    }

    let mut matched = false;
    for index in (0..segments.len()).step_by(2) {
        let run = &segments[index].clone();
        let key = build_match_key(run);
        if key.is_empty() {
            continue;
        }

        if let Some((replacement, _)) =
            find_best_match(&key, custom_words, custom_word_match_keys, threshold, true)
        {
            segments[index] =
                preserve_case_pattern_for_span(&[run.as_str()], replacement);
            matched = true;
        }
    }

    matched.then(|| segments.concat())
}

fn extract_punctuation(word: &str) -> (&str, &str) {
    let prefix_end = word
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, _)| index)
        .unwrap_or(word.len());
    let suffix_start = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, c)| index + c.len_utf8())
        .unwrap_or(0);

    let prefix = if prefix_end > 0 {
        &word[..prefix_end]
    } else {
        ""
    };

    let suffix = if suffix_start < word.len() {
        &word[suffix_start..]
    } else {
        ""
    };

    (prefix, suffix)
}

/// Corrects a transcript using the user's keywords.
pub fn apply(text: &str, custom_words: &[String]) -> String {
    apply_custom_words(text, custom_words, DEFAULT_THRESHOLD)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<String> {
        [
            "Spokenly", "Typeless", "OpenCode", "Ghostty", "Freebuff", "DeepSeek",
            "CotEditor", "Pearcleaner", "ATLauncher", "macOS", "Nemotron", "Parakeet",
            "Handy", "FluidAudio", "Hugging Face", "llama.cpp", "GGUF", "MLX", "Core ML",
            "BF16", "Q8_0", "Q6_K", "Superwhisper", "Wispr Flow", "WhisperKit",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn corrects_common_mishearings() {
        let words = words();
        let cases = [
            ("spokenly", "Spokenly"),
            ("type less", "Typeless"),
            ("open code", "OpenCode"),
            ("ghosty", "Ghostty"),
            ("free buff", "Freebuff"),
            ("deep seek", "DeepSeek"),
            ("cot editor", "CotEditor"),
            ("para cleaner", "Pearcleaner"),
            ("at launcher", "ATLauncher"),
            ("macos", "macOS"),
            ("nemo tron", "Nemotron"),
            ("parakeet", "Parakeet"),
            ("handy", "Handy"),
            ("fluid audio", "FluidAudio"),
            ("hugging face", "Hugging Face"),
            ("llama cpp", "llama.cpp"),
            ("gguf", "GGUF"),
            ("mlx", "MLX"),
            ("core ml", "Core ML"),
            ("bf16", "BF16"),
            ("q8 0", "Q8_0"),
            ("q6 k", "Q6_K"),
            ("super whisper", "Superwhisper"),
            ("whisper flow", "Wispr Flow"),
            ("whisper kit", "WhisperKit"),
        ];
        for (input, expected) in cases {
            assert_eq!(apply(input, &words), expected, "input: {input:?}");
        }
    }

    #[test]
    fn leaves_unrelated_text_alone() {
        let words = words();
        assert_eq!(
            apply("the quick brown fox jumps over the lazy dog", &words),
            "the quick brown fox jumps over the lazy dog"
        );
    }

    /// "the pill is" glues to "thepillis", whose metaphone alternate is TPLS —
    /// the same code as "Typeless". A function word must not be enough to pull
    /// an unrelated phrase onto a keyword.
    #[test]
    fn function_word_spans_do_not_hijack_a_keyword() {
        let words = words();
        for input in [
            "I think the pill is probably done now",
            "the pill is done",
            "the people are here",
            "a typo in the code",
        ] {
            assert_eq!(apply(input, &words), input, "input: {input:?}");
        }
    }

    /// The guard only applies to spans carrying function words the keyword does
    /// not have, so keywords that contain them keep correcting their own
    /// mishearings.
    #[test]
    fn keywords_with_function_words_still_correct() {
        let words: Vec<String> = ["Art of War", "The Verge", "Band of Brothers", "Freebuff"]
            .into_iter()
            .map(String::from)
            .collect();
        let cases = [
            ("art of wore", "Art of War"),
            ("the vurge", "The Verge"),
            ("band of bruther's", "Band of Brothers"),
            ("free buf", "Freebuff"),
        ];
        for (input, expected) in cases {
            assert_eq!(apply(input, &words), expected, "input: {input:?}");
        }
    }
}
