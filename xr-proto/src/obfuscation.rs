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
            ModifierStrategy::RotatingSalt => {
                (self.salt.rotate_left((i % 32) as u32) & 0xFF) as u8
            }
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

        assert_ne!(data1, data2, "different nonces should produce different output");
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
}
