use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blake3::derive_key;
use chacha20::ChaCha8;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use serde::Deserialize;

/// Puzzle solutions cannot be redeemed for tokens if the puzzle was generated too long ago.
const PUZZLE_LIFETIME: Duration = Duration::from_mins(5);

/// Number of sub-puzzles in a complete proof-of-work. This is a hardcoded part of the PoW
/// algorithm, i.e., separate from the `difficulty` parameter in Heavy's config.
const NUM_SOLUTION_OFFSETS: usize = 10;

/// State for working with challenge puzzles and token cookies.
///
/// This struct handles most of the cryptography for Heavy, from generating new PoW challenges to
/// verifying existing cookies. State is not mutated after creation, i.e., it's safe to share
/// references or to clone this struct.
#[derive(Clone, Copy)]
pub struct Authenticator {
    puzzle_key: [u8; 32], // for making nonces specific to this Heavy instance
    token_key: [u8; 32],  // for signing auth cookie tokens
    difficulty: u32,
    token_lifetime: Duration,
}

impl Authenticator {
    pub fn new(secret: &str, difficulty: u32, token_lifetime: u64) -> Self {
        let key = derive_key(
            "[github/cmounce/heavy] [2026-04-10T03:24:16Z] MAC keys",
            secret.as_bytes(),
        );
        Self {
            puzzle_key: derive_key("puzzle", &key),
            token_key: derive_key("token", &key),
            difficulty,
            token_lifetime: Duration::from_secs(token_lifetime),
        }
    }

    /// Generate a puzzle for the client to solve.
    ///
    /// Returns a String that defines everything the client JS needs to solve the puzzle. The puzzle
    /// description contains a nonce that ties the puzzle to the client's IP address and user agent.
    pub fn make_puzzle(&self, ip: IpAddr, ua: &str) -> String {
        let ts = now_secs();
        let nonce = self.puzzle_nonce(&Fingerprint::new(ts, ip, ua));
        format!("{ts}.{}.{}", self.difficulty, hex::encode(nonce))
    }

    /// Verify a puzzle solution, exchanging it for a signed token if the solution is correct.
    ///
    /// Puzzles and tokens are both tied to the same IP address and user agent. If the solution is
    /// valid and belongs to the same client that requested it, this function returns a token String
    /// that can be used in the client's cookie.
    ///
    /// Returns None on failure. All errors are treated equivalently: malformed JSON, expired
    /// puzzle, wrong puzzle, invalid solution, etc.
    pub fn redeem_solution(&self, ip: IpAddr, ua: &str, body: &[u8]) -> Option<String> {
        let solution: Solution = serde_json::from_slice(body).ok()?;
        let ts = solution.timestamp;
        if !is_fresh(ts, PUZZLE_LIFETIME) {
            return None;
        }

        // The nonce is implied by the client's fingerprint, and so it is not necessary for the
        // client to include it when submitting their solution. That does mean we have to re-derive
        // it, but we'd have to do that anyway because we can't trust the client not to lie.
        let fingerprint = Fingerprint::new(ts, ip, ua);
        let nonce = self.puzzle_nonce(&fingerprint);

        if solution.verify_proof_of_work(&nonce, self.difficulty) {
            // We generate the token using the same timestamp as the puzzle solution. This keeps
            // token generation deterministic: you can't replay a solution to get unlimited tokens.
            Some(self.mint_token(&fingerprint))
        } else {
            None
        }
    }

    /// Verify a token from a client cookie value.
    ///
    /// Returns true if the token is still valid. A return value of false is not necessarily a cause
    /// for alarm; it could just mean that the token is expired.
    pub fn verify_token(&self, ip: IpAddr, ua: &str, token: &str) -> bool {
        let Some((ts_hex, hash_hex)) = token.split_once('.') else {
            return false;
        };
        let Ok(ts) = u64::from_str_radix(ts_hex, 16) else {
            return false;
        };
        if !is_fresh(ts, self.token_lifetime) {
            return false;
        }
        let Ok(actual) = blake3::Hash::from_hex(hash_hex) else {
            return false;
        };

        // The blake3 crate guarantees that Hash::eq runs in constant time. This is important
        // because the correct hash value is a secret, and using a regular byte-slice comparison
        // would leak what the token is supposed to look like via a timing side channel.
        let expected = self.token_mac(&Fingerprint::new(ts, ip, &ua));
        expected == actual
    }

    /// To keep token generation deterministic, `ts` should match the puzzle's timestamp.
    fn mint_token(&self, fingerprint: &Fingerprint) -> String {
        format!(
            "{:x}.{}",
            fingerprint.timestamp,
            self.token_mac(fingerprint)
        )
    }

