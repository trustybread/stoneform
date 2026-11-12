//! Local storage's `.idx` files (version 7): one per bucket, mapping
//! truncated keys to where their entries sit.
//!
//! ```text
//! 0x00  header size (u32 LE, 16)  hashlittle of the header (u32 LE)
//! 0x08  header: version 7 (u16 LE), bucket, 0, size bytes 4, offset bytes 5,
//!       key bytes 9, offset bits 30, max file size 0x3FF << 30 (u64 LE)
//!       padding to 16 bytes
//! 0x20  sorted entries' size (u32 LE)  their chained hashlittle2 (u32 LE)
//! 0x28  sorted entries: key (9), storage offset (u40 BE), size (u32 LE)
//!       the update section, at the next 4 KiB boundary: 24-byte records in
//!       512-byte pages, which the game and the Agent append before merging
//!       zero padding to a whole number of 64 KiB
//! ```

use super::StorageLocation;
use crate::bytes::{self, le_u32_at, put_le32};
use crate::{Error, Result, TruncatedKey, lookup3};

/// The update section starts at the sorted entries' end, rounded up to this.
pub const UPDATE_SECTION_ALIGNMENT: usize = 0x1000;

/// Bytes of update section the game insists on from that point. With less it
/// refuses the storage ("Truncated KMT update section detected", then
/// NeedsRepair) however sound the entries are.
pub const UPDATE_SECTION_RESERVE: usize = 0x7800;

/// Index files are a whole number of these.
pub const INDEX_FILE_ALIGNMENT: usize = 0x10000;

const VERSION: u16 = 7;
const HEADER_SIZE: usize = 16;
const ENTRIES_START: usize = 0x28;
const RECORD_SIZE: usize = 18;
const UPDATE_RECORD_SIZE: usize = 24;
const UPDATE_PAGE_SIZE: usize = 0x200;
/// The largest storage the header admits: 0x3FF archives of 1 GiB.
const MAX_FILE_SIZE: u64 = (super::MAX_ARCHIVES as u64) << super::OFFSET_BITS;

/// One entry: a truncated key, where its header and blob start, and their
/// size together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexEntry {
    pub key: TruncatedKey,
    pub location: StorageLocation,
    /// Entry header plus blob.
    pub size: u32,
}

impl IndexEntry {
    /// The 18-byte record of the sorted section.
    fn record(&self) -> [u8; RECORD_SIZE] {
        let mut record = [0u8; RECORD_SIZE];
        record[..9].copy_from_slice(self.key.as_bytes());
        bytes::put_be(&mut record, 9, self.location.packed(), 5);
        put_le32(&mut record, 14, self.size);
        record
    }

    fn from_record(record: &[u8]) -> Option<Self> {
        Some(Self {
            key: TruncatedKey::from_slice(&record[..9])?,
            location: StorageLocation::from_packed(bytes::be_uint(&record[9..14]))?,
            size: le_u32_at(record, 14),
        })
    }
}

/// `XXYYYYYYYY.idx`: bucket and version in lowercase hex.
pub fn index_file_name(bucket: u8, version: u32) -> String {
    format!("{bucket:02x}{version:08x}.idx")
}

/// The bucket and version of an index file name.
pub fn parse_index_file_name(name: &str) -> Option<(u8, u32)> {
    let stem = name.strip_suffix(".idx").filter(|stem| stem.len() == 10 && stem.is_ascii())?;
    let bucket = u8::from_str_radix(&stem[..2], 16).ok().filter(|&bucket| bucket < 16)?;
    let version = u32::from_str_radix(&stem[2..], 16).ok()?;
    Some((bucket, version))
}

/// How big an index file whose sorted entries end at `sorted_end` must be:
/// the game looks for its update section at the next 4 KiB boundary and
/// wants [`UPDATE_SECTION_RESERVE`] bytes of it, and the file is a whole
/// number of 64 KiB blocks, as the Agent writes it.
///
/// Measuring the reserve from the entries' end instead of the 4 KiB boundary
/// left as little as 0x7000 past it (WoW Forever, 121,968 entries in a
/// bucket: Waypoint issue #3), which the game reads as a truncated update
/// section and refuses to start.
pub fn index_file_size(sorted_end: usize) -> usize {
    let update_start = sorted_end.next_multiple_of(UPDATE_SECTION_ALIGNMENT);
    (update_start + UPDATE_SECTION_RESERVE).next_multiple_of(INDEX_FILE_ALIGNMENT)
}

