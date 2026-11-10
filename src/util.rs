//! Frozen shared helpers: hash, deterministic payload, percentiles, minimal JSON.

/// FNV-1a 64-bit (frozen hash for the index + payload seeding, EGM #12b).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Hash of a block key (session, layer, token_start) — used by index and payload seeding.
pub fn key_hash(session: u64, layer: u32, token_start: u64) -> u64 {
    let mut h = session.to_le_bytes().to_vec();
    h.extend_from_slice(&layer.to_le_bytes());
    h.extend_from_slice(&token_start.to_le_bytes());
    fnv1a64(&h)
}

/// SplitMix64 step.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Frozen deterministic payload pattern (ARCHITECTURE §11): SplitMix64 seeded with
/// `seed ^ key_hash(session, layer, token_start)`, filling 8-byte little-endian words.
/// Integrity verification regenerates and compares — payload bytes are never stored in traces.
pub fn payload_fill(session: u64, layer: u32, token_start: u64, seed: u64, buf: &mut [u8]) {
    let mut st = seed ^ key_hash(session, layer, token_start);
    let mut chunks = buf.chunks_exact_mut(8);
    for c in &mut chunks {
        c.copy_from_slice(&splitmix64(&mut st).to_le_bytes());
    }
    let rem = chunks.into_remainder();
    if !rem.is_empty() {
        let last = splitmix64(&mut st).to_le_bytes();
        rem.copy_from_slice(&last[..rem.len()]);
    }
}

/// Percentile of a sample vector in µs (nearest-rank). Does not reorder input.
pub fn pct(samples: &[u64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s: Vec<u64> = samples.to_vec();
    s.sort_unstable();
    let n = s.len();
    let idx = (((p / 100.0) * n as f64).ceil() as usize).clamp(1, n) - 1;
    s[idx] as f64
}

pub fn mean(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<u64>() as f64 / samples.len() as f64
    }
}

/// Minimal JSON object writer (std-only; values are numbers/strings we control).
pub struct Json {
    parts: Vec<String>,
}

impl Json {
    pub fn new() -> Json {
        Json { parts: Vec::new() }
    }
    pub fn kv_raw(&mut self, k: &str, v: &str) -> &mut Self {
        self.parts.push(format!("\"{k}\":{v}"));
        self
    }
    pub fn kv_u64(&mut self, k: &str, v: u64) -> &mut Self {
        self.kv_raw(k, &v.to_string())
    }
    pub fn kv_f64(&mut self, k: &str, v: f64) -> &mut Self {
        self.kv_raw(k, &format!("{v}"))
    }
    pub fn kv_str(&mut self, k: &str, v: &str) -> &mut Self {
        self.kv_raw(k, &format!("\"{}\"", v.replace('\\', "/").replace('"', "'")))
    }
    pub fn kv_nested(&mut self, k: &str, nested: &str) -> &mut Self {
        self.kv_raw(k, nested)
    }
    pub fn build(&self) -> String {
        format!("{{{}}}", self.parts.join(","))
    }
}

/// Nested stats object: {"mean":..,"p50":..,"p95":..,"p99":..}
pub fn stats_json(samples: &[u64]) -> String {
    let mut j = Json::new();
    j.kv_f64("mean", mean(samples))
        .kv_f64("p50", pct(samples, 50.0))
        .kv_f64("p95", pct(samples, 95.0))
        .kv_f64("p99", pct(samples, 99.0));
    j.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_deterministic_and_length_exact() {
        let mut a = vec![0u8; 100];
        let mut b = vec![0u8; 100];
        payload_fill(1, 2, 4096, 42, &mut a);
        payload_fill(1, 2, 4096, 42, &mut b);
        assert_eq!(a, b);
        payload_fill(1, 2, 4096, 43, &mut b);
        assert_ne!(a, b);
        payload_fill(1, 2, 8192, 42, &mut b); // different token_start differs
        assert_ne!(a, b);
    }

    #[test]
    fn fnv_reference_vector() {
        // FNV-1a 64 of "a" = 0xaf63dc4c8601ec8c (published reference vector)
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn percentiles_sorted() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!(pct(&s, 50.0), 50.0);
        assert_eq!(pct(&s, 99.0), 99.0);
        assert_eq!(pct(&s, 100.0), 100.0);
    }
}