    fn puzzle_nonce(&self, client: &Fingerprint) -> [u8; 32] {
        *client.keyed_hash(&self.puzzle_key).as_bytes()
    }

    fn token_mac(&self, client: &Fingerprint) -> blake3::Hash {
        client.keyed_hash(&self.token_key)
    }
}

/// A timestamped representation of a user/client.
struct Fingerprint<'a> {
    timestamp: u64,
    ip: IpAddr,
    user_agent: &'a str,
}

impl<'a> Fingerprint<'a> {
    fn new(ts: u64, ip: IpAddr, ua: &'a str) -> Self {
        Fingerprint {
            timestamp: ts,
            ip,
            user_agent: &ua,
        }
    }

    fn keyed_hash(&self, key: &[u8; 32]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(key);

        // Hash timestamp, a constant-size field.
        hasher.update(&self.timestamp.to_le_bytes());

        // Hash IP address. This field is variable length, so use a pseudo-length prefix to make it
        // unambiguous which bytes belong to which field in the hash input.
        match &self.ip {
            IpAddr::V4(addr) => {
                hasher.update(b"4");
                hasher.update(&addr.octets()); // [u8; 4]
            }
            IpAddr::V6(addr) => {
                hasher.update(b"6");
                hasher.update(&addr.octets()); // [u8; 16]
            }
        }

        // Hash user agent. This field is also variable length, but we don't need a length prefix
        // because the end of the string is well-defined by virtue of being the final hash input.
        // (If that ever changes, we'll need to add a length prefix here as well!)
        hasher.update(self.user_agent.as_bytes());

        hasher.finalize()
    }
}

/// Representation of a solution a client sent to us, parsed from JSON.
#[derive(Deserialize)]
struct Solution {
    offsets: Vec<u32>,
    timestamp: u64,
}

impl Solution {
    /// Verify just the PoW portion of a puzzle solution.
    ///
    /// This is not the entire verification process; in particular, this does not contain any
    /// timestamp checks to make sure the puzzle isn't stale. This function is only concerned with
    /// whether the cryptography checks out.
    fn verify_proof_of_work(&self, puzzle_nonce: &[u8; 32], difficulty: u32) -> bool {
        if self.offsets.len() != NUM_SOLUTION_OFFSETS {
            return false;
        }
        if self.offsets.windows(2).any(|x| x[0] >= x[1]) {
            return false; // enforce strictly increasing order (and thus no duplicates)
        }

        // Generate ChaCha8 keystream and seek to each one of the client's offsets
        let mut cipher = ChaCha8::new(puzzle_nonce.into(), &Default::default());
        for offset in &self.offsets {
            cipher.seek(*offset as u64 * 4); // convert word offset to byte offset
            let mut buf = [0u8; 4];
            cipher.apply_keystream(&mut buf);
            let word = u32::from_le_bytes(buf);
            if word.leading_zeros() < difficulty {
                // Invalid, solution rejected. It's safe to return early because there's nothing
                // sensitive that a timing side channel could leak: the client already knows the
                // nonce we gave them and the offsets they provided us.
                return false;
            }
        }
        true
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_fresh(epoch_timestamp: u64, max_lifetime: Duration) -> bool {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_timestamp);
    if let Ok(elapsed) = SystemTime::now().duration_since(ts) {
        elapsed <= max_lifetime
    } else {
        false // input timestamp was from the future
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    const TEST_UA: &str = "some-client/1.0";

    fn make_auth() -> Authenticator {
        // Difficulty is chosen to be small enough that tests run quickly, while being high enough
        // such that random data won't accidentally be valid.
        //
        // At D = 6 and NUM_SOLUTION_OFFSETS = 10, the chance of a random solution accidentally
        // being correct is 1 in 2^60, which is in the same ballpark as finding two colliding UUIDs.
        // A random word has a 1 in 64 chance of being correct; finding 10 of them will require
        // generating about 640 words of ChaCha8 output, which is basically nothing (2.5 KB).
        let difficulty = 6;
        Authenticator::new("some-passphrase", difficulty, 60)
    }

    /// Solve a puzzle string. This is basically just a port of the JS algorithm to Rust.
    fn solve_puzzle(puzzle: &str) -> (u64, Vec<u32>) {
        // Decode parameters from string
        let parts: Vec<&str> = puzzle.splitn(3, '.').collect();
        let ts: u64 = parts[0].parse().unwrap();
        let difficulty: u32 = parts[1].parse().unwrap();
        let nonce: [u8; 32] = hex::decode(parts[2]).unwrap().try_into().unwrap();

        // Search ChaCha8 keystream
        let mut offsets = Vec::new();
        let mut cipher = ChaCha8::new((&nonce).into(), &Default::default());
        'outer: for counter in 0.. {
            let mut block = [0u8; 64];
            cipher.apply_keystream(&mut block);
            for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
                let word = u32::from_le_bytes(*chunk);
                if word.leading_zeros() >= difficulty {
                    offsets.push(counter * 16 + i as u32);
                    if offsets.len() == NUM_SOLUTION_OFFSETS {
                        break 'outer;
                    }
                }
            }
        }

        (ts, offsets)
    }

