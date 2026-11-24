//! English spell checking with the macOS system checker (NSSpellChecker): same
//! dictionaries and learned words as TextEdit/Pages. Must be used on the main thread.

use std::collections::HashMap;
use std::sync::Arc;

/// A misspelled word, as char indices into the checked text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Miss {
    pub start: usize,
    pub end: usize,
    pub word: String,
}

/// Char ranges inside `{...}` placeholders, which are never flagged.
fn placeholder_spans(text: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut open = None;
    for (i, c) in text.chars().enumerate() {
        match c {
            '{' => open = Some(i),
            '}' => {
                if let Some(s) = open.take() {
                    v.push((s, i + 1));
                }
            }
            _ => {}
        }
    }
    v
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2_app_kit::NSSpellChecker;
    use objc2_foundation::{NSRange, NSString};

    pub struct Native {
        tag: isize,
        lang: Retained<NSString>,
    }

    impl Native {
        pub fn new() -> Self {
            let sc = NSSpellChecker::sharedSpellChecker();
            let langs: Vec<String> = sc.availableLanguages().iter().map(|s| s.to_string()).collect();
            let pick = ["en_US", "en", "en_GB"]
                .into_iter()
                .find(|l| langs.iter().any(|x| x == l))
                .unwrap_or("en");
            Self { tag: NSSpellChecker::uniqueSpellDocumentTag(), lang: NSString::from_str(pick) }
        }

        /// Misspelled ranges as UTF-16 (location, length).
        pub fn check(&self, text: &str) -> Vec<(usize, usize)> {
            let sc = NSSpellChecker::sharedSpellChecker();
            let ns = NSString::from_str(text);
            let len = ns.length();
            let mut out = Vec::new();
            let mut offset = 0usize;
            while offset < len {
                let mut words: isize = 0;
                // SAFETY: valid NSString, valid out-pointer for the word count.
                let r: NSRange = unsafe {
                    sc.checkSpellingOfString_startingAt_language_wrap_inSpellDocumentWithTag_wordCount(
                        &ns,
                        offset as isize,
                        Some(&self.lang),
                        false,
                        self.tag,
                        &mut words,
                    )
                };
                // NSNotFound = done; a range behind `offset` or past the end would loop or overflow.
                if r.length == 0 || r.location < offset || r.location.saturating_add(r.length) > len {
                    break;
                }
                out.push((r.location, r.length));
                offset = r.location + r.length;
            }
            out
        }

        pub fn guesses(&self, text: &str, loc: usize, len: usize) -> Vec<String> {
            let sc = NSSpellChecker::sharedSpellChecker();
            let ns = NSString::from_str(text);
            sc.guessesForWordRange_inString_language_inSpellDocumentWithTag(
                NSRange::new(loc, len),
                &ns,
                Some(&self.lang),
                self.tag,
            )
            .map(|a| a.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
        }

        pub fn learn(&self, word: &str) {
            NSSpellChecker::sharedSpellChecker().learnWord(&NSString::from_str(word));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub struct Native;
    impl Native {
        pub fn new() -> Self {
            Self
        }
        pub fn check(&self, _: &str) -> Vec<(usize, usize)> {
            vec![]
        }
        pub fn guesses(&self, _: &str, _: usize, _: usize) -> Vec<String> {
            vec![]
        }
        pub fn learn(&self, _: &str) {}
    }
}

pub struct Speller {
    native: imp::Native,
    cache: HashMap<String, Arc<Vec<Miss>>>,
}

impl Default for Speller {
    fn default() -> Self {
        Self::new()
    }
}

/// UTF-16 offset -> char index table (len = utf16 length + 1).
fn utf16_to_char(text: &str) -> Vec<usize> {
    let mut map = Vec::with_capacity(text.len() + 1);
    for (ci, c) in text.chars().enumerate() {
        for _ in 0..c.len_utf16() {
            map.push(ci);
        }
    }
    map.push(text.chars().count());
    map
}

fn char_to_utf16(text: &str, ci: usize) -> usize {
    text.chars().take(ci).map(char::len_utf16).sum()
}

impl Speller {
    pub fn new() -> Self {
        Self { native: imp::Native::new(), cache: HashMap::new() }
    }

    /// Misspellings in `text` (cached per distinct text).
    pub fn check(&mut self, text: &str) -> Arc<Vec<Miss>> {
        if let Some(m) = self.cache.get(text) {
            return m.clone();
        }
        let misses = to_misses(text, self.native.check(text));
        if self.cache.len() > 256 {
            self.cache.clear();
        }
        let m = Arc::new(misses);
        self.cache.insert(text.to_string(), m.clone());
        m
    }

    pub fn suggestions(&self, text: &str, m: &Miss) -> Vec<String> {
        let loc = char_to_utf16(text, m.start);
        let len = char_to_utf16(text, m.end) - loc;
        let mut g = self.native.guesses(text, loc, len);
        g.truncate(6);
        g
    }

    /// Add to the user's macOS dictionary.
    pub fn learn(&mut self, word: &str) {
        self.native.learn(word);
        self.cache.clear();
    }
}

/// UTF-16 (location, length) ranges from the checker -> char-indexed misses, dropping
/// ranges inside placeholders or outside `text`.
fn to_misses(text: &str, ranges: Vec<(usize, usize)>) -> Vec<Miss> {
    let map = utf16_to_char(text);
    let skip = placeholder_spans(text);
    let chars: Vec<char> = text.chars().collect();
    ranges
        .into_iter()
        .filter_map(|(loc, len)| {
            let start = *map.get(loc)?;
            let end = *map.get(loc.checked_add(len)?)?;
            if skip.iter().any(|&(a, b)| start < b && end > a) {
                return None;
            }
            Some(Miss { start, end, word: chars[start..end].iter().collect() })
        })
        .collect()
}

/// Replace chars `start..end` of `text` with `with`.
pub fn replace_chars(text: &mut String, start: usize, end: usize, with: &str) {
    let byte = |ci: usize| text.char_indices().nth(ci).map_or(text.len(), |(b, _)| b);
    let (a, b) = (byte(start), byte(end));
    text.replace_range(a..b, with);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn flags_typos_not_placeholders() {
        let mut s = Speller::new();
        let text = "Teh quick brwn fox {brwn} {n:03} café";
        let m = s.check(text);
        let words: Vec<&str> = m.iter().map(|x| x.word.as_str()).collect();
        assert!(words.contains(&"Teh"), "{words:?}");
        assert_eq!(words.iter().filter(|&&w| w == "brwn").count(), 1, "{words:?}");
        let brwn = m.iter().find(|x| x.word == "brwn").unwrap();
        assert_eq!(brwn.start, 10);
        assert!(s.suggestions(text, brwn).iter().any(|g| g == "brown"));
    }

    #[test]
    fn utf16_ranges_map_to_chars() {
        // UTF-16: "Grüße"=0..5, 😀=6..8 (surrogate pair), "Teh"=9..12, "{n:03}"=13..19, "brwn"=20..24.
        let text = "Grüße 😀 Teh {n:03} brwn";
        let ranges = vec![(0, 5), (9, 3), (14, 1), (20, 4), (30, 2), (22, 5), (20, usize::MAX)];
        let m = to_misses(text, ranges);
        let got: Vec<(usize, usize, &str)> = m.iter().map(|x| (x.start, x.end, x.word.as_str())).collect();
        assert_eq!(got, [(0, 5, "Grüße"), (8, 11, "Teh"), (19, 23, "brwn")]);
        assert_eq!((char_to_utf16(text, 19), char_to_utf16(text, 23)), (20, 24));
        let mut t = text.to_string();
        replace_chars(&mut t, m[2].start, m[2].end, "brown");
        assert_eq!(t, "Grüße 😀 Teh {n:03} brown");
    }
}
