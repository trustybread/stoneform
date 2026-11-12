//! The install manifest (`IN`): the loose files a game keeps in its folder
//! (the `.app` and friends), by path and content key.
//!
//! ```text
//! "IN"  version (u8)  hash size (u8)  tag count (u16 BE)  entry count (u32 BE)
//! tags: name, NUL, type (u16 BE), entry bitmask (⌈entries/8⌉ bytes, MSB first)
//! entries: path, NUL, content key, size (u32 BE)
//! ```

use crate::bytes::Reader;
use crate::tags::{self, Selection, Tag};
use crate::{ContentKey, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEntry {
    /// Relative to the flavor folder, with `/` separators (Windows-style
    /// `\` in a manifest is turned into `/`).
    pub path: String,
    pub content_key: ContentKey,
    pub size: u32,
}

/// A parsed install manifest. Small enough to hold whole (WoW's has a few
/// thousand entries); the tags borrow their masks from the manifest's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallManifest<'a> {
    pub version: u8,
    pub tags: Vec<Tag<'a>>,
    pub entries: Vec<InstallEntry>,
}

impl<'a> InstallManifest<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(data, "install manifest");
        if r.take(2)? != b"IN" {
            return Err(Error::malformed("install manifest header"));
        }
        let version = r.u8()?;
        let hash_size = r.u8()?;
        if hash_size != 16 {
            return Err(Error::unsupported(format!("install manifest with {hash_size}-byte keys")));
        }
        let tag_count = r.be(2)? as usize;
        let entry_count = r.be(4)? as usize;
        let tags = tags::read_tags(&mut r, tag_count, entry_count)?;

        // Each entry takes at least 21 bytes: don't trust the count further than that.
        let mut entries = Vec::with_capacity(entry_count.min(r.remaining() / 21));
        for _ in 0..entry_count {
            let path = String::from_utf8_lossy(r.c_str()?).replace('\\', "/");
            let content_key = ContentKey::new(r.array()?);
            let size = r.be(4)? as u32;
            entries.push(InstallEntry { path, content_key, size });
        }
        Ok(Self { version, tags, entries })
    }

    /// Which entries Battle.net installs for a tag string such as
    /// `OSX EU? enUS speech?:OSX EU? deDE text?`; see [`tags::select`].
    pub fn selection(&self, tag_string: &str) -> Selection {
        tags::select(tag_string, &self.tags, self.entries.len())
    }

    /// The entries [`InstallManifest::selection`] picks, in manifest order.
    pub fn select(&self, tag_string: &str) -> Vec<&InstallEntry> {
        self.selection(tag_string).indices().map(|index| &self.entries[index]).collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bytes::push_be;
    use crate::tags::tests::{FIXTURE, FIXTURE_PATHS, tag_mask};

    pub(crate) fn key(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    /// The tag section both manifests share.
    pub(crate) fn tag_data(tags: &[(&str, u16, &[usize])], count: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, entries) in tags {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            push_be(&mut out, u64::from(*kind), 2);
            out.extend(tag_mask(entries, count));
        }
        out
    }

    /// An install manifest whose entry i has key i and size `size(i)`.
    pub(crate) fn manifest(tags: &[(&str, u16, &[usize])], paths: &[&str], size: impl Fn(usize) -> u64) -> Vec<u8> {
        let mut out = b"IN\x01\x10".to_vec();
        push_be(&mut out, tags.len() as u64, 2);
        push_be(&mut out, paths.len() as u64, 4);
        out.extend(tag_data(tags, paths.len()));
        for (i, path) in paths.iter().enumerate() {
            out.extend_from_slice(path.as_bytes());
            out.push(0);
            out.extend(key(i as u8));
            push_be(&mut out, size(i), 4);
        }
        out
    }

    /// Port of Waypoint's `installManifestSelectsByTagGroups`.
    #[test]
    fn selects_by_tag_groups() {
        let paths = ["mac/base", "win/base", "mac/de", "mac/en", "mac/en-us-only"];
        let tags: [(&str, u16, &[usize]); 6] = [
            ("OSX", 1, &[0, 2, 3, 4]),
            ("Windows", 1, &[1]),
            ("EU", 2, &[0, 1, 2, 3]),
            ("US", 2, &[4]),
            ("deDE", 3, &[0, 1, 2]),
            ("enUS", 3, &[0, 1, 3, 4]),
        ];
        let data = manifest(&tags, &paths, |i| 100 + i as u64);
        let manifest = InstallManifest::parse(&data).unwrap();
        assert_eq!(manifest.entries.len(), 5);
        assert_eq!(
            manifest.entries[3],
            InstallEntry { path: "mac/en".into(), content_key: ContentKey::new(key(3)), size: 103 }
        );
        let paths = |tag_string| manifest.select(tag_string).into_iter().map(|e| e.path.as_str()).collect::<Vec<_>>();
        // Unknown words are ignored; each mentioned type must match.
        assert_eq!(paths("OSX EU? acct-CZE? enUS speech?:OSX EU? enUS text?"), ["mac/base", "mac/en"]);
        // A type that isn't mentioned (region here) doesn't filter.
        assert_eq!(paths("OSX deDE"), ["mac/base", "mac/de"]);
    }

    /// The install half of Waypoint's `tagSelectionThroughBothManifests`.
    #[test]
    fn selects_through_the_manifest() {
        let data = manifest(&FIXTURE, &FIXTURE_PATHS, |i| i as u64 + 1);
        let manifest = InstallManifest::parse(&data).unwrap();
        let selected: Vec<&str> =
            manifest.select("OSX EU? deDE speech?:OSX EU? enUS text?").into_iter().map(|e| e.path.as_str()).collect();
        assert_eq!(selected, ["base", "de/speech", "en/text"]);
    }

    #[test]
    fn normalizes_windows_paths_and_rejects_truncation() {
        let data = manifest(&[("OSX", 1, &[0])], &["Data\\Win64\\x.dll"], |_| 1);
        assert_eq!(InstallManifest::parse(&data).unwrap().entries[0].path, "Data/Win64/x.dll");
        assert!(matches!(InstallManifest::parse(&data[..data.len() - 1]), Err(Error::Malformed(_))));
        assert!(matches!(InstallManifest::parse(b"DL\x01\x10"), Err(Error::Malformed(_))));
    }
}