    #[test]
    fn test_happy_path() {
        // Generate and solve a puzzle
        let auth = make_auth();
        let puzzle = auth.make_puzzle(TEST_IP, TEST_UA);
        let (ts, offsets) = solve_puzzle(&puzzle);

        // Exchange it for a token
        let json = serde_json::json!({ "timestamp": ts, "offsets": offsets });
        let token = auth
            .redeem_solution(TEST_IP, TEST_UA, json.to_string().as_bytes())
            .unwrap();

        // Verify the token
        assert!(auth.verify_token(TEST_IP, TEST_UA, &token));
    }

    #[test]
    fn test_invalid_solutions() {
        // Generate and solve a puzzle
        let auth = make_auth();
        let puzzle = auth.make_puzzle(TEST_IP, TEST_UA);
        let (ts, offsets) = solve_puzzle(&puzzle);

        // Sanity check: make sure it's valid as it currently is
        let redeem = |ip, ua, (ts, offsets): (u64, &[u32])| {
            let json = serde_json::json!({ "timestamp": ts, "offsets": offsets });
            auth.redeem_solution(ip, ua, json.to_string().as_bytes())
        };
        assert!(redeem(TEST_IP, TEST_UA, (ts, &offsets)).is_some());

        // Make sure it becomes invalid when we break the rules
        assert!(redeem("12.34.56.78".parse().unwrap(), TEST_UA, (ts, &offsets)).is_none());
        assert!(redeem(TEST_IP, "a-different-client/3.2.1", (ts, &offsets)).is_none());
        assert!(redeem(TEST_IP, TEST_UA, (ts + 1, &offsets)).is_none());
        assert!(redeem(TEST_IP, TEST_UA, (ts, &[])).is_none()); // empty solution
        assert!(redeem(TEST_IP, TEST_UA, (ts, &offsets[0..1])).is_none()); // too short
        assert!(redeem(TEST_IP, TEST_UA, (ts, &[offsets[0]; NUM_SOLUTION_OFFSETS])).is_none()); // duplicates
    }

    #[test]
    fn test_token_verification() {
        let auth = make_auth();
        let mint_token = |ts| {
            let fp = Fingerprint::new(ts, TEST_IP, TEST_UA);
            auth.mint_token(&fp)
        };

        // Make sure tokens expire
        assert!(!auth.verify_token(TEST_IP, TEST_UA, &mint_token(now_secs() - 61))); // too old
        assert!(auth.verify_token(TEST_IP, TEST_UA, &mint_token(now_secs() - 59))); // old
        assert!(auth.verify_token(TEST_IP, TEST_UA, &mint_token(now_secs()))); // brand new
        assert!(!auth.verify_token(TEST_IP, TEST_UA, &mint_token(now_secs() + 1))); // someone's clock is off

        // Make sure bad formats don't cause problems
        assert!(!auth.verify_token(TEST_IP, TEST_UA, ""));
        assert!(!auth.verify_token(TEST_IP, TEST_UA, ".junk"));
        assert!(!auth.verify_token(TEST_IP, TEST_UA, "junk."));
        assert!(!auth.verify_token(TEST_IP, TEST_UA, "junk.morejunk"));
        assert!(!auth.verify_token(TEST_IP, TEST_UA, "c0ffee.morejunk")); // hex timestamp
        assert!(!auth.verify_token(TEST_IP, TEST_UA, "c0ffee.bad")); // not enough nybbles
        assert!(!auth.verify_token(TEST_IP, TEST_UA, "c0ffee.900d")); // MAC too short
    }

    #[test]
    fn test_is_fresh() {
        let now = now_secs();
        let a_minute = Duration::from_mins(1);
        assert!(!is_fresh(now - 61, a_minute)); // INVALID: definitely too old
        assert!(is_fresh(now - 59, a_minute)); // almost too old
        assert!(is_fresh(now - 30, a_minute));
        assert!(is_fresh(now, a_minute)); // brand new
        assert!(!is_fresh(now + 1, a_minute)); // INVALID: from the future
    }
}