/// One bucket's index file with `entries`, sorted by key then position, and
/// an empty update section of the size the game wants.
pub fn encode_index_file(bucket: u8, entries: &[IndexEntry]) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_SIZE);
    header.extend(VERSION.to_le_bytes());
    header.extend([bucket, 0, 4, 5, 9, super::OFFSET_BITS as u8]);
    header.extend(MAX_FILE_SIZE.to_le_bytes());

    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.key.cmp(&b.key).then(a.location.packed().cmp(&b.location.packed())));
    let sorted_end = ENTRIES_START + RECORD_SIZE * sorted.len();
    let mut out = Vec::with_capacity(index_file_size(sorted_end));
    out.extend((HEADER_SIZE as u32).to_le_bytes());
    out.extend(lookup3::hashlittle(&header, 0).to_le_bytes());
    out.extend(header);
    out.resize(out.len().next_multiple_of(16), 0);

    let mut body = Vec::with_capacity(RECORD_SIZE * sorted.len());
    let (mut pc, mut pb) = (0, 0);
    for entry in &sorted {
        let record = entry.record();
        (pc, pb) = lookup3::hashlittle2(&record, pc, pb);
        body.extend(record);
    }
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(pc.to_le_bytes());
    out.extend(body);
    out.resize(index_file_size(out.len()), 0);
    out
}

/// A change the update section records: status 0 is stored; anything else
/// is gone or not resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexUpdate {
    pub entry: IndexEntry,
    pub status: u8,
}

/// One update-section record: a guard (`hashlittle` of the next 19 bytes,
/// top bit set), the entry as in the sorted section, the status, a pad byte.
pub fn encode_update_record(entry: &IndexEntry, status: u8) -> [u8; UPDATE_RECORD_SIZE] {
    let mut record = [0u8; UPDATE_RECORD_SIZE];
    record[4..22].copy_from_slice(&entry.record());
    record[22] = status;
    let guard = lookup3::hashlittle(&record[4..23], 0) | 0x8000_0000;
    put_le32(&mut record, 0, guard);
    record
}

/// A parsed index file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFile {
    pub bucket: u8,
    /// The sorted section, in file order.
    pub entries: Vec<IndexEntry>,
    /// The update section's records, oldest first.
    pub updates: Vec<IndexUpdate>,
    /// Where the sorted entries end: the update section starts at the next
    /// 4 KiB boundary from here.
    pub sorted_end: usize,
}

impl IndexFile {
    /// Reads a v7 index file, checking its lookup3 hashes. Update records
    /// whose guard doesn't match are skipped, as the game skips them.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < ENTRIES_START {
            return Err(Error::malformed("index file (unexpected end)"));
        }
        let header_size = le_u32_at(data, 0) as usize;
        let header = data.get(8..8 + header_size).filter(|_| header_size >= HEADER_SIZE);
        if header.is_none_or(|header| lookup3::hashlittle(header, 0) != le_u32_at(data, 4)) {
            return Err(Error::malformed("index file header"));
        }
        let version = u16::from_le_bytes([data[8], data[9]]);
        if version != VERSION || data[0x0C..0x10] != [4, 5, 9, super::OFFSET_BITS as u8] {
            return Err(Error::unsupported(format!(
                "index file version {version} (field sizes {}/{}/{}/{})",
                data[0x0C], data[0x0D], data[0x0E], data[0x0F]
            )));
        }
        let bucket = data[0x0A];

        let block_start = (8 + header_size).next_multiple_of(16);
        if block_start + 8 > data.len() {
            return Err(Error::malformed("index file (unexpected end)"));
        }
        let block_size = le_u32_at(data, block_start) as usize;
        let entries_start = block_start + 8;
        let sorted_end = entries_start + block_size;
        if block_size % RECORD_SIZE != 0 || sorted_end > data.len() {
            return Err(Error::malformed("index file entries"));
        }
        let mut entries = Vec::with_capacity(block_size / RECORD_SIZE);
        let (mut pc, mut pb) = (0, 0);
        for record in data[entries_start..sorted_end].chunks_exact(RECORD_SIZE) {
            (pc, pb) = lookup3::hashlittle2(record, pc, pb);
            entries.push(IndexEntry::from_record(record).ok_or_else(|| Error::malformed("index file entry"))?);
        }
        if pc != le_u32_at(data, block_start + 4) {
            return Err(Error::ChecksumMismatch(format!("index file for bucket {bucket}")));
        }

        let mut updates = Vec::new();
        let mut page = sorted_end.next_multiple_of(UPDATE_PAGE_SIZE);
        while page + UPDATE_PAGE_SIZE <= data.len() {
            for record in data[page..page + UPDATE_PAGE_SIZE].chunks_exact(UPDATE_RECORD_SIZE) {
                let guard = le_u32_at(record, 0);
                if guard == 0 {
                    break;
                }
                if guard & 0x8000_0000 == 0 || lookup3::hashlittle(&record[4..23], 0) | 0x8000_0000 != guard {
                    continue;
                }
                if let Some(entry) = IndexEntry::from_record(&record[4..22]) {
                    updates.push(IndexUpdate { entry, status: record[22] });
                }
            }
            page += UPDATE_PAGE_SIZE;
        }
        Ok(Self { bucket, entries, updates, sorted_end })
    }

    /// The bucket's entries with the update section applied: for each key
    /// the update section names, its last record wins, and stays only with
    /// status 0. Unchanged entries keep their order; updated ones follow.
    pub fn current_entries(&self) -> Vec<IndexEntry> {
        use std::collections::HashMap;
        let mut last_update: HashMap<TruncatedKey, usize> = HashMap::new();
        for (position, update) in self.updates.iter().enumerate() {
            last_update.insert(update.entry.key, position);
        }
        let unchanged = self.entries.iter().filter(|entry| !last_update.contains_key(&entry.key)).copied();
        let updated = self
            .updates
            .iter()
            .enumerate()
            .filter(|(position, update)| update.status == 0 && last_update.get(&update.entry.key) == Some(position))
            .map(|(_, update)| update.entry);
        unchanged.chain(updated).collect()
    }
}

