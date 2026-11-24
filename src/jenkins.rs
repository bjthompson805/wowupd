//! Bob Jenkins' lookup3 `hashlittle2`, which CASC uses to guard its index files and data headers.

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c); *a ^= c.rotate_left(4); *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a); *b ^= a.rotate_left(6); *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b); *c ^= b.rotate_left(8); *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c); *a ^= c.rotate_left(16); *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a); *b ^= a.rotate_left(19); *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b); *c ^= b.rotate_left(4); *b = b.wrapping_add(*a);
}

fn finish(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b; *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c; *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a; *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b; *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c; *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a; *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b; *c = c.wrapping_sub(b.rotate_left(24));
}

fn word(k: &[u8]) -> u32 {
    let mut w = 0u32;
    for (i, &byte) in k.iter().enumerate() {
        w |= (byte as u32) << (8 * i);
    }
    w
}

/// Hashes `key` from the seeds `(pc, pb)` and returns the new `(pc, pb)`.
pub fn hashlittle2(key: &[u8], pc: u32, pb: u32) -> (u32, u32) {
    let init = 0xdeadbeefu32.wrapping_add(key.len() as u32).wrapping_add(pc);
    let (mut a, mut b, mut c) = (init, init, init.wrapping_add(pb));
    let mut k = key;
    while k.len() > 12 {
        a = a.wrapping_add(word(&k[0..4]));
        b = b.wrapping_add(word(&k[4..8]));
        c = c.wrapping_add(word(&k[8..12]));
        mix(&mut a, &mut b, &mut c);
        k = &k[12..];
    }
    if k.is_empty() {
        return (c, b);
    }
    // The tail is added as zero-padded little-endian words.
    a = a.wrapping_add(word(&k[..k.len().min(4)]));
    if k.len() > 4 {
        b = b.wrapping_add(word(&k[4..k.len().min(8)]));
    }
    if k.len() > 8 {
        c = c.wrapping_add(word(&k[8..]));
    }
    finish(&mut a, &mut b, &mut c);
    (c, b)
}

pub fn hashlittle(key: &[u8], seed: u32) -> u32 {
    hashlittle2(key, seed, 0).0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vectors from lookup3.c's driver5().
    #[test]
    fn lookup3_vectors() {
        assert_eq!(hashlittle2(b"", 0, 0), (0xdeadbeef, 0xdeadbeef));
        assert_eq!(hashlittle2(b"", 0, 0xdeadbeef), (0xbd5b7dde, 0xdeadbeef));
        assert_eq!(hashlittle2(b"", 0xdeadbeef, 0xdeadbeef), (0x9c093ccd, 0xbd5b7dde));
        assert_eq!(hashlittle2(b"Four score and seven years ago", 0, 0), (0x17770551, 0xce7226e6));
        assert_eq!(hashlittle2(b"Four score and seven years ago", 0, 1), (0xe3607cae, 0xbd371de4));
        assert_eq!(hashlittle2(b"Four score and seven years ago", 1, 0), (0xcd628161, 0x6cbea4b3));
        assert_eq!(hashlittle(b"Four score and seven years ago", 0), 0x17770551);
        assert_eq!(hashlittle(b"Four score and seven years ago", 1), 0xcd628161);
    }
}
