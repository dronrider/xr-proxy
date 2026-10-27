/// Obfuscation engine: positional XOR with configurable modifiers.
///
/// Formula: obfuscated[i] = data[i] ^ key[(i + offset) % key_len] ^ modifier(i)
///
/// The combination of key, salt, and modifier strategy makes each deployment
/// unique and unrecognizable to signature-based DPI systems.

/// Available modifier strategies that determine how position affects obfuscation.
#[derive(Debug, Clone, Copy)]
pub enum ModifierStrategy {
    /// modifier(i) = (i * salt) & 0xFF
    PositionalXorRotate,
    /// modifier(i) = salt.rotate_left(i % 32) & 0xFF
    RotatingSalt,
    /// modifier(i) = substitution_table[(i + salt) % 256]
    SubstitutionTable,
}

impl ModifierStrategy {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "positional_xor_rotate" => Some(Self::PositionalXorRotate),
            "rotating_salt" => Some(Self::RotatingSalt),
            "substitution_table" => Some(Self::SubstitutionTable),
            _ => None,
        }
    }
}

/// Core obfuscation context, created once from config and reused for all packets.
#[derive(Debug, Clone)]
pub struct Obfuscator {
    key: Vec<u8>,
    salt: u32,
    strategy: ModifierStrategy,
    /// Pre-computed substitution table (256 bytes), used by SubstitutionTable strategy.
    sub_table: [u8; 256],
}

impl Obfuscator {
    pub fn new(key: Vec<u8>, salt: u32, strategy: ModifierStrategy) -> Self {
        assert!(!key.is_empty(), "obfuscation key must not be empty");

        // Generate substitution table from key and salt.
        // This creates a deterministic but hard-to-guess byte mapping.
        let mut sub_table = [0u8; 256];
        let mut state = salt;
        for (i, entry) in sub_table.iter_mut().enumerate() {
            // Simple PRNG seeded by salt and key
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state ^= key[i % key.len()] as u32;
            *entry = (state >> 16) as u8;
        }

        Self {
            key,
            salt,
            strategy,
            sub_table,
        }
    }

    /// Compute modifier byte for position `i`.
    #[inline]
    fn modifier(&self, i: usize) -> u8 {
        match self.strategy {
            ModifierStrategy::PositionalXorRotate => {
                ((i as u32).wrapping_mul(self.salt) & 0xFF) as u8
            }
            ModifierStrategy::RotatingSalt => (self.salt.rotate_left((i % 32) as u32) & 0xFF) as u8,
            ModifierStrategy::SubstitutionTable => {
                self.sub_table[(i.wrapping_add(self.salt as usize)) % 256]
            }
        }
    }

    /// Obfuscate or deobfuscate data in-place. XOR is symmetric — same operation
    /// for both directions.
    ///
    /// `offset` is derived from the packet nonce and shifts the key position,
    /// making each packet use a different key alignment.
    pub fn apply(&self, data: &mut [u8], offset: u32) {
        let key_len = self.key.len();
        let offset = offset as usize;

        if key_len.is_power_of_two() {
            let mask = key_len - 1;
            for (i, byte) in data.iter_mut().enumerate() {
                let key_byte = self.key[(i + offset) & mask];
                let mod_byte = self.modifier(i + offset);
                *byte ^= key_byte ^ mod_byte;
            }
            return;
        }

        for (i, byte) in data.iter_mut().enumerate() {
            let key_byte = self.key[(i + offset) % key_len];
            let mod_byte = self.modifier(i + offset);
            *byte ^= key_byte ^ mod_byte;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let key = b"test-key-1234567890abcdef".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);

        let original = b"Hello, World! This is a test payload.".to_vec();
        let mut data = original.clone();

        // Obfuscate
        obfs.apply(&mut data, 42);
        assert_ne!(data, original, "data should be different after obfuscation");

        // Deobfuscate (same operation)
        obfs.apply(&mut data, 42);
        assert_eq!(data, original, "data should match after deobfuscation");
    }

    #[test]
    fn test_different_nonce_different_output() {
        let key = b"test-key".to_vec();
        let obfs = Obfuscator::new(key, 0x12345678, ModifierStrategy::RotatingSalt);

        let original = b"same data".to_vec();

        let mut data1 = original.clone();
        obfs.apply(&mut data1, 1);

        let mut data2 = original.clone();
        obfs.apply(&mut data2, 2);

        assert_ne!(
            data1, data2,
            "different nonces should produce different output"
        );
    }

