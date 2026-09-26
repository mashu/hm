//! Reed–Solomon codes over GF(256) as IL2P uses them: field polynomial
//! x^8 + x^4 + x^3 + x^2 + 1 (0x11d), generator roots α^0 … α^(n-1),
//! systematic, parity after the data, shortened to the block's length.
//!
//! Byte 0 of a block is the coefficient of the highest power of x.

use alloc::vec;
use alloc::vec::Vec;

const POLY: u16 = 0x11d;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn tables() -> Tables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    exp[510] = exp[0];
    exp[511] = exp[1];
    Tables { exp, log }
}

static T: Tables = tables();

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        T.exp[T.log[a as usize] as usize + T.log[b as usize] as usize]
    }
}

fn div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0);
    if a == 0 {
        0
    } else {
        T.exp[T.log[a as usize] as usize + 255 - T.log[b as usize] as usize]
    }
}

/// α^i.
fn pow_a(i: usize) -> u8 {
    T.exp[i % 255]
}

/// Generator polynomial with `nroots` roots, highest power first, monic.
fn generator(nroots: usize) -> Vec<u8> {
    let mut g = vec![1u8];
    for i in 0..nroots {
        // g(x) *= (x + α^i)
        let root = pow_a(i);
        let mut next = vec![0u8; g.len() + 1];
        for (j, &c) in g.iter().enumerate() {
            next[j] ^= c;
            next[j + 1] ^= mul(c, root);
        }
        g = next;
    }
    g
}

/// Parity bytes for `data` (at most 255 - `nroots` bytes).
pub fn encode(data: &[u8], nroots: usize) -> Vec<u8> {
    debug_assert!(data.len() + nroots <= 255);
    let g = generator(nroots);
    let mut rem = vec![0u8; nroots];
    for &d in data {
        let feedback = d ^ rem[0];
        rem.rotate_left(1);
        rem[nroots - 1] = 0;
        if feedback != 0 {
            for j in 0..nroots {
                rem[j] ^= mul(feedback, g[j + 1]);
            }
        }
    }
    rem
}

/// Correct `block` (data then `nroots` parity bytes) in place. Returns the
/// number of bytes corrected, or `None` when the errors are beyond repair.
pub fn decode(block: &mut [u8], nroots: usize) -> Option<usize> {
    let n = block.len();
    if n <= nroots || n > 255 {
        return None;
    }
    // Syndromes S_j = r(α^j).
    let mut s = vec![0u8; nroots];
    for (j, sj) in s.iter_mut().enumerate() {
        let a = pow_a(j);
        *sj = block.iter().fold(0u8, |acc, &b| mul(acc, a) ^ b);
    }
    if s.iter().all(|&x| x == 0) {
        return Some(0);
    }
    // Berlekamp–Massey: error locator Λ(x), lowest power first.
    let mut lambda = vec![0u8; nroots + 1];
    lambda[0] = 1;
    let mut prev = lambda.clone();
    let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
    for k in 0..nroots {
        let mut d = s[k];
        for i in 1..=l {
            d ^= mul(lambda[i], s[k - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = div(d, b);
        let t = lambda.clone();
        for i in 0..=nroots - m {
            lambda[i + m] ^= mul(coef, prev[i]);
        }
        if 2 * l <= k {
            l = k + 1 - l;
            prev = t;
            b = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if 2 * l > nroots {
        return None;
    }
    // Ω(x) = S(x) Λ(x) mod x^nroots.
    let mut omega = vec![0u8; nroots];
    for i in 0..nroots {
        for j in 0..=i.min(l) {
            omega[i] ^= mul(s[i - j], lambda[j]);
        }
    }
    // Chien search over the positions of this (shortened) block, then Forney.
    let mut fixes = Vec::new();
    for pos in 0..n {
        let power = n - 1 - pos; // X = α^power
        let x_inv = pow_a(255 - power % 255);
        let at = lambda[..=l].iter().rev().fold(0u8, |acc, &c| mul(acc, x_inv) ^ c);
        if at != 0 {
            continue;
        }
        // Λ'(x): odd terms only (characteristic 2).
        let mut deriv = 0u8;
        let mut xp = 1u8; // x_inv^(i-1)
        for (i, &c) in lambda[..=l].iter().enumerate().skip(1) {
            if i % 2 == 1 {
                deriv ^= mul(c, xp);
            }
            xp = mul(xp, x_inv);
        }
        let om = omega.iter().rev().fold(0u8, |acc, &c| mul(acc, x_inv) ^ c);
        if deriv == 0 {
            return None;
        }
        // First root α^0: magnitude = X · Ω(X⁻¹) / Λ'(X⁻¹).
        fixes.push((pos, mul(pow_a(power), div(om, deriv))));
    }
    if fixes.len() != l {
        return None; // roots outside the block: too many errors
    }
    for &(pos, e) in &fixes {
        block[pos] ^= e;
    }
    Some(fixes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn field_basics() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, div(1, a)), 1);
        }
        assert_eq!(generator(2), vec![1, 3, 2]); // (x+1)(x+2) = x^2 + 3x + 2
    }

    #[test]
    fn a_codeword_has_zero_syndromes() {
        let data: Vec<u8> = (0..100).map(|i| (i * 37 + 11) as u8).collect();
        for nroots in [2, 4, 6, 8, 16] {
            let mut block = data.clone();
            block.extend(encode(&data, nroots));
            assert_eq!(decode(&mut block, nroots), Some(0));
        }
    }

    #[test]
    fn corrects_up_to_half_the_parity_and_never_panics_beyond() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        for nroots in [2usize, 4, 6, 8, 16] {
            for _ in 0..300 {
                let len = 1 + (rng(&mut seed) % (255 - nroots as u64)) as usize;
                let data: Vec<u8> = (0..len).map(|_| rng(&mut seed) as u8).collect();
                let mut good = data.clone();
                good.extend(encode(&data, nroots));
                let n = good.len();
                let errors = (rng(&mut seed) as usize) % (nroots / 2 + 1);
                let mut bad = good.clone();
                let mut hit = Vec::new();
                while hit.len() < errors.min(n) {
                    let p = (rng(&mut seed) as usize) % n;
                    if !hit.contains(&p) {
                        hit.push(p);
                        bad[p] ^= 1 + (rng(&mut seed) % 255) as u8;
                    }
                }
                assert_eq!(
                    decode(&mut bad, nroots),
                    Some(hit.len()),
                    "nroots {nroots} len {len}"
                );
                assert_eq!(bad, good);
                // Past the limit: either refused or "corrected" to some codeword, never a panic.
                let mut worse = good.clone();
                for _ in 0..nroots {
                    let p = (rng(&mut seed) as usize) % n;
                    worse[p] ^= 1 + (rng(&mut seed) % 255) as u8;
                }
                if decode(&mut worse, nroots).is_some() {
                    let mut again = worse.clone();
                    assert_eq!(decode(&mut again, nroots), Some(0));
                }
            }
        }
    }
}
