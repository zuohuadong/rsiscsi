use crate::{Error, Session};
use hdrhistogram::Histogram;
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Barrier, task::JoinSet};

/// Access pattern. Writes destroy the selected remote range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchPattern {
    SeqRead,
    RandRead,
    SeqWrite,
    RandWrite,
}

impl BenchPattern {
    pub fn is_write(self) -> bool {
        matches!(self, Self::SeqWrite | Self::RandWrite)
    }
    fn is_random(self) -> bool {
        matches!(self, Self::RandRead | Self::RandWrite)
    }
}

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub pattern: BenchPattern,
    /// Transfer size in bytes, must be a multiple of the logical block size.
    pub block_size: usize,
    pub queue_depths: Vec<usize>,
    pub duration: Duration,
    pub warmup: Duration,
    pub start_lba: u64,
    /// Bounded region in logical blocks; defaults to the remaining LUN for reads.
    pub range_blocks: Option<u64>,
    /// Required together with an explicit region for any write benchmark.
    pub allow_destructive_write: bool,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            pattern: BenchPattern::SeqRead,
            block_size: 1024 * 1024,
            queue_depths: vec![16],
            duration: Duration::from_secs(10),
            warmup: Duration::from_secs(1),
            start_lba: 0,
            range_blocks: None,
            allow_destructive_write: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchResult {
    pub queue_depth: usize,
    pub commands: u64,
    pub bytes: u64,
    pub elapsed_seconds: f64,
    pub iops: f64,
    pub mib_per_second: f64,
    pub p50_latency_us: f64,
    pub p95_latency_us: f64,
    pub p99_latency_us: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub pattern: BenchPattern,
    pub block_size: usize,
    pub start_lba: u64,
    pub range_blocks: u64,
    pub warmup_seconds: f64,
    /// Write results exclude the subsequent SYNCHRONIZE CACHE duration.
    pub flush_seconds: f64,
    pub results: Vec<BenchResult>,
}

/// Benchmark a bounded region with bounded-memory HDR latency histograms.
///
/// All workers are joined before returning, including on error. Warmup I/O and
/// allocations are excluded; outstanding measured I/O is drained and included
/// in elapsed time. Writes require an explicit destructive opt-in and range.
pub async fn benchmark(session: &Session, opts: BenchOptions) -> Result<BenchReport, Error> {
    if opts.pattern.is_write() && (!opts.allow_destructive_write || opts.range_blocks.is_none()) {
        return Err(Error::Config(
            "write benchmark requires destructive opt-in and explicit range_blocks".into(),
        ));
    }
    if opts.duration.is_zero()
        || opts.queue_depths.is_empty()
        || opts
            .queue_depths
            .iter()
            .any(|q| *q == 0 || *q > session.queue_depth())
        || opts.block_size == 0
        || opts.block_size > u32::MAX as usize
    {
        return Err(Error::Config(
            "invalid benchmark size, duration or queue depth".into(),
        ));
    }
    let capacity = session.capacity().await?;
    if !opts.block_size.is_multiple_of(capacity.block_size as usize) {
        return Err(Error::Config(
            "benchmark size is not a whole logical block".into(),
        ));
    }
    let blocks = (opts.block_size / capacity.block_size as usize) as u32;
    let remaining = capacity
        .blocks
        .checked_sub(opts.start_lba)
        .ok_or_else(|| Error::Config("benchmark starts past end of LUN".into()))?;
    let range = opts.range_blocks.unwrap_or(remaining);
    if range < u64::from(blocks) || range > remaining {
        return Err(Error::Config(
            "benchmark range exceeds LUN or is too small".into(),
        ));
    }
    let mut report = BenchReport {
        pattern: opts.pattern,
        block_size: opts.block_size,
        start_lba: opts.start_lba,
        range_blocks: range,
        warmup_seconds: opts.warmup.as_secs_f64(),
        flush_seconds: 0.0,
        results: Vec::new(),
    };
    for &qd in &opts.queue_depths {
        if !opts.warmup.is_zero() {
            phase(session, &opts, blocks, range, qd, opts.warmup, false).await?;
        }
        let result = phase(session, &opts, blocks, range, qd, opts.duration, true).await?;
        report.results.push(result);
        if opts.pattern.is_write() {
            let start = Instant::now();
            session.flush().await?;
            report.flush_seconds += start.elapsed().as_secs_f64();
        }
    }
    Ok(report)
}

