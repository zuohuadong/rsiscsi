use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Default)]
pub(crate) struct Stats {
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub commands: AtomicU64,
    pub errors: AtomicU64,
    /// Unit-attention retries, which are not failures.
    pub retries: AtomicU64,
}

/// Monotonic payload counters plus lifetime rates.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct StatsSnapshot {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub commands: u64,
    pub errors: u64,
    pub retries: u64,
    pub elapsed_seconds: f64,
    pub iops: f64,
    /// Payload bytes/sec, both directions.
    pub throughput: f64,
}

impl Stats {
    pub fn snapshot(&self, start: Instant) -> StatsSnapshot {
        let seconds = start.elapsed().as_secs_f64().max(f64::EPSILON);
        let bytes_read = self.bytes_read.load(Ordering::Relaxed);
        let bytes_written = self.bytes_written.load(Ordering::Relaxed);
        let commands = self.commands.load(Ordering::Relaxed);
        StatsSnapshot {
            bytes_read,
            bytes_written,
            commands,
            errors: self.errors.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            elapsed_seconds: seconds,
            iops: commands as f64 / seconds,
            throughput: (bytes_read + bytes_written) as f64 / seconds,
        }
    }
}

/// Stateful live-rate sampler for a GUI, sampled approximately once per second.
pub struct SessionStats {
    source: Arc<Stats>,
    start: Instant,
    last: StatsSnapshot,
    pub iops: f64,
    pub mib_per_second: f64,
    initialized: bool,
}

impl SessionStats {
    pub(crate) fn new(source: Arc<Stats>, start: Instant) -> Self {
        let last = source.snapshot(start);
        Self {
            source,
            start,
            last,
            iops: 0.0,
            mib_per_second: 0.0,
            initialized: false,
        }
    }

    /// EWMA with a three-second time constant. Returns the raw cumulative sample.
    pub fn sample(&mut self) -> StatsSnapshot {
        let now = self.source.snapshot(self.start);
        let dt = now.elapsed_seconds - self.last.elapsed_seconds;
        if dt > 0.0 {
            let alpha = if self.initialized {
                1.0 - (-dt / 3.0).exp()
            } else {
                1.0
            };
            let iops = now.commands.saturating_sub(self.last.commands) as f64 / dt;
            let bytes = now.bytes_read.saturating_sub(self.last.bytes_read)
                + now.bytes_written.saturating_sub(self.last.bytes_written);
            self.iops += alpha * (iops - self.iops);
            self.mib_per_second += alpha * (bytes as f64 / dt / 1_048_576.0 - self.mib_per_second);
            self.initialized = true;
            self.last = now;
        }
        now
    }
}
