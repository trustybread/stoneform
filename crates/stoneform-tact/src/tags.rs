//! Manifest tags and the rule that picks entries by a Battle.net tag string.
//!
//! Install and download manifests tag their entries: every tag has a name
//! (`OSX`, `EU`, `enUS`, `speech`), a type (platform, region, locale,
//! content…) and a bitmask with one bit per entry, most significant bit
//! first. An install records the tags it wants as a string such as
//! `OSX x86_64 arm64 EU? deDE speech?:OSX x86_64 arm64 EU? enUS text?`;
//! [`select`] turns that into the entries to fetch.

use std::collections::{BTreeMap, HashSet};

use crate::Result;
use crate::bytes::Reader;

/// The type bit of option tags (`code`, `Alternate`, `HighRes`): packages
/// that are opted into rather than selected by platform or language.
pub const OPTION_TAG: u16 = 0x4000;

/// WoW's optional high-resolution package.
pub const HIGH_RES: &str = "HighRes";

/// One manifest tag. The mask borrows the manifest's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag<'a> {
    pub name: String,
    pub kind: u16,
    mask: &'a [u8],
}

impl<'a> Tag<'a> {
    /// A tag whose `mask` has bit i (MSB first) set when entry i carries it.
    pub fn new(name: impl Into<String>, kind: u16, mask: &'a [u8]) -> Self {
        Self { name: name.into(), kind, mask }
    }

    pub fn mask(&self) -> &'a [u8] {
        self.mask
    }

    /// Whether entry `index` carries the tag.
    pub fn contains(&self, index: usize) -> bool {
        self.mask.get(index / 8).is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0)
    }

    pub fn is_option(&self) -> bool {
        self.kind & OPTION_TAG != 0
    }
}

/// Reads `count` tags (name, NUL, type u16 BE, mask) for `entry_count` entries.
pub(crate) fn read_tags<'a>(r: &mut Reader<'a>, count: usize, entry_count: usize) -> Result<Vec<Tag<'a>>> {
    let mask_size = entry_count.div_ceil(8);
    let mut tags = Vec::with_capacity(count.min(r.remaining()));
    for _ in 0..count {
        let name = String::from_utf8_lossy(r.c_str()?).into_owned();
        let kind = r.be(2)? as u16;
        tags.push(Tag::new(name, kind, r.take(mask_size)?));
    }
    Ok(tags)
}

/// The entries a tag string selects, as a bitmap: ⌈entries/8⌉ bytes
/// however many entries there are (WoW's download manifest has millions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    mask: Vec<u8>,
    entry_count: usize,
}

impl Selection {
    pub fn contains(&self, index: usize) -> bool {
        index < self.entry_count && self.mask[index / 8] & (0x80 >> (index % 8)) != 0
    }

    /// The selected entries' indices, in manifest order.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.entry_count).filter(|&index| self.contains(index))
    }

    /// How many entries are selected.
    pub fn count(&self) -> usize {
        self.mask.iter().map(|byte| byte.count_ones() as usize).sum()
    }

    /// How many entries the manifest has.
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// Bit i (MSB first) set: entry i is selected.
    pub fn as_bytes(&self) -> &[u8] {
        &self.mask
    }
}

