//! Bob Jenkins' lookup3 hash (`hashlittle`, `hashlittle2`), which local
//! storage checks its `.idx` files and archive entry headers with.
//!
//! A port of the byte-at-a-time path of `lookup3.c`, which gives the same
//! result as its aligned fast paths on every input.

/// `hashlittle2`: hashes `data` seeded with `(pc, pb)` and returns the
/// primary and secondary hash `(c, b)`. Feeding them back in as the next
/// call's seeds chains hashes over several pieces, as `.idx` files do.
pub fn hashlittle2(data: &[u8], pc: u32, pb: u32) -> (u32, u32) {
    let mut a = 0xdead_beef_u32.wrapping_add(data.len() as u32).wrapping_add(pc);
    let mut b = a;
    let mut c = a.wrapping_add(pb);

    let mut rest = data;
    while rest.len() > 12 {
        a = a.wrapping_add(word(&rest[0..4]));
        b = b.wrapping_add(word(&rest[4..8]));
        c = c.wrapping_add(word(&rest[8..12]));
        mix(&mut a, &mut b, &mut c);
        rest = &rest[12..];
    }

    // The last block: 0 to 12 bytes, zero-padded. An empty one isn't mixed.
    if rest.is_empty() {
        return (c, b);
    }
    let mut tail = [0u8; 12];
    tail[..rest.len()].copy_from_slice(rest);
    a = a.wrapping_add(word(&tail[0..4]));
    b = b.wrapping_add(word(&tail[4..8]));
    c = c.wrapping_add(word(&tail[8..12]));
    finish(&mut a, &mut b, &mut c);
    (c, b)
}

/// `hashlittle`: the primary hash of `data` seeded with `seed`.
pub fn hashlittle(data: &[u8], seed: u32) -> u32 {
    hashlittle2(data, seed, 0).0
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

/// lookup3's `final`.
fn finish(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUR_SCORE: &[u8] = b"Four score and seven years ago";

    /// Reference values from `driver5()` in lookup3.c (Waypoint's
    /// `lookup3MatchesReferenceVectors`).
    #[test]
    fn matches_reference_vectors() {
        assert_eq!(hashlittle2(&[], 0, 0), (0xdeadbeef, 0xdeadbeef));
        assert_eq!(hashlittle2(&[], 0, 0xdeadbeef), (0xbd5b7dde, 0xdeadbeef));
        assert_eq!(hashlittle2(&[], 0xdeadbeef, 0xdeadbeef), (0x9c093ccd, 0xbd5b7dde));
        assert_eq!(hashlittle2(FOUR_SCORE, 0, 0), (0x17770551, 0xce7226e6));
        // driver5 prints (c, b) after seeding c=1,b=0 and then c=0,b=1.
        assert_eq!(hashlittle2(FOUR_SCORE, 1, 0), (0xcd628161, 0x6cbea4b3));
        assert_eq!(hashlittle2(FOUR_SCORE, 0, 1), (0xe3607cae, 0xbd371de4));
        assert_eq!(hashlittle(FOUR_SCORE, 0), 0x17770551);
        assert_eq!(hashlittle(FOUR_SCORE, 1), 0xcd628161);
    }
}
