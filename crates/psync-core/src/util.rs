use crate::error::Result;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// `f` applied to every item on all CPU cores; results in input order.
///
/// Items are handed out in small chunks from a shared queue, so a few slow
/// items (large files) do not leave the other threads idle. Small inputs run
/// on the calling thread. A panic in `f` is propagated to the caller.
pub fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(n);
    if threads <= 1 || n < 2 {
        return items.into_iter().map(f).collect();
    }
    let chunk = (n / (threads * 8)).max(1);
    let mut chunks = Vec::with_capacity(n.div_ceil(chunk));
    let mut it = items.into_iter();
    loop {
        let c: Vec<T> = it.by_ref().take(chunk).collect();
        if c.is_empty() {
            break;
        }
        chunks.push(c);
    }
    let queue = std::sync::Mutex::new(chunks.into_iter().enumerate());
    let mut done: Vec<(usize, Vec<R>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        // The lock is held only to take the next chunk.
                        let next = queue.lock().unwrap_or_else(|p| p.into_inner()).next();
                        let Some((i, c)) = next else { break };
                        out.push((i, c.into_iter().map(&f).collect::<Vec<R>>()));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
    });
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().flat_map(|(_, v)| v).collect()
}

/// `n` random bytes from the OS, as lowercase hex (`2n` characters). Used for
/// unique temporary names, daemon instances and access tokens.
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    crate::platform::random_bytes(&mut b);
    let mut s = String::with_capacity(2 * n);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Replace a file atomically and durably: after a crash it holds either the
/// old or the new contents, and everything written before (e.g. the objects
/// a new `op_head` refers to) is on disk before the new contents can be.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        crate::platform::barrier(&f)?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        crate::platform::sync_dir(dir)?;
    }
    Ok(())
}

/// Replace a file atomically, without waiting for the disk (for caches that
/// are rebuilt when lost or unreadable, such as a workspace's index).
pub fn replace_file(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Format a unix-millisecond timestamp as RFC 3339 UTC.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe as i64 + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Human-friendly relative time, e.g. "5m ago".
pub fn relative(ms: i64) -> String {
    let s = ((now_ms() - ms).max(0)) / 1000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(rfc3339(1_793_750_400_000), "2026-11-04T00:00:00.000Z");
    }
}

#[cfg(test)]
mod random_tests {
    use super::random_hex;

    #[test]
    fn random_hex_is_hex_and_differs() {
        let a = random_hex(16);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(a, random_hex(16));
        // Larger than one getentropy call.
        assert_eq!(random_hex(1000).len(), 2000);
    }

    #[test]
    fn par_map_keeps_order() {
        for n in [0, 1, 2, 7, 64, 1000, 10_007] {
            let got = super::par_map((0..n).collect(), |x: u64| x * 3);
            assert_eq!(got, (0..n).map(|x| x * 3).collect::<Vec<_>>(), "n={n}");
        }
    }

    #[test]
    #[should_panic(expected = "boom")]
    fn par_map_propagates_panics() {
        super::par_map((0..100).collect(), |x: u32| if x == 57 { panic!("boom") } else { x });
    }
}
