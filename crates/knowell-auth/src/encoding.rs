//! Small byte helpers shared by tokens and panel security: lowercase
//! RFC 4648 base32 (canonical only), constant-time comparison and OS
//! randomness.

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Encodes `data` as unpadded lowercase base32.
pub(crate) fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().saturating_mul(8).div_ceil(5));
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(symbol((acc >> bits) & 31));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(symbol((acc << (5 - bits)) & 31));
    }
    out
}

fn symbol(index: u32) -> char {
    let i = usize::try_from(index & 31).unwrap_or(0);
    ALPHABET.get(i).copied().map_or('a', char::from)
}

/// Decodes canonical unpadded lowercase base32.
///
/// Returns `None` for characters outside the alphabet (including uppercase)
/// and for non-canonical input whose unused trailing bits are not zero, so
/// every byte string has exactly one accepted text form.
pub(crate) fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len().saturating_mul(5) / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for ch in text.bytes() {
        let value = ALPHABET.iter().position(|&a| a == ch)?;
        acc = (acc << 5) | u32::try_from(value).ok()?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// Constant-time equality for secret byte strings. Lengths are public.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// `N` random bytes from the operating system.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], getrandom::Error> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_roundtrip_and_known_vector() {
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let text = base32_encode(&data);
            assert_eq!(base32_decode(&text), Some(data));
        }
    }

    #[test]
    fn base32_rejects_noncanonical() {
        assert_eq!(base32_decode("MZXW6YTBOI"), None);
        assert_eq!(base32_decode("mzxw6ytbo1"), None);
        // 1 byte = "me" + 3 zero pad bits; "mf" has a non-zero pad bit.
        assert_eq!(base32_decode("me"), Some(vec![b'a']));
        assert_eq!(base32_decode("mf"), None);
    }

    #[test]
    fn ct_eq_behaviour() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
