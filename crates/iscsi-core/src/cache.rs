//! Opt-in, session-local volatile cache. No network I/O is performed here.
use crate::Error;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};

pub const SEGMENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_BATCH_BYTES: usize = 1024 * 1024;

const MIB: usize = 1024 * 1024;
const AUTO_READ_MIN_BYTES: usize = 128 * MIB;
const AUTO_READ_MAX_BYTES: usize = 2 * 1024 * MIB;
const AUTO_READ_FALLBACK_BYTES: usize = 512 * MIB;
const WRITE_BACK_MIN_BYTES: usize = 64 * MIB;
const WRITE_BACK_MAX_BYTES: usize = 256 * MIB;

/// Automatic read cache size: 1/8 of available memory, clamped to 128 MiB-2 GiB,
/// rounded down to a multiple of 64 MiB.
///
/// On macOS, use physical memory as an estimate to avoid extra dependencies.
/// Fall back to 512 MiB if memory cannot be queried.
/// Cache the result per process to avoid probing on every UI frame.
pub fn auto_read_cache_bytes() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| auto_read_cache_from(usable_memory_bytes()))
}

/// Recommended write-back size: 1/4 of the automatic read cache, clamped to 64 MiB-256 MiB.
pub fn recommended_write_back_bytes() -> usize {
    (auto_read_cache_bytes() / 4).clamp(WRITE_BACK_MIN_BYTES, WRITE_BACK_MAX_BYTES)
}

fn auto_read_cache_from(memory: Option<u64>) -> usize {
    let budget = memory.map_or(AUTO_READ_FALLBACK_BYTES as u64, |bytes| bytes / 8);
    let clamped = budget.clamp(AUTO_READ_MIN_BYTES as u64, AUTO_READ_MAX_BYTES as u64) as usize;
    clamped / (64 * MIB) * (64 * MIB)
}

fn usable_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |key: &str| {
            meminfo.lines().find_map(|line| {
                line.strip_prefix(key)?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
                    .map(|kib| kib * 1024)
            })
        };
        field("MemAvailable:").or_else(|| field("MemTotal:"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheConfig {
    /// Zero disables the read cache. Budgets are rounded down to whole segments.
    pub read_capacity_bytes: usize,
    /// Zero preserves write-through. Nonzero enables volatile write-back.
    pub write_back_capacity_bytes: usize,
    pub flush_interval: Option<Duration>,
}

impl CacheConfig {
    pub fn enabled(&self) -> bool {
        self.read_capacity_bytes > 0 || self.write_back_capacity_bytes > 0
    }
}

/// Hit/miss counts are bytes, not requests. Bypassed reads count as LUN misses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes_served: u64,
    pub evictions: u64,
    pub dirty_bytes: usize,
    pub flushes: u64,
    pub write_bytes: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits as f64 + self.misses as f64;
        if total == 0.0 {
            0.0
        } else {
            self.hits as f64 / total
        }
    }
}

#[derive(Default)]
pub struct Cache {
    state: Mutex<State>,
}

pub(crate) struct Entry {
    data: Arc<[u8]>,
    age: u128,
}

#[derive(Default)]
pub(crate) struct State {
    pub config: CacheConfig,
    clean: HashMap<u64, Entry>,
    lru: BTreeSet<(u128, u64)>,
    clock: u128,
    pub dirty: BTreeMap<u64, Arc<[u8]>>,
    pub stats: CacheStats,
    pub flush_error: Option<String>,
}

impl Cache {
    pub fn new(config: CacheConfig) -> Self {
        Self {
            state: Mutex::new(State {
                config,
                ..State::default()
            }),
        }
    }