/// Whether an index file leaves the game its update section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveCheck {
    /// Long enough already, as the Agent and [`encode_index_file`] write it.
    Sufficient,
    /// Too short, with nothing past the sorted entries: extending it with
    /// zeros to `len` bytes (an empty update section) repairs it, leaving the
    /// header, entries and checksums as they are.
    Extend { len: usize },
    /// Too short, and the update section already holds records: leave it to
    /// the game's own repair.
    InUse,
}

/// Measures an index file for the repair of Waypoint issue #3 (an older
/// Waypoint wrote them too short; the game then demanded a repair with every
/// file present). Fails for a file this crate can't read, which isn't ours
/// to pad.
pub fn check_update_reserve(data: &[u8]) -> Result<ReserveCheck> {
    let parsed = IndexFile::parse(data)?;
    let needed = index_file_size(parsed.sorted_end);
    Ok(if data.len() >= needed {
        ReserveCheck::Sufficient
    } else if data[parsed.sorted_end..].iter().all(|&byte| byte == 0) {
        ReserveCheck::Extend { len: needed }
    } else {
        ReserveCheck::InUse
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::casc::SEGMENT_HEADERS_SIZE;

    fn location(archive: u16, offset: u64) -> StorageLocation {
        StorageLocation::new(archive, offset).unwrap()
    }

    fn entry(key: [u8; 9], offset: u64, size: u32) -> IndexEntry {
        IndexEntry { key: TruncatedKey::new(key), location: location(0, offset), size }
    }

    /// `count` distinct entries for one index file.
    fn index_entries(count: usize) -> Vec<IndexEntry> {
        (0..count)
            .map(|i| {
                let mut key = [0u8; 9];
                key[..4].copy_from_slice(&(i as u32).to_le_bytes());
                key[8] = 0xA5;
                entry(key, (SEGMENT_HEADERS_SIZE + i * 0x40) as u64, 0x40)
            })
            .collect()
    }

    /// Waypoint 0.9.5's rule: the reserve measured from the entries' end, not
    /// from the 4 KiB boundary the game starts its update section at.
    fn short_index_file_size(sorted_end: usize) -> usize {
        (sorted_end + 0x7800).next_multiple_of(0x10000)
    }

    fn update_section_room(size: usize, sorted_end: usize) -> usize {
        size - sorted_end.next_multiple_of(0x1000)
    }

    /// Port of `indexFileSizeLeavesTheGameItsUpdateSection`: 0x7800 bytes
    /// from the 4 KiB boundary after the sorted entries. 121,968 entries in
    /// one bucket (WoW Forever 1.60.1, issue #3) left 0x7000 of it, and the
    /// game refused the storage with "Truncated KMT update section detected".
    #[test]
    fn index_file_size_leaves_the_game_its_update_section() {
        for entries in [0, 1, 1_000, 1_819, 1_820, 3_640, 4_738, 50_000, 119_647, 120_866, 121_968, 200_000] {
            let sorted_end = 0x28 + 18 * entries;
            let size = index_file_size(sorted_end);
            assert_eq!(size % 0x10000, 0, "{entries} entries");
            assert!(size >= sorted_end + 0x7800, "{entries} entries");
            assert!(update_section_room(size, sorted_end) >= 0x7800, "{entries} entries");
        }
        // The reported case, and the one before the boundary moved.
        assert_eq!(index_file_size(0x28 + 18 * 121_968), 0x230000);
        assert_eq!(short_index_file_size(0x28 + 18 * 121_968), 0x220000);
        assert_eq!(update_section_room(0x220000, 0x28 + 18 * 121_968), 0x7000);
    }

    /// Port of `indexFileSizeMatchesAgentWrittenSizes`: the entry counts of
    /// the 16 index files of a Battle.net-installed Warcraft III (131,072
    /// bytes each) and World of Warcraft with the Forever flavor (2,228,224
    /// bytes each), measured on 2026-11-09.
    #[test]
    fn index_file_size_matches_agent_written_sizes() {
        let warcraft_iii =
            [4738, 4726, 4711, 4703, 4725, 4614, 4758, 4658, 4567, 4730, 4789, 4692, 4663, 4679, 4761, 4757];
        let world_of_warcraft = [
            119_967, 119_993, 119_863, 119_776, 119_647, 120_020, 120_223, 120_508, 120_482, 119_805, 119_690, 119_801,
            120_223, 119_975, 120_866, 120_762,
        ];
        for entries in warcraft_iii {
            assert_eq!(index_file_size(0x28 + 18 * entries), 131_072, "{entries} entries");
        }
        for entries in world_of_warcraft {
            assert_eq!(index_file_size(0x28 + 18 * entries), 2_228_224, "{entries} entries");
        }
    }

    /// Port of `writtenIndexFileHasRoomForTheUpdateSection`: a written index
    /// file is that big, and reads back whole.
    #[test]
    fn written_index_file_has_room_for_the_update_section() {
        // 1,819 entries is the smallest count the old rule got wrong.
        let entries = index_entries(1_819);
        let sorted_end = 0x28 + 18 * entries.len();
        let file = encode_index_file(3, &entries);
        assert_eq!(file.len(), 0x20000);
        assert_eq!(short_index_file_size(sorted_end), 0x10000); // what 0.9.5 wrote
        assert!(update_section_room(file.len(), sorted_end) >= 0x7800);
        let parsed = IndexFile::parse(&file).unwrap();
        assert_eq!(parsed.bucket, 3);
        assert_eq!(parsed.sorted_end, sorted_end);
        assert_eq!(parsed.entries.len(), entries.len());
        assert!(parsed.updates.is_empty());
        assert!(file[sorted_end..].iter().all(|&byte| byte == 0));
        assert_eq!(check_update_reserve(&file).unwrap(), ReserveCheck::Sufficient);
    }

    /// The header and checksums byte for byte: the header hash, the 8-byte
    /// pad, the sorted entries' size and chained hash, and an 18-byte record.
    #[test]
    fn writes_the_v7_layout() {
        let a = IndexEntry { key: TruncatedKey::new([0x22; 9]), location: location(1, 0x1E0), size: 100 };
        let b = IndexEntry { key: TruncatedKey::new([0x11; 9]), location: location(0, 0x244), size: 0x12345678 };
        let file = encode_index_file(5, &[a, b]);
        let header: [u8; 16] = [7, 0, 5, 0, 4, 5, 9, 30, 0, 0, 0, 0xC0, 0xFF, 0, 0, 0];
        assert_eq!(file[..4], [16, 0, 0, 0]);
        assert_eq!(file[4..8], lookup3::hashlittle(&header, 0).to_le_bytes());
        assert_eq!(file[8..24], header);
        assert_eq!(file[24..32], [0; 8]);
        assert_eq!(file[32..36], [36, 0, 0, 0]);
        // Sorted by key: b first. Storage offset u40 BE, size u32 LE.
        let record_b = [[0x11; 9].as_slice(), &[0, 0, 0, 0x02, 0x44], &[0x78, 0x56, 0x34, 0x12]].concat();
        let record_a = [[0x22; 9].as_slice(), &[0, 0x40, 0, 0x01, 0xE0], &[100, 0, 0, 0]].concat();
        assert_eq!(file[0x28..0x28 + 18], record_b);
        assert_eq!(file[0x28 + 18..0x28 + 36], record_a);
        let (pc, pb) = lookup3::hashlittle2(&record_b, 0, 0);
        assert_eq!(file[36..40], lookup3::hashlittle2(&record_a, pc, pb).0.to_le_bytes());
        assert_eq!(file.len(), 0x10000);
        assert_eq!(IndexFile::parse(&file).unwrap().entries, [b, a]);
    }

    /// Same keys sort by position; a corrupt entry fails the checksum.
    #[test]
    fn sorts_by_position_and_checks_the_entries() {
        let near = entry([0x33; 9], 0x300, 10);
        let far = entry([0x33; 9], 0x200, 10);
        let mut file = encode_index_file(0, &[near, far]);
        assert_eq!(IndexFile::parse(&file).unwrap().entries, [far, near]);
        file[0x28] ^= 1;
        assert!(matches!(IndexFile::parse(&file), Err(Error::ChecksumMismatch(_))));
        let mut header = encode_index_file(0, &[]);
        header[8] = 8;
        assert!(matches!(IndexFile::parse(&header), Err(Error::Malformed(_))));
        assert!(IndexFile::parse(&[0; 0x27]).is_err());
    }

    /// Port of `loadsIndexUpdateSection`: later records win, and a non-zero
    /// status removes the key.
    #[test]
    fn applies_the_update_section() {
        let (a, b, c) = ([0x11; 9], [0x22; 9], [0x33; 9]);
        let mut index = encode_index_file(0, &[entry(a, 0x1E0, 100), entry(b, 0x244, 100)]);
        let updates = [encode_update_record(&entry(c, 0x2A8, 50), 0), encode_update_record(&entry(b, 0x244, 100), 3)];
        let page = (0x28 + 2 * 18usize).next_multiple_of(0x200);
        index[page..page + 48].copy_from_slice(&updates.concat());
        // A record with a bad guard is skipped.
        index[page + 48..page + 72].copy_from_slice(&[0x80; 24]);

        let parsed = IndexFile::parse(&index).unwrap();
        assert_eq!(parsed.updates.len(), 2);
        let current = parsed.current_entries();
        assert_eq!(current, [entry(a, 0x1E0, 100), entry(c, 0x2A8, 50)]);
        assert_eq!(check_update_reserve(&index).unwrap(), ReserveCheck::Sufficient);

        // An update of a stored key moves it; only the last update counts.
        let mut moved = encode_index_file(0, &[entry(a, 0x1E0, 100)]);
        let records = [encode_update_record(&entry(a, 0x500, 100), 0), encode_update_record(&entry(a, 0x600, 100), 0)];
        let page = (0x28 + 18usize).next_multiple_of(0x200);
        moved[page..page + 48].copy_from_slice(&records.concat());
        assert_eq!(IndexFile::parse(&moved).unwrap().current_entries(), [entry(a, 0x600, 100)]);
    }

    /// The decision half of `repairGivesShortIndexesTheirUpdateSectionBack`
    /// and `repairLeavesSoundAndBusyStoragesAlone`.
    #[test]
    fn measures_short_index_files_for_repair() {
        let entries = index_entries(1_819);
        let sorted_end = 0x28 + 18 * entries.len();
        let full = encode_index_file(0, &entries);
        let short = &full[..short_index_file_size(sorted_end)];
        assert_eq!(check_update_reserve(short).unwrap(), ReserveCheck::Extend { len: index_file_size(sorted_end) });
        // Extending with zeros gives exactly the file this crate writes.
        let mut repaired = short.to_vec();
        repaired.resize(index_file_size(sorted_end), 0);
        assert_eq!(repaired, full);

        let mut used = short.to_vec();
        let page = sorted_end.next_multiple_of(0x200);
        let record = encode_update_record(&entry([0x5C; 9], 0, 0x40), 0);
        used[page..page + 24].copy_from_slice(&record);
        assert_eq!(check_update_reserve(&used).unwrap(), ReserveCheck::InUse);
        assert!(check_update_reserve(&[0; 64]).is_err());
    }

    #[test]
    fn names_index_files() {
        assert_eq!(index_file_name(0x0a, 0x12), "0a00000012.idx");
        assert_eq!(parse_index_file_name("0a00000012.idx"), Some((0x0a, 0x12)));
        assert_eq!(parse_index_file_name("1000000001.idx"), None);
        assert_eq!(parse_index_file_name("0a0000012.idx"), None);
        assert_eq!(parse_index_file_name("0a00000012.ids"), None);
    }
}