/// The entries Battle.net installs for a tag string.
///
/// The string holds `:`-separated tag sets, e.g.
/// `OSX EU? enUS speech?:OSX EU? deDE text?`: the first set picks the
/// speech-language files, the second the text-language files. An entry is
/// selected when any set selects it.
///
/// Within a set, the manifest's tags named in it are grouped by type
/// (platform, region, locale, content…). The set selects an entry when, for
/// every such type, the entry carries at least one of its named tags. Types
/// the set names no tag of don't filter. Words the manifest has no tag for
/// are ignored; a trailing `?` marks a tag as optional, which amounts to the
/// same thing.
///
/// Option tags ([`OPTION_TAG`]: `code`, `Alternate`, `HighRes`) opt in extra
/// files. Naming one never narrows the set to its files (they don't filter
/// like the other types), but files marked with an option the set doesn't
/// name are left out, unless the set is empty: an empty string asks for
/// everything. So an ordinary WoW install doesn't fetch the optional
/// high-resolution package the Agent would discard at its next cleanup.
pub fn select(tag_string: &str, tags: &[Tag<'_>], entry_count: usize) -> Selection {
    let byte_count = entry_count.div_ceil(8);
    let mut sets: Vec<&str> = tag_string.split(':').filter(|set| !set.is_empty()).collect();
    if sets.is_empty() {
        sets.push(""); // no tags at all: everything
    }
    let mut result = vec![0u8; byte_count];
    for set in sets {
        let names: HashSet<&str> = set.split_whitespace().map(|word| word.strip_suffix('?').unwrap_or(word)).collect();
        let mut selected = vec![0xFFu8; byte_count];

        let mut by_kind: BTreeMap<u16, Vec<&Tag<'_>>> = BTreeMap::new();
        for tag in tags.iter().filter(|tag| !tag.is_option() && names.contains(tag.name.as_str())) {
            by_kind.entry(tag.kind).or_default().push(tag);
        }
        for group in by_kind.values() {
            let mut any = vec![0u8; byte_count];
            for tag in group {
                for (out, byte) in any.iter_mut().zip(tag.mask) {
                    *out |= byte;
                }
            }
            for (out, byte) in selected.iter_mut().zip(&any) {
                *out &= byte;
            }
        }
        if !names.is_empty() {
            for tag in tags.iter().filter(|tag| tag.is_option() && !names.contains(tag.name.as_str())) {
                for (out, byte) in selected.iter_mut().zip(tag.mask) {
                    *out &= !byte;
                }
            }
        }
        for (out, byte) in result.iter_mut().zip(&selected) {
            *out |= byte;
        }
    }
    if entry_count % 8 != 0 {
        result[byte_count - 1] &= 0xFF << (8 - entry_count % 8);
    }
    Selection { mask: result, entry_count }
}

/// `tag_string` with WoW's [`HIGH_RES`] package selected in every set: a
/// full-quality install. Only for WoW, and only for an install made from
/// scratch: an existing install keeps the content it was installed with,
/// since Battle.net leaves HighRes out of some installs on purpose and its
/// cleanup would discard it again.
pub fn with_high_res(tag_string: &str) -> String {
    let sets: Vec<String> = tag_string
        .split(':')
        .map(|set| match (set_selects_high_res(set), set.is_empty()) {
            (true, _) => set.to_owned(),
            (false, true) => HIGH_RES.to_owned(),
            (false, false) => format!("{set} {HIGH_RES}"),
        })
        .collect();
    sets.join(":")
}

/// Whether every set of `tag_string` selects [`HIGH_RES`]: what the Agent's
/// `UserSettings.additional_tags` would claim of an install. An empty
/// string, or HighRes in only some sets, is not that.
pub fn selects_high_res(tag_string: &str) -> bool {
    !tag_string.is_empty() && tag_string.split(':').all(set_selects_high_res)
}

fn set_selects_high_res(set: &str) -> bool {
    set.split_whitespace().any(|word| word == HIGH_RES || word.strip_suffix('?') == Some(HIGH_RES))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Tag mask bits, MSB first, for entries `0..count`.
    pub(crate) fn tag_mask(entries: &[usize], count: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; count.div_ceil(8)];
        for &i in entries {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
        bytes
    }

    /// A small game tagged the way Blizzard's manifests are: shared files
    /// carry every locale and content tag, language files one of each.
    pub(crate) const FIXTURE_PATHS: [&str; 7] =
        ["base", "win/base", "us-only", "de/speech", "de/text", "en/speech", "en/text"];
    pub(crate) const FIXTURE: [(&str, u16, &[usize]); 8] = [
        ("OSX", 1, &[0, 2, 3, 4, 5, 6]),
        ("Windows", 1, &[1]),
        ("EU", 2, &[0, 1, 3, 4, 5, 6]),
        ("US", 2, &[0, 1, 2, 3, 4, 5, 6]),
        ("deDE", 3, &[0, 1, 3, 4]),
        ("enUS", 3, &[0, 1, 2, 5, 6]),
        ("speech", 4, &[0, 1, 2, 3, 5]),
        ("text", 4, &[0, 1, 2, 4, 6]),
    ];

    fn select_from(fixture: &[(&str, u16, &[usize])], tag_string: &str) -> Vec<&'static str> {
        let count = FIXTURE_PATHS.len();
        let masks: Vec<Vec<u8>> = fixture.iter().map(|(_, _, entries)| tag_mask(entries, count)).collect();
        let tags: Vec<Tag<'_>> =
            fixture.iter().zip(&masks).map(|((name, kind, _), mask)| Tag::new(*name, *kind, mask)).collect();
        select(tag_string, &tags, count).indices().map(|i| FIXTURE_PATHS[i]).collect()
    }

    fn select_fixture(tag_string: &str) -> Vec<&'static str> {
        select_from(&FIXTURE, tag_string)
    }

    #[test]
    fn single_set() {
        assert_eq!(select_fixture("OSX EU enUS speech"), ["base", "en/speech"]);
        assert_eq!(select_fixture("Windows deDE"), ["win/base"]);
    }

    #[test]
    fn identical_sets_match_one_set() {
        assert_eq!(select_fixture("OSX EU enUS:OSX EU enUS"), select_fixture("OSX EU enUS"));
        // Same language for speech and text: what merging both sets gave before.
        assert_eq!(select_fixture("OSX EU? enUS speech?:OSX EU? enUS text?"), ["base", "en/speech", "en/text"]);
    }

    #[test]
    fn mixed_speech_and_text_languages() {
        // German speech, English text: no English speech, no German text.
        assert_eq!(select_fixture("OSX EU? deDE speech?:OSX EU? enUS text?"), ["base", "de/speech", "en/text"]);
        // Manifest order is kept whichever set comes first; shared files once.
        assert_eq!(select_fixture("OSX EU? enUS text?:OSX EU? deDE speech?"), ["base", "de/speech", "en/text"]);
    }

    #[test]
    fn ignores_optional_tags_the_manifest_lacks() {
        // acct-CZE and geoip-NL aren't in the manifest; EU is, and filters.
        let tag_string = "OSX EU? acct-CZE? geoip-NL? enUS speech?:OSX EU? acct-CZE? geoip-NL? deDE text?";
        assert_eq!(select_fixture(tag_string), ["base", "de/text", "en/speech"]);
        // KR isn't in the manifest either, so region doesn't filter.
        assert_eq!(select_fixture("OSX KR? enUS speech"), ["base", "us-only", "en/speech"]);
    }

    #[test]
    fn types_without_named_tags_dont_filter() {
        // No region named: the US-only file stays in.
        assert_eq!(select_fixture("OSX enUS speech"), ["base", "us-only", "en/speech"]);
        // No content type named: speech and text both.
        assert_eq!(select_fixture("OSX EU deDE"), ["base", "de/speech", "de/text"]);
        assert_eq!(select_fixture(""), FIXTURE_PATHS);
    }

    #[test]
    fn selected_options_keep_base_files() {
        // HotS tags its code files `code` (type 0x4000) and installs as
        // `OSX code …`; that must not narrow the install to its code files.
        let mut fixture = FIXTURE.to_vec();
        fixture.push(("code", 0x4000, &[1]));
        assert_eq!(
            select_from(&fixture, "OSX code EU? enUS speech?:OSX code EU? enUS text?"),
            ["base", "en/speech", "en/text"]
        );
        assert_eq!(select_from(&fixture, "Windows code"), ["win/base"]);
    }

    #[test]
    fn selection_bitmap_has_no_bits_past_the_entries() {
        let mask = [0xFF];
        let tags = [Tag::new("OSX", 1, &mask)];
        let selection = select("", &tags, 3);
        assert_eq!(selection.as_bytes(), [0b1110_0000]);
        assert_eq!((selection.count(), selection.entry_count()), (3, 3));
        assert!(!selection.contains(3));
        assert!(tags[0].contains(7) && !tags[0].contains(8));
    }

    /// The tag-string half of Waypoint's
    /// `tagSelectionExcludesUnselectedOptionalPackagesInBothManifests` and
    /// `highResSelectionNeedsEveryTagSetOfAWoWInstall`.
    #[test]
    fn high_res_goes_into_every_set() {
        let full = with_high_res("OSX enUS speech?:OSX enUS text?");
        assert_eq!(full, "OSX enUS speech? HighRes:OSX enUS text? HighRes");
        assert_eq!(with_high_res(&full), full);
        assert_eq!(with_high_res("OSX HighRes?"), "OSX HighRes?");
        assert_eq!(with_high_res(""), "HighRes");

        assert!(selects_high_res("OSX HighRes enUS speech?:OSX HighRes? ruRU text?"));
        assert!(selects_high_res("OSX HighRes"));
        assert!(!selects_high_res("OSX HighRes enUS speech?:OSX ruRU text?"));
        assert!(!selects_high_res("OSX HighResolution enUS text?"));
        assert!(!selects_high_res(""));
    }
}
