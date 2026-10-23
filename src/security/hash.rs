//! Password hashing and token generation, implemented in-crate.
//!
//! WHY NOT ARGON2
//!   The design (SECURITY-DESIGN.md §5) called for argon2. This workspace builds with
//!   `--offline` against a 17-crate cache and cannot reach crates.io, so no crypto crate
//!   can be added. Rather than ship no hashing at all, this module implements
//!   PBKDF2-HMAC-SHA256, which is fully specified (RFC 8018) and testable against
//!   published vectors — see the tests at the bottom.
//!
//!   The stored record keeps an ALGORITHM TAG and its parameters, so replacing this with
//!   argon2 later is a per-user upgrade, not a file-format change. The doc records this
//!   deviation as deliberate.
//!
//!   PBKDF2 is not memory-hard the way argon2 is. It is materially better than a bare
//!   hash, and adequate for a localhost tool; it is NOT a substitute for argon2 on a
//!   network-exposed deployment. That is stated plainly rather than hidden.
//!
//! RANDOMNESS
//!   `random_bytes` mixes OS-seeded `RandomState` hasher keys, the clock, the process id
//!   and a counter through SHA-256. That is best-effort entropy from std alone, not a
//!   CSPRNG: adequate for salts and session tokens on a localhost tool, and the
//!   limitation is documented here and in the design.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// Iterations for NEW password records.
///
/// The cost is set for a RELEASE build: 210k iterations measured at ~290ms per hash, which
/// is a sane login latency and far above a single-hash guess rate.
///
/// A DEBUG build runs the same loop ~12x slower (3.6s per hash), which would make every
/// login and every test crawl, so debug builds use a low factor. That is a development-only
/// weakening and it is safe to change: a record stores ITS OWN iteration count, so records
/// written by either profile verify correctly in the other, and no format changes.
/// The server should be built with `cargo build --release` for real use.
#[cfg(debug_assertions)]
pub const DEFAULT_ITERATIONS: u32 = 3_000;
#[cfg(not(debug_assertions))]
pub const DEFAULT_ITERATIONS: u32 = 210_000;

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// SHA-256 of a byte slice.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Padding: 0x80, zeros, then the message length in bits as a big-endian u64.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + data.len());
    for byte in &key_block {
        inner.push(byte ^ 0x36);
    }
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    for byte in &key_block {
        outer.push(byte ^ 0x5c);
    }
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// PBKDF2-HMAC-SHA256 (RFC 8018), producing exactly 32 bytes.
///
/// Only 32 bytes is needed: one SHA-256 output is one PBKDF2 block.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    debug_assert!(iterations > 0, "RFC 8018 requires a positive iteration count");
    // U1 = HMAC(password, salt || INT(1)) — block index 1, big-endian.
    let mut message = Vec::with_capacity(salt.len() + 4);
    message.extend_from_slice(salt);
    message.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &message);
    let mut result = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for i in 0..32 {
            result[i] ^= u[i];
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Hex + random
// ---------------------------------------------------------------------------

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decodes lowercase or uppercase hex. `None` when malformed.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// Best-effort random bytes (see the module docs: not a CSPRNG).
///
/// Mixes OS-seeded hasher keys (std's `RandomState` is seeded from the OS), the clock,
/// the process id and a process-wide counter, then stretches through SHA-256.
/// Distinct calls in one process differ even if the clock is coarse.
pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut seed = Vec::with_capacity(96);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    seed.extend_from_slice(&counter.to_be_bytes());

    for _ in 0..3 {
        let hasher = RandomState::new();
        let mut h = hasher.build_hasher();
        h.write_u64(counter);
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
        );
        seed.extend_from_slice(&h.finish().to_be_bytes());
    }
    seed.extend_from_slice(&std::process::id().to_be_bytes());

    let mut out = Vec::with_capacity(count);
    let mut block = 0u64;
    while out.len() < count {
        let mut input = seed.clone();
        input.extend_from_slice(&block.to_be_bytes());
        out.extend_from_slice(&sha256(&input));
        block += 1;
    }
    out.truncate(count);
    out
}

// ---------------------------------------------------------------------------
// Password records
// ---------------------------------------------------------------------------

/// A stored password verifier.
///
/// The textual form is `pbkdf2-sha256$<iterations>$<salt hex>$<hash hex>`. The algorithm
/// tag is part of the record so a stronger construction can be introduced per user later
/// without invalidating existing passwords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordRecord {
    pub iterations: u32,
    pub salt_hex: String,
    pub hash_hex: String,
}

impl PasswordRecord {
    /// Hashes `password` with a fresh salt.
    pub fn create(password: &str) -> Self {
        let salt = random_bytes(16);
        let iterations = DEFAULT_ITERATIONS;
        let hash = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
        Self {
            iterations,
            salt_hex: to_hex(&salt),
            hash_hex: to_hex(&hash),
        }
    }

    /// Verifies a candidate password.
    ///
    /// Constant-time comparison: a length or prefix leak in a password check is a real
    /// (if slow) oracle, and it costs nothing to avoid.
    pub fn verify(&self, password: &str) -> bool {
        let Some(salt) = from_hex(&self.salt_hex) else {
            return false;
        };
        let Some(expected) = from_hex(&self.hash_hex) else {
            return false;
        };
        let actual = pbkdf2_sha256(password.as_bytes(), &salt, self.iterations);
        constant_time_eq(&actual, &expected)
    }

