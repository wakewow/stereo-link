//! 极小的无依赖随机串生成器。
//!
//! 只用来给 MiNA 的 `requestId` 生成 `app_ios_` 后面的 30 位随机串，
//! 不需要密码学强度，因此不引入 `rand` 依赖。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static STATE: AtomicU64 = AtomicU64::new(0);

fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let stack = &nanos as *const u64 as u64;
    let heap = &STATE as *const _ as u64;
    nanos ^ stack.rotate_left(17) ^ heap.rotate_left(31) | 1
}

fn next_u64() -> u64 {
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        s = seed();
    }
    // xorshift64*
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    STATE.store(s, Ordering::Relaxed);
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 生成 `n` 位小写字母 + 数字的随机串。
pub fn alnum(n: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..n)
        .map(|_| CHARS[(next_u64() % CHARS.len() as u64) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn alnum_length_and_charset() {
        let s = alnum(30);
        assert_eq!(s.len(), 30);
        assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn alnum_is_not_constant() {
        let set: HashSet<String> = (0..64).map(|_| alnum(30)).collect();
        assert!(set.len() > 60, "随机串重复率过高: {}", set.len());
    }
}
