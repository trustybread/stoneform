//! The download manifest (`DL`): every encoded file a game keeps in local
//! storage, with its size and download priority.
//!
//! ```text
//! "DL"  version (1–3)  key size (u8)  has checksum (u8)  entry count (u32 BE)  tag count (u16 BE)
//! v2+: flag byte count (u8)   v3: base priority (i8), 3 reserved bytes
//! entries: encoding key, size (u40 BE), priority (i8), [checksum (u32 BE)], flag bytes
//! tags: as in the install manifest, after the entries
//! ```
//!
//! WoW's lists about 3 million entries, far too many to hold as values: the
//! manifest reads them from its borrowed bytes on demand, and a
//! [`Selection`] says which ones a tag string wants.

use crate::bytes::{self, Reader};
use crate::tags::{self, Selection, Tag};
use crate::{EncodingKey, Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadEntry {
    pub encoding_key: EncodingKey,
    /// The encoded (BLTE) size.
    pub size: u64,
    /// Lower downloads first; games can start before low-priority data arrives.
    pub priority: i8,
    pub checksum: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct DownloadManifest<'a> {
    data: &'a [u8],
    version: u8,
    has_checksum: bool,
    entry_size: usize,
    entries_start: usize,
    count: usize,
    tags: Vec<Tag<'a>>,
}

impl<'a> DownloadManifest<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(data, "download manifest");
        if r.take(2)? != b"DL" {
            return Err(Error::malformed("download manifest header"));
        }
        let version = r.u8()?;
        if !(1..=3).contains(&version) {
            return Err(Error::unsupported(format!("download manifest v{version}")));
        }
        let key_size = r.u8()?;
        if key_size != 16 {
            return Err(Error::unsupported(format!("download manifest with {key_size}-byte keys")));
        }
        let has_checksum = r.u8()? != 0;
        let count = r.be(4)? as usize;
        let tag_count = r.be(2)? as usize;
        let flag_bytes = if version >= 2 { usize::from(r.u8()?) } else { 0 };
        if version >= 3 {
            r.skip(4)?; // base priority, reserved
        }
        let entry_size = 16 + 5 + 1 + if has_checksum { 4 } else { 0 } + flag_bytes;
        let entries_start = r.pos();
        let entries_size = count.checked_mul(entry_size).filter(|&size| size <= r.remaining());
        let Some(entries_size) = entries_size else {
            return Err(Error::malformed("download manifest entries (unexpected end)"));
        };
        r.skip(entries_size)?;
        let tags = tags::read_tags(&mut r, tag_count, count)?;
        Ok(Self { data, version, has_checksum, entry_size, entries_start, count, tags })
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn tags(&self) -> &[Tag<'a>] {
        &self.tags
    }

    /// Entry `index`, read from the manifest's bytes.
    pub fn get(&self, index: usize) -> Option<DownloadEntry> {
        if index >= self.count {
            return None;
        }
        let at = self.entries_start + index * self.entry_size;
        let record = &self.data[at..at + self.entry_size];
        let encoding_key = EncodingKey::from_slice(&record[..16])?;
        let checksum = self.has_checksum.then(|| bytes::be_uint(&record[22..26]) as u32);
        Some(DownloadEntry {
            encoding_key,
            size: bytes::be_uint(&record[16..21]),
            priority: record[21] as i8,
            checksum,
        })
    }

    /// Every entry in manifest order, read lazily.
    pub fn iter(&self) -> impl Iterator<Item = DownloadEntry> + '_ {
        (0..self.count).filter_map(|index| self.get(index))
    }

    /// Which entries Battle.net downloads for a tag string; see [`tags::select`].
    pub fn selection(&self, tag_string: &str) -> Selection {
        tags::select(tag_string, &self.tags, self.count)
    }

    /// The selected entries, in manifest order, read lazily.
    pub fn selected<'s>(&'s self, selection: &'s Selection) -> impl Iterator<Item = DownloadEntry> + 's {
        selection.indices().filter_map(|index| self.get(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::push_be;
    use crate::install::tests::{key, tag_data};
    use crate::install::{self, InstallManifest};
    use crate::tags::tests::{FIXTURE, FIXTURE_PATHS};

    /// A v1 download manifest whose entry i has key i and size `size(i)`.
    fn manifest(tags: &[(&str, u16, &[usize])], count: usize, size: impl Fn(usize) -> u64) -> Vec<u8> {
        let mut out = b"DL\x01\x10\x00".to_vec();
        push_be(&mut out, count as u64, 4);
        push_be(&mut out, tags.len() as u64, 2);
        for i in 0..count {
            out.extend(key(i as u8));
            push_be(&mut out, size(i), 5);
            out.push(0);
        }
        out.extend(tag_data(tags, count));
        out
    }

    fn sizes(manifest: &DownloadManifest<'_>, tag_string: &str) -> Vec<u64> {
        let selection = manifest.selection(tag_string);
        manifest.selected(&selection).map(|entry| entry.size).collect()
    }

    /// Port of Waypoint's `parsesDownloadManifestAndSelectsByTags`, every version.
    #[test]
    fn parses_every_version_and_selects_by_tags() {
        for version in 1..=3u8 {
            let mut d = vec![b'D', b'L', version, 16, 1];
            push_be(&mut d, 3, 4);
            push_be(&mut d, 2, 2);
            if version >= 2 {
                d.push(1);
            }
            if version >= 3 {
                d.extend([0, 0, 0, 0]);
            }
            for i in 1..=3u8 {
                d.extend([i; 16]);
                push_be(&mut d, u64::from(i) * 100, 5);
                d.push(i - 1);
                push_be(&mut d, 0xABCD, 4);
                if version >= 2 {
                    d.push(0);
                }
            }
            d.extend(b"OSX\0\x00\x01\xA0"); // entries 0, 2
            d.extend(b"enUS\0\x00\x03\x60"); // entries 1, 2

            let m = DownloadManifest::parse(&d).unwrap();
            assert_eq!(m.len(), 3);
            assert_eq!(
                m.get(1),
                Some(DownloadEntry {
                    encoding_key: EncodingKey::new([2; 16]),
                    size: 200,
                    priority: 1,
                    checksum: Some(0xABCD)
                })
            );
            assert_eq!(m.get(3), None);
            assert_eq!(sizes(&m, "OSX enUS speech?"), [300]);
            assert_eq!(sizes(&m, "OSX"), [100, 300]);
            assert_eq!(m.iter().count(), 3);
        }
    }

    /// The download half of Waypoint's `tagSelectionThroughBothManifests`.
    #[test]
    fn selects_through_the_manifest() {
        let data = manifest(&FIXTURE, FIXTURE_PATHS.len(), |i| i as u64);
        let m = DownloadManifest::parse(&data).unwrap();
        assert_eq!(sizes(&m, "OSX EU? deDE speech?:OSX EU? enUS text?"), [0, 3, 6]);
    }

    /// Port of Waypoint's
    /// `tagSelectionExcludesUnselectedOptionalPackagesInBothManifests`: WoW's
    /// ordinary files plus its HighRes and Alternate packages.
    #[test]
    fn excludes_unselected_optional_packages_in_both_manifests() {
        let paths = ["base", "hires", "alternate", "both", "windows"];
        let tags: [(&str, u16, &[usize]); 4] = [
            ("OSX", 1, &[0, 1, 2, 3]),
            ("Windows", 1, &[4]),
            ("HighRes", 0x4000, &[1, 3]),
            ("Alternate", 0x4000, &[2, 3]),
        ];
        let install_data = install::tests::manifest(&tags, &paths, |i| i as u64 + 1);
        let download_data = manifest(&tags, paths.len(), |i| i as u64 + 1);
        let im = InstallManifest::parse(&install_data).unwrap();
        let dm = DownloadManifest::parse(&download_data).unwrap();
        let install_paths = |query: &str| im.select(query).into_iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        let cases: [(&str, &[usize]); 7] = [
            ("OSX", &[0]),
            ("OSX HighRes?", &[0, 1]),
            ("OSX Alternate", &[0, 2]),
            ("OSX HighRes Alternate", &[0, 1, 2, 3]),
            ("Windows HighRes", &[4]),
            ("OSX HighRes:OSX Alternate", &[0, 1, 2]),
            ("", &[0, 1, 2, 3, 4]),
        ];
        for (query, expected) in cases {
            assert_eq!(install_paths(query), expected.iter().map(|&i| paths[i]).collect::<Vec<_>>(), "{query}");
            assert_eq!(sizes(&dm, query), expected.iter().map(|&i| i as u64 + 1).collect::<Vec<_>>(), "{query}");
        }
        // A full-quality WoW install made from scratch opts into HighRes.
        let full_quality = tags::with_high_res("OSX enUS speech?:OSX enUS text?");
        assert_eq!(install_paths(&full_quality), ["base", "hires"]);
        assert_eq!(sizes(&dm, &full_quality), [1, 2]);
        assert_eq!(tags::with_high_res(&full_quality), full_quality);
        let anniversary = tags::with_high_res("OSX Alternate");
        assert_eq!(install_paths(&anniversary), ["base", "hires", "alternate", "both"]);
        // An installation that already exists keeps its own selection.
        assert_eq!(install_paths("OSX enUS speech?:OSX enUS text?"), ["base"]);
        assert_eq!(sizes(&dm, "OSX enUS speech?:OSX enUS text?"), [1]);
    }

    #[test]
    fn rejects_malformed_manifests() {
        let data = manifest(&FIXTURE, FIXTURE_PATHS.len(), |i| i as u64);
        assert!(matches!(DownloadManifest::parse(&data[..40]), Err(Error::Malformed(_))));
        assert!(matches!(DownloadManifest::parse(&data[..data.len() - 1]), Err(Error::Malformed(_))));
        let mut v4 = data.clone();
        v4[2] = 4;
        assert!(matches!(DownloadManifest::parse(&v4), Err(Error::Unsupported(_))));
        // An entry count that can't fit is caught before any entry is read.
        let mut huge = data;
        huge[5..9].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(DownloadManifest::parse(&huge), Err(Error::Malformed(_))));
    }
}