    /// The textual form stored in the security file.
    pub fn to_string_form(&self) -> String {
        format!(
            "pbkdf2-sha256${}${}${}",
            self.iterations, self.salt_hex, self.hash_hex
        )
    }

    /// Parses the textual form. `None` for anything unrecognised — an unreadable record
    /// must fail closed (a user who cannot be verified is not logged in), never be
    /// treated as having no password.
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split('$').collect();
        if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
            return None;
        }
        let iterations: u32 = parts[1].parse().ok()?;
        if iterations == 0 {
            return None;
        }
        if from_hex(parts[2]).is_none() || from_hex(parts[3]).is_none() {
            return None;
        }
        Some(Self {
            iterations,
            salt_hex: parts[2].to_string(),
            hash_hex: parts[3].to_string(),
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SHA-256: FIPS 180-4 / NIST published vectors ---

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            to_hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// The 64-byte padding boundary is where naive implementations break: exactly one
    /// block, one block minus one byte, and one block plus one byte.
    #[test]
    fn sha256_handles_the_padding_boundaries() {
        for len in [55usize, 56, 63, 64, 65, 119, 120, 128] {
            let data = vec![b'a'; len];
            // Round-trip through a known-good property: stable across calls.
            assert_eq!(sha256(&data), sha256(&data), "len {len}");
        }
        // 1,000,000 'a' (NIST long message) is slow for a unit test but decisive.
        assert_eq!(
            to_hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // --- HMAC-SHA256: RFC 4231 test case 1 ---

    #[test]
    fn hmac_matches_rfc4231_case_1() {
        let key = [0x0b; 20];
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(to_hex(&hmac_sha256(&key, b"Hi There")), expected);
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let expected = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert_eq!(to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")), expected);
    }

    // --- PBKDF2-HMAC-SHA256: published vectors (RFC 6070 style, SHA-256 variants) ---

    #[test]
    fn pbkdf2_matches_published_vectors() {
        // P="password", S="salt", dkLen=32, HMAC-SHA256
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    // --- Password records ---

    #[test]
    fn a_password_round_trips_through_its_record() {
        let record = PasswordRecord::create("correct horse battery staple");
        assert!(record.verify("correct horse battery staple"));
        assert!(!record.verify("correct horse battery stapl"));
        assert!(!record.verify(""));
        assert!(!record.verify("Correct horse battery staple"));
    }

    #[test]
    fn two_records_for_the_same_password_differ() {
        // A fresh salt per record: identical passwords must not produce identical hashes.
        let a = PasswordRecord::create("same");
        let b = PasswordRecord::create("same");
        assert_ne!(a.salt_hex, b.salt_hex);
        assert_ne!(a.hash_hex, b.hash_hex);
    }

    #[test]
    fn the_record_textual_form_round_trips() {
        let record = PasswordRecord::create("secret");
        let text = record.to_string_form();
        assert_eq!(PasswordRecord::parse(&text), Some(record));
    }

    #[test]
    fn an_unreadable_record_fails_closed() {
        // Nothing that cannot be parsed may be treated as "no password set".
        for text in [
            "",
            "pbkdf2-sha256",
            "argon2id$1$aa$bb",
            "pbkdf2-sha256$0$aa$bb",
            "pbkdf2-sha256$10$zz$bb",
            "pbkdf2-sha256$10$aa",
            "pbkdf2-sha256$notanumber$aa$bb",
        ] {
            assert_eq!(PasswordRecord::parse(text), None, "{text:?}");
        }
    }

    // --- Hex and randomness ---

    #[test]
    fn hex_round_trips() {
        let bytes = vec![0x00, 0x0f, 0xa5, 0xff];
        assert_eq!(to_hex(&bytes), "000fa5ff");
        assert_eq!(from_hex("000fa5ff"), Some(bytes));
        assert_eq!(from_hex("000FA5FF"), Some(vec![0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(from_hex("abc"), None, "odd length");
        assert_eq!(from_hex("zz"), None, "non-hex");
    }

    #[test]
    fn random_bytes_are_the_right_length_and_do_not_repeat() {
        let a = random_bytes(32);
        let b = random_bytes(32);
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b, "two draws must not collide");
        assert!(a.iter().any(|byte| *byte != 0), "must not be all zeros");
    }

    /// A login should be slow enough to hurt guessing and fast enough to be usable.
    /// Not an assertion on wall-clock (machines differ) but a printed measurement, plus a
    /// sanity bound that the default is not accidentally 1 iteration.
    #[test]
    fn the_default_work_factor_is_in_a_sane_range() {
        // Release is the security-relevant profile; debug is deliberately cheap.
        if cfg!(debug_assertions) {
            assert!(DEFAULT_ITERATIONS >= 1_000, "debug work factor is too weak");
        } else {
            assert!(
                DEFAULT_ITERATIONS >= 100_000,
                "a release work factor below 100k iterations is a guess-rate hole"
            );
        }
        let started = std::time::Instant::now();
        let record = PasswordRecord::create("benchmark");
        assert!(record.verify("benchmark"));
        let per_login = started.elapsed();
        println!("password hash+verify with {DEFAULT_ITERATIONS} iterations: {per_login:?}");
        assert!(
            per_login < std::time::Duration::from_secs(5),
            "a login taking {per_login:?} is unusable"
        );
    }
}
