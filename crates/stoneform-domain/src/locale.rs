use std::fmt;

/// A game language as Blizzard writes it: `enUS`, `deDE`, `zhTW`, …
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Locale(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a game language; expected two lower-case and two upper-case letters, like enUS")]
pub struct ParseLocaleError(String);

impl Locale {
    pub fn parse(value: &str) -> Result<Self, ParseLocaleError> {
        let bytes = value.as_bytes();
        let valid = bytes.len() == 4
            && bytes[..2].iter().all(u8::is_ascii_lowercase)
            && bytes[2..].iter().all(u8::is_ascii_uppercase);
        if valid { Ok(Self(value.to_owned())) } else { Err(ParseLocaleError(value.to_owned())) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The language's own name, as Battle.net lists it.
    pub fn display_name(&self) -> &str {
        match self.0.as_str() {
            "enUS" => "English (US)",
            "enGB" => "English (EU)",
            "deDE" => "Deutsch",
            "esES" => "Español (EU)",
            "esMX" => "Español (AL)",
            "frFR" => "Français",
            "itIT" => "Italiano",
            "ruRU" => "Русский",
            "koKR" => "한국어",
            "ptBR" => "Português (BR)",
            "zhTW" => "繁體中文",
            "zhCN" => "简体中文",
            other => other,
        }
    }

    /// The best of `available` for a Mac whose preferred languages, most
    /// preferred first, are the BCP 47 tags in `preferred` (`de-DE`,
    /// `zh-Hant-TW`, `es-419`). Falls back to `enUS`, then to the first one.
    pub fn best_match(preferred: &[&str], available: &[Locale]) -> Option<Locale> {
        let offered = |code: &str| available.iter().find(|locale| locale.0 == code).cloned();
        for tag in preferred {
            let parts: Vec<&str> = tag.split(['-', '_']).collect();
            let language = parts[0].to_ascii_lowercase();
            let region =
                parts.iter().skip(1).find(|part| part.len() == 2 || part.len() == 3).map(|r| r.to_ascii_uppercase());
            let script = parts.iter().skip(1).find(|part| part.len() == 4).map(|s| s.to_ascii_lowercase());
            let wanted = match (language.as_str(), script.as_deref(), region.as_deref()) {
                ("zh", Some("hant"), _) | ("zh", None, Some("TW" | "HK" | "MO")) => vec!["zhTW".to_owned()],
                ("zh", _, _) => vec!["zhCN".to_owned()],
                // Latin American Spanish is esMX; Spain's is esES.
                ("es", _, Some(region)) if region != "ES" => vec!["esMX".to_owned(), "esES".to_owned()],
                (language, _, Some(region)) => vec![format!("{language}{region}")],
                _ => vec![],
            };
            if let Some(locale) = wanted.iter().find_map(|code| offered(code)) {
                return Some(locale);
            }
            // Same language, another region: de-AT plays in deDE, pt-PT in ptBR.
            if let Some(locale) = available.iter().find(|locale| locale.0.starts_with(&language)) {
                return Some(locale.clone());
            }
        }
        offered("enUS").or_else(|| available.first().cloned())
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Locale({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locales(codes: &[&str]) -> Vec<Locale> {
        codes.iter().map(|code| Locale::parse(code).unwrap()).collect()
    }

    const FOREVER: &[&str] = &["enUS", "deDE", "esES", "esMX", "frFR", "ruRU", "koKR", "ptBR", "zhTW", "zhCN"];

    fn best(preferred: &[&str]) -> String {
        Locale::best_match(preferred, &locales(FOREVER)).unwrap().to_string()
    }

    #[test]
    fn parses_blizzard_locale_codes() {
        assert!(Locale::parse("enUS").is_ok());
        assert!(Locale::parse("en-US").is_err());
        assert!(Locale::parse("ENUS").is_err());
        assert!(Locale::parse("enU").is_err());
    }

    #[test]
    fn picks_the_mac_s_language() {
        assert_eq!(best(&["de-DE"]), "deDE");
        assert_eq!(best(&["de-AT"]), "deDE");
        assert_eq!(best(&["fr-CA", "en-US"]), "frFR");
        assert_eq!(best(&["en-GB"]), "enUS");
        assert_eq!(best(&["pt-PT"]), "ptBR");
        assert_eq!(best(&["ko-KR"]), "koKR");
    }

    #[test]
    fn tells_the_spanish_and_chinese_variants_apart() {
        assert_eq!(best(&["es-ES"]), "esES");
        assert_eq!(best(&["es-419"]), "esMX");
        assert_eq!(best(&["es-MX"]), "esMX");
        assert_eq!(best(&["es"]), "esES");
        assert_eq!(best(&["zh-Hant-TW"]), "zhTW");
        assert_eq!(best(&["zh-HK"]), "zhTW");
        assert_eq!(best(&["zh-Hans-CN"]), "zhCN");
    }

    #[test]
    fn falls_back_to_english() {
        assert_eq!(best(&["it-IT", "nl-NL"]), "enUS");
        assert_eq!(best(&[]), "enUS");
        assert_eq!(Locale::best_match(&["it-IT"], &locales(&["deDE"])).unwrap().to_string(), "deDE");
        assert_eq!(Locale::best_match(&["it-IT"], &[]), None);
    }
}