    #[test]
    fn test_all_strategies_roundtrip() {
        let key = b"key-for-testing-all-strategies!!".to_vec();
        let original = b"Payload data for strategy test".to_vec();

        for strategy in [
            ModifierStrategy::PositionalXorRotate,
            ModifierStrategy::RotatingSalt,
            ModifierStrategy::SubstitutionTable,
        ] {
            let obfs = Obfuscator::new(key.clone(), 0xCAFEBABE, strategy);
            let mut data = original.clone();
            obfs.apply(&mut data, 100);
            assert_ne!(data, original);
            obfs.apply(&mut data, 100);
            assert_eq!(data, original, "roundtrip failed for {:?}", strategy);
        }
    }

    fn reference_apply(
        key: &[u8],
        salt: u32,
        strategy: ModifierStrategy,
        data: &mut [u8],
        offset: u32,
    ) {
        let mut table = [0u8; 256];
        let mut state = salt;
        for (i, entry) in table.iter_mut().enumerate() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state ^= key[i % key.len()] as u32;
            *entry = (state >> 16) as u8;
        }
        for (i, byte) in data.iter_mut().enumerate() {
            let position = i as u64 + u64::from(offset);
            let modifier = match strategy {
                ModifierStrategy::PositionalXorRotate => (position as u32).wrapping_mul(salt) as u8,
                ModifierStrategy::RotatingSalt => salt.rotate_left((position % 32) as u32) as u8,
                ModifierStrategy::SubstitutionTable => {
                    table[((position + u64::from(salt)) % 256) as usize]
                }
            };
            *byte ^= key[(position % key.len() as u64) as usize] ^ modifier;
        }
    }

    #[test]
    fn wire_bytes_match_reference_at_key_and_offset_boundaries() {
        let lengths: [usize; 12] = [0, 1, 7, 8, 15, 16, 31, 32, 33, 255, 256, 257];
        for key_len in [1, 2, 3, 7, 8, 15, 16, 31, 32, 33, 63, 64, 65, 128, 256, 257] {
            let key: Vec<u8> = (0..key_len).map(|i| (i * 37 + 11) as u8).collect();
            for strategy in [
                ModifierStrategy::PositionalXorRotate,
                ModifierStrategy::RotatingSalt,
                ModifierStrategy::SubstitutionTable,
            ] {
                for salt in [0, 1, 0xDEADBEEF, u32::MAX] {
                    let obfs = Obfuscator::new(key.clone(), salt, strategy);
                    for offset in [0, 1, 31, 32, 255, 256, u32::MAX - 7, u32::MAX] {
                        for len in lengths {
                            if (offset as usize)
                                .checked_add(len.saturating_sub(1))
                                .is_none()
                            {
                                continue;
                            }
                            let mut actual: Vec<u8> =
                                (0..len).map(|i| (i * 29 + 7) as u8).collect();
                            let mut expected = actual.clone();
                            reference_apply(&key, salt, strategy, &mut expected, offset);
                            obfs.apply(&mut actual, offset);
                            assert_eq!(actual, expected, "key_len={key_len}, strategy={strategy:?}, salt={salt}, offset={offset}, len={len}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn maximum_frame_payload_matches_reference_for_all_strategies() {
        for key_len in [31, 32, 33, 64] {
            let key: Vec<u8> = (0..key_len).map(|i| (i * 37 + 11) as u8).collect();
            for strategy in [
                ModifierStrategy::PositionalXorRotate,
                ModifierStrategy::RotatingSalt,
                ModifierStrategy::SubstitutionTable,
            ] {
                for offset in [0, 255, u32::MAX - 7, u32::MAX] {
                    if (offset as usize).checked_add(65534).is_none() {
                        continue;
                    }
                    let mut actual: Vec<u8> = (0..65535).map(|i| (i * 29 + 7) as u8).collect();
                    let original = actual.clone();
                    let mut expected = actual.clone();
                    reference_apply(&key, 0xDEADBEEF, strategy, &mut expected, offset);
                    let obfs = Obfuscator::new(key.clone(), 0xDEADBEEF, strategy);
                    obfs.apply(&mut actual, offset);
                    assert_eq!(actual, expected);
                    obfs.apply(&mut actual, offset);
                    assert_eq!(actual, original);
                }
            }
        }
    }
}
