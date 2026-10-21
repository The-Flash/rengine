//! # Checksums: detecting corrupted or half-written data
//!
//! Disks and operating systems can fail in annoying ways:
//!
//! * The power goes out halfway through a write, so only the first part of a
//!   record reaches the disk (a **torn write**).
//! * A bit flips on the storage medium (rare, but it happens at scale).
//!
//! If we blindly trust the bytes we read back, we could return garbage to the
//! user. The fix is a **checksum**: when writing, we compute a small number
//! from the data and store it next to the data. When reading, we compute it
//! again and compare. If the numbers differ, the data is damaged.
//!
//! We use **CRC32**, the same checksum used by zip files, Ethernet, PNG
//! images and many databases. It is not cryptographic (an attacker could forge
//! it), but it is very good at catching accidental damage.
//!
//! ## How CRC32 works (the short version)
//!
//! CRC treats the data as one giant binary number and divides it by a fixed
//! "polynomial" (here `0xEDB88320`). The remainder of that division is the
//! checksum. Doing that bit by bit is slow, so we precompute a 256-entry
//! lookup table that tells us the effect of processing a whole byte at once.

/// The CRC32 polynomial (IEEE 802.3), written in "reversed" bit order.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// Lookup table: `TABLE[b]` is the CRC contribution of byte value `b`.
///
/// `const fn` means the compiler runs `build_table` at compile time, so the
/// table is baked into the program and costs nothing at runtime.
static TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        // Process the 8 bits of this byte one at a time.
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// Computes the CRC32 checksum of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    // Start with all bits set, process each byte through the table, then
    // flip all bits at the end. (These two inversions are part of the
    // standard and make sure leading/trailing zero bytes change the result.)
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = TABLE[index] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_standard_test_vector() {
        // Every CRC32 implementation must produce this value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn detects_a_single_flipped_bit() {
        let original = b"hello storage engine".to_vec();
        let mut damaged = original.clone();
        damaged[3] ^= 0b0000_0100;
        assert_ne!(crc32(&original), crc32(&damaged));
    }
}
