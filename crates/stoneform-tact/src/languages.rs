//! A game's text and spoken language, and where a tag string names them.
//!
//! In `… enUS speech?:… deDE text?` the set that names `speech` picks the
//! spoken-language files and the one that names `text` the text-language
//! files (see [`crate::tags::select`]).

/// A game's text and spoken language, as Blizzard locale codes (`enUS`, `deDE`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Languages {
    pub text: String,
    pub speech: String,
}

impl Languages {
    /// Text in `text`, speech in `speech` (or `text` too, when `None`).
    pub fn new(text: impl Into<String>, speech: Option<&str>) -> Self {
        let text = text.into();
        let speech = speech.map_or_else(|| text.clone(), str::to_owned);
        Self { text, speech }
    }
}

const MARKERS: [&str; 4] = ["speech", "speech?", "text", "text?"];

/// The spoken language a tag string installs: the locale in its `speech` set.
pub fn speech(tags: &str) -> Option<&str> {
    locale_in(tags, "speech")
}

/// The text language a tag string installs: the locale in its `text` set.
pub fn text(tags: &str) -> Option<&str> {
    locale_in(tags, "text")
}

/// `tags` installing `languages`: each set's locale swapped in place, so
/// everything else Battle.net put there (`acct-CZE?`, content tags) stays.
/// A string without one speech set and one text set becomes both, from its
/// first set.
pub fn replace(tags: &str, languages: &Languages) -> String {
    let mut sets: Vec<String> = tags.split(':').map(str::to_owned).collect();
    let only = |marker: &str, other: &str| {
        sets.iter().position(|set| {
            let names = names(set);
            names.contains(&marker) && !names.contains(&other)
        })
    };
    match (only("speech", "text"), only("text", "speech")) {
        (Some(speech_set), Some(text_set)) => {
            sets[speech_set] = swap(&sets[speech_set], &languages.speech);
            sets[text_set] = swap(&sets[text_set], &languages.text);
            sets.join(":")
        }
        _ => {
            let base: Vec<&str> = sets[0].split_whitespace().filter(|word| !MARKERS.contains(word)).collect();
            let base = base.join(" ");
            format!("{} speech?:{} text?", swap(&base, &languages.speech), swap(&base, &languages.text))
        }
    }
}

/// Whether `word` looks like a locale: two lowercase letters, then two
/// uppercase (`enUS`, `zhTW`).
pub fn is_locale(word: &str) -> bool {
    let bytes = word.as_bytes();
    bytes.len() == 4 && bytes[..2].iter().all(u8::is_ascii_lowercase) && bytes[2..].iter().all(u8::is_ascii_uppercase)
}

/// A set's words, each without its trailing `?`.
fn names(set: &str) -> Vec<&str> {
    set.split_whitespace().map(|word| word.strip_suffix('?').unwrap_or(word)).collect()
}

fn locale_in<'a>(tags: &'a str, marker: &str) -> Option<&'a str> {
    let words = names(tags.split(':').filter(|set| !set.is_empty()).find(|set| names(set).contains(&marker))?);
    words.into_iter().find(|word| is_locale(word))
}

/// One set with its locale replaced (any others dropped), or added before the
/// speech/text marker (or at the end) when it names none.
fn swap(set: &str, locale: &str) -> String {
    let is_locale_word = |word: &str| is_locale(word.strip_suffix('?').unwrap_or(word));
    let mut words: Vec<String> = set.split_whitespace().map(str::to_owned).collect();
    if let Some(first) = words.iter().position(|word| is_locale_word(word)) {
        words[first] = if words[first].ends_with('?') { format!("{locale}?") } else { locale.to_owned() };
        let mut index = 0;
        words.retain(|word| {
            let keep = index == first || !is_locale_word(word);
            index += 1;
            keep
        });
    } else if let Some(marker) = words.iter().position(|word| MARKERS.contains(&word.as_str())) {
        words.insert(marker, locale.to_owned());
    } else {
        words.push(locale.to_owned());
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hearthstone's tags as Battle.net wrote them.
    const BATTLE_NET_TAGS: &str =
        "OSX adventure base EU? acct-CZE? geoip-NL? enUS speech?:OSX adventure base EU? acct-CZE? geoip-NL? enUS text?";

    /// Port of Waypoint's `readsLanguagesFromTags`.
    #[test]
    fn reads_languages_from_tags() {
        assert_eq!(text(BATTLE_NET_TAGS), Some("enUS"));
        assert_eq!(speech(BATTLE_NET_TAGS), Some("enUS"));
        let mixed = "OSX x86_64 arm64 EU? deDE speech?:OSX x86_64 arm64 EU? ruRU text?";
        assert_eq!(speech(mixed), Some("deDE"));
        assert_eq!(text(mixed), Some("ruRU"));
        assert_eq!(text("OSX EU enUS"), None);
        for word in ["OSX", "EU", "arm64", "x86_64", "acct-CZE", "geoip-NL", "base"] {
            assert!(!is_locale(word), "{word}");
        }
        for word in ["enUS", "zhTW", "ptBR"] {
            assert!(is_locale(word), "{word}");
        }
    }

    /// Port of Waypoint's `swapsLanguagesKeepingTheOtherTags`.
    #[test]
    fn swaps_languages_keeping_the_other_tags() {
        let german = Languages::new("ruRU", Some("deDE"));
        assert_eq!(
            replace(BATTLE_NET_TAGS, &german),
            "OSX adventure base EU? acct-CZE? geoip-NL? deDE speech?:OSX adventure base EU? acct-CZE? geoip-NL? ruRU text?"
        );
        // Speech and text sets in the other order, and a set without a locale.
        assert_eq!(
            replace("OSX EU? enUS text?:OSX EU? speech?", &Languages::new("frFR", Some("esES"))),
            "OSX EU? frFR text?:OSX EU? esES speech?"
        );
        // No speech and text sets: both, from the one there is.
        assert_eq!(replace("OSX EU enUS", &german), "OSX EU deDE speech?:OSX EU ruRU text?");
        // Odd strings come out as two clean sets too: only a text set, one
        // set naming both, two locales in a set.
        for odd in ["OSX EU enUS text?", "OSX EU enUS speech? text?", "OSX EU enUS frFR"] {
            let fixed = replace(odd, &german);
            assert_eq!(fixed, "OSX EU deDE speech?:OSX EU ruRU text?", "{odd}");
            assert_eq!((text(&fixed), speech(&fixed)), (Some("ruRU"), Some("deDE")));
        }
        assert_eq!(replace("", &german), "deDE speech?:ruRU text?");
    }

    /// The tag-string part of Waypoint's `installLanguagesComeFromTagsThenRecords`.
    #[test]
    fn changes_both_languages_of_an_install() {
        let changed = replace("OSX EU? deDE speech?:OSX EU? ruRU text?", &Languages::new("esES", Some("esMX")));
        assert_eq!(changed, "OSX EU? esMX speech?:OSX EU? esES text?");
        assert_eq!(Languages::new("frFR", None), Languages { text: "frFR".into(), speech: "frFR".into() });
    }
}