async fn phase(
    session: &Session,
    opts: &BenchOptions,
    blocks: u32,
    range: u64,
    qd: usize,
    duration: Duration,
    measure: bool,
) -> Result<BenchResult, Error> {
    let barrier = Arc::new(Barrier::new(qd + 1));
    let clock = Arc::new(OnceLock::<Instant>::new());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cursor = Arc::new(AtomicU64::new(0));
    let slots = range / u64::from(blocks);
    let mut workers = JoinSet::new();
    // Preparation happens before the barrier so it cannot consume measurement time.
    for _ in 0..qd {
        let mut buffer = session.buffer_pool().acquire(opts.block_size);
        if opts.pattern.is_write() {
            rand::thread_rng().fill(&mut *buffer);
        }
        let mut histogram = Histogram::<u64>::new(3).map_err(|e| Error::Config(e.to_string()))?;
        let session = session.clone();
        let barrier = barrier.clone();
        let cursor = cursor.clone();
        let stop = stop.clone();
        let clock = clock.clone();
        let pattern = opts.pattern;
        let start_lba = opts.start_lba;
        workers.spawn(async move {
            let mut rng = StdRng::from_entropy();
            barrier.wait().await;
            let start = *clock.get_or_init(Instant::now);
            let mut finished = start;
            let mut count = 0u64;
            while start.elapsed() < duration && !stop.load(Ordering::Relaxed) {
                let slot = if pattern.is_random() {
                    rng.gen_range(0..slots)
                } else {
                    cursor.fetch_add(1, Ordering::Relaxed) % slots
                };
                let lba = start_lba + slot * u64::from(blocks);
                let before = Instant::now();
                let result = if pattern.is_write() {
                    session.write_buffer(lba, buffer).await
                } else {
                    session.read_buffer(lba, blocks, buffer).await
                };
                buffer = match result {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        stop.store(true, Ordering::Relaxed);
                        return Err(error);
                    }
                };
                finished = Instant::now();
                count += 1;
                if measure {
                    histogram
                        .record(
                            finished
                                .duration_since(before)
                                .as_nanos()
                                .min(u64::MAX as u128) as u64,
                        )
                        .map_err(|e| Error::Protocol(e.to_string()))?;
                }
            }
            Ok((count, histogram, finished))
        });
    }
    barrier.wait().await;
    let start = *clock.get_or_init(Instant::now);
    let mut count = 0;
    let mut histogram = Histogram::<u64>::new(3).map_err(|e| Error::Config(e.to_string()))?;
    let mut failure = None;
    let mut finished = start;
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(Ok((n, h, end))) => {
                count += n;
                finished = finished.max(end);
                if let Err(error) = histogram.add(h) {
                    failure.get_or_insert(Error::Protocol(error.to_string()));
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Ok(Err(error)) => {
                failure.get_or_insert(error);
            }
            Err(error) => {
                failure.get_or_insert(Error::Protocol(format!("benchmark worker: {error}")));
                stop.store(true, Ordering::Relaxed);
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let seconds = measured_seconds(duration, finished.duration_since(start));
    let bytes = count
        .checked_mul(opts.block_size as u64)
        .ok_or_else(|| Error::Protocol("benchmark byte count overflow".into()))?;
    Ok(BenchResult {
        queue_depth: qd,
        commands: count,
        bytes,
        elapsed_seconds: seconds,
        iops: count as f64 / seconds,
        mib_per_second: bytes as f64 / seconds / 1_048_576.0,
        p50_latency_us: histogram.value_at_quantile(0.50) as f64 / 1000.0,
        p95_latency_us: histogram.value_at_quantile(0.95) as f64 / 1000.0,
        p99_latency_us: histogram.value_at_quantile(0.99) as f64 / 1000.0,
    })
}

fn measured_seconds(window: Duration, last_completion: Duration) -> f64 {
    // A scheduling pause after the last completion can cross the deadline.
    // Keep that tail in the denominator without counting histogram aggregation.
    window.max(last_completion).as_secs_f64().max(f64::EPSILON)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measurement_includes_deadline_tail_and_outstanding_io_drain() {
        let window = Duration::from_millis(25);
        // Last completion at 24 ms, next loop check delayed beyond 25 ms.
        assert_eq!(measured_seconds(window, Duration::from_millis(24)), 0.025);
        assert_eq!(measured_seconds(window, Duration::from_millis(30)), 0.030);
    }
}