    pub fn stats(&self) -> CacheStats {
        let state = self.lock();
        if state.config.enabled() {
            state.stats
        } else {
            CacheStats::default()
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        // A panic must never implicitly discard acknowledged dirty writes.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl State {
    pub fn configure(&mut self, config: CacheConfig) -> Result<(), Error> {
        if !self.dirty.is_empty() {
            return Err(Error::Config(
                "flush dirty data before changing cache configuration".into(),
            ));
        }
        if config
            .flush_interval
            .is_some_and(|interval| interval.is_zero())
        {
            return Err(Error::Config("flush interval must be nonzero".into()));
        }
        *self = Self {
            config,
            ..Self::default()
        };
        Ok(())
    }

    pub fn get(&mut self, offset: u64) -> Option<Arc<[u8]>> {
        let data = if let Some(data) = self.dirty.get(&offset) {
            data.clone()
        } else {
            let entry = self.clean.get_mut(&offset)?;
            self.lru.remove(&(entry.age, offset));
            self.clock += 1;
            entry.age = self.clock;
            self.lru.insert((entry.age, offset));
            entry.data.clone()
        };
        self.stats.hits += data.len() as u64;
        self.stats.bytes_served += data.len() as u64;
        Some(data)
    }

    pub fn contains(&self, offset: u64) -> bool {
        self.dirty.contains_key(&offset) || self.clean.contains_key(&offset)
    }

    pub fn insert(&mut self, offset: u64, data: &[u8]) {
        if data.len() != SEGMENT_BYTES || self.config.read_capacity_bytes < SEGMENT_BYTES {
            return;
        }
        self.invalidate(offset, data.len());
        while self.clean.len() >= self.config.read_capacity_bytes / SEGMENT_BYTES {
            if let Some((age, key)) = self.lru.pop_first() {
                self.clean.remove(&key);
                self.stats.evictions += 1;
                debug_assert!(age <= self.clock);
            }
        }
        self.clock += 1;
        self.clean.insert(
            offset,
            Entry {
                data: Arc::from(data),
                age: self.clock,
            },
        );
        self.lru.insert((self.clock, offset));
    }

    pub fn invalidate(&mut self, offset: u64, len: usize) {
        let end = u128::from(offset) + len as u128;
        let start = offset / SEGMENT_BYTES as u64 * SEGMENT_BYTES as u64;
        let mut segment = u128::from(start);
        while segment < end {
            if let Some(entry) = self.clean.remove(&(segment as u64)) {
                self.lru.remove(&(entry.age, segment as u64));
            }
            segment += SEGMENT_BYTES as u128;
        }
    }

    pub fn stage(&mut self, offset: u64, data: &[u8]) {
        self.invalidate(offset, data.len());
        for (index, segment) in data.as_chunks::<SEGMENT_BYTES>().0.iter().enumerate() {
            self.dirty.insert(
                offset + (index * SEGMENT_BYTES) as u64,
                Arc::from(segment.as_slice()),
            );
        }
        self.stats.dirty_bytes = self.dirty.len() * SEGMENT_BYTES;
        self.stats.write_bytes += data.len() as u64;
    }

    pub fn miss(&mut self, bytes: usize) {
        self.stats.misses += bytes as u64;
    }

    pub fn flushed(&mut self) {
        // Caller holds the asynchronous write barrier through SYNCHRONIZE CACHE.
        self.dirty.clear();
        self.stats.dirty_bytes = 0;
        self.stats.flushes += 1;
        self.flush_error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_budget_is_clamped_and_rounded() {
        assert_eq!(auto_read_cache_from(None), AUTO_READ_FALLBACK_BYTES);
        assert_eq!(auto_read_cache_from(Some(512 * MIB as u64)), 128 * MIB);
        assert_eq!(
            auto_read_cache_from(Some(8 * 1024 * MIB as u64)),
            1024 * MIB
        );
        assert_eq!(
            auto_read_cache_from(Some(64 * 1024 * MIB as u64)),
            2 * 1024 * MIB
        );
        assert!(recommended_write_back_bytes().is_multiple_of(64 * MIB));
    }

    #[test]
    fn lru_enforces_byte_budget_and_touches_hits() {
        let cache = Cache::new(CacheConfig {
            read_capacity_bytes: SEGMENT_BYTES * 2 + 1024,
            ..CacheConfig::default()
        });
        let mut state = cache.lock();
        let data = vec![1; SEGMENT_BYTES];
        state.insert(0, &data);
        state.insert(SEGMENT_BYTES as u64, &data);
        assert!(state.get(0).is_some());
        state.insert((SEGMENT_BYTES * 2) as u64, &data);
        assert!(state.contains(0));
        assert!(!state.contains(SEGMENT_BYTES as u64));
        assert_eq!(state.clean.len() * SEGMENT_BYTES, SEGMENT_BYTES * 2);
        assert_eq!(state.stats.evictions, 1);
        state.insert(0, &[1; 512]);
        assert_eq!(state.clean.len(), 2);
    }

    #[test]
    fn overlap_invalidation_only_removes_affected_segments() {
        let cache = Cache::new(CacheConfig {
            read_capacity_bytes: SEGMENT_BYTES * 4,
            ..CacheConfig::default()
        });
        let mut state = cache.lock();
        for index in 0..4 {
            state.insert(index * SEGMENT_BYTES as u64, &vec![0; SEGMENT_BYTES]);
        }
        state.invalidate((SEGMENT_BYTES * 2 - 512) as u64, 1024);
        assert!(state.contains(0));
        assert!(!state.contains(SEGMENT_BYTES as u64));
        assert!(!state.contains((SEGMENT_BYTES * 2) as u64));
        assert!(state.contains((SEGMENT_BYTES * 3) as u64));
    }

    #[test]
    fn dirty_replacement_budget_and_reconfiguration_are_lossless() {
        let cache = Cache::new(CacheConfig {
            write_back_capacity_bytes: SEGMENT_BYTES,
            ..CacheConfig::default()
        });
        let mut state = cache.lock();
        state.stage(0, &vec![1; SEGMENT_BYTES]);
        state.stage(0, &vec![2; SEGMENT_BYTES]);
        assert_eq!(state.stats.dirty_bytes, SEGMENT_BYTES);
        assert!(state.configure(CacheConfig::default()).is_err());
        assert_eq!(&*state.get(0).unwrap(), vec![2; SEGMENT_BYTES]);
        state.flushed();
        state.configure(CacheConfig::default()).unwrap();
        drop(state);
        assert_eq!(cache.stats(), CacheStats::default());
    }
}
