//! Resource metrics for ticks, collected on a thread of their own.
//!
//! The sampler owns a long-lived `sysinfo::System` and the NVML handle and
//! takes a reading every `sampler_interval_ms`, publishing the latest one
//! with its capture time. The tick loop only ever copies that slot (the
//! lock is never held while measuring), so a slow GPU query can delay the
//! next sample but never a tick: the tick goes out regardless, without
//! metrics if the newest sample is too old.

use log::info;
use nvml_wrapper::Nvml;
use protocol::{GpuMetrics, Metrics};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::Thread;
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// One reading, without the timing that turns it into tick `Metrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub cpu_percent: f32,
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub gpus: Vec<GpuMetrics>,
}

/// Where readings come from: the real machine, or a stand-in in tests.
/// `read` may block for as long as it likes; returning `None` publishes
/// nothing this round.
pub trait Source: Send + 'static {
    fn read(&mut self) -> Option<Reading>;
}

#[derive(Debug, Clone)]
struct Snapshot {
    /// When this reading started, so its age includes however long the
    /// measurement itself took.
    captured_at: Instant,
    reading: Reading,
}

struct Shared {
    latest: Mutex<Option<Snapshot>>,
    interval_ms: AtomicU64,
}

/// Handle to the sampler thread. Cheap to clone; the thread exits once
/// every handle is gone.
#[derive(Clone)]
pub struct Sampler {
    shared: Arc<Shared>,
    thread: Thread,
}

impl Sampler {
    /// Samples this machine. Front ends start it at launch with the
    /// compiled default interval; `welcome` then sets the server's.
    pub fn spawn(interval: Duration) -> Sampler {
        Sampler::spawn_with(interval, SystemSource::new)
    }

    /// `make` runs on the sampler thread, so a slow NVML load never holds
    /// up the caller.
    pub fn spawn_with<S: Source>(interval: Duration, make: impl FnOnce() -> S + Send + 'static) -> Sampler {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            interval_ms: AtomicU64::new(clamp_interval(interval)),
        });
        let weak = Arc::downgrade(&shared);
        let handle = std::thread::Builder::new()
            .name("sampler".into())
            .spawn(move || run(weak, make()))
            .expect("couldn't start the sampler thread");
        Sampler {
            shared,
            thread: handle.thread().clone(),
        }
    }

    /// Takes effect from the next reading, without restarting the thread.
    pub fn set_interval(&self, interval: Duration) {
        let ms = clamp_interval(interval);
        if self.shared.interval_ms.swap(ms, Ordering::Relaxed) != ms {
            info!(target: "metrics", "sampling every {ms} ms");
            self.thread.unpark();
        }
    }

    /// What the next tick should carry: the latest reading, or `None` if
    /// there's none yet or it's older than `max_age` (the sampler stalled).
    pub fn metrics_for_tick(&self, max_age: Duration) -> Option<Metrics> {
        let latest = lock(&self.shared.latest).clone();
        metrics_from(latest.as_ref(), Instant::now(), max_age)
    }
}

fn clamp_interval(interval: Duration) -> u64 {
    (interval.as_millis() as u64).max(protocol::telemetry::MIN_SAMPLER_INTERVAL_MS)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn metrics_from(snapshot: Option<&Snapshot>, now: Instant, max_age: Duration) -> Option<Metrics> {
    let snapshot = snapshot?;
    let age = now.saturating_duration_since(snapshot.captured_at);
    if age > max_age {
        return None;
    }
    let r = snapshot.reading.clone();
    Some(Metrics {
        sample_age_ms: u32::try_from(age.as_millis()).unwrap_or(u32::MAX),
        cpu_percent: r.cpu_percent,
        ram_total_bytes: r.ram_total_bytes,
        ram_available_bytes: r.ram_available_bytes,
        swap_total_bytes: r.swap_total_bytes,
        swap_used_bytes: r.swap_used_bytes,
        gpus: r.gpus,
    })
}

fn run(weak: Weak<Shared>, mut source: impl Source) {
    // When the previous reading started (or the source was created, which
    // takes the first CPU reading).
    let mut previous = Instant::now();
    loop {
        // Waits out the interval. `set_interval` unparks the thread so a new
        // interval applies to the reading being waited for; a spurious
        // wakeup just goes back to waiting, since CPU % needs its readings
        // at least the interval apart.
        let (started, interval) = loop {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let interval = Duration::from_millis(shared.interval_ms.load(Ordering::Relaxed));
            drop(shared);
            let deadline = previous + interval;
            let now = Instant::now();
            if now >= deadline {
                break (now, interval);
            }
            std::thread::park_timeout(deadline - now);
        };
        let reading = source.read();
        let Some(shared) = weak.upgrade() else {
            return;
        };
        if let Some(reading) = reading {
            *lock(&shared.latest) = Some(Snapshot {
                captured_at: started,
                reading,
            });
        }
        // Readings start an interval apart without drifting, but one that
        // overran (a slow GPU query) is followed by a full interval's rest
        // rather than another reading straight away.
        let finished = Instant::now();
        previous = if finished.duration_since(started) > interval {
            finished
        } else {
            started
        };
    }
}

/// CPU and RAM through sysinfo, NVIDIA GPUs through NVML.
struct SystemSource {
    /// Created with a first CPU reading, so CPU % (the difference between
    /// two readings) is valid from the first `read`, which the sampler only
    /// makes at least one interval (200 ms or more) later.
    /// None on an OS sysinfo doesn't support (since 0.40 that's an error
    /// rather than empty numbers): no CPU/RAM readings, GPUs still reported.
    system: Option<System>,
    nvml: Option<Nvml>,
    /// (GPU index, query) pairs whose failure has been logged, so a broken
    /// query logs once rather than every reading.
    logged: HashSet<(u32, &'static str)>,
}

impl SystemSource {
    fn new() -> Self {
        let system = match System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_usage())
                .with_memory(MemoryRefreshKind::everything()),
        ) {
            Ok(system) => Some(system),
            Err(e) => {
                info!(target: "metrics", "sysinfo not available ({e}); no CPU or RAM metrics");
                None
            }
        };
        // Loaded at runtime (libnvidia-ml.so.1 / nvml.dll). Machines without
        // an NVIDIA driver, and containers without the GPU override, end up
        // here: say so once and report no GPUs.
        let nvml = match Nvml::init() {
            Ok(nvml) => Some(nvml),
            Err(e) => {
                info!(target: "metrics", "NVML not available ({e}); reporting CPU and RAM only");
                None
            }
        };
        SystemSource {
            system,
            nvml,
            logged: HashSet::new(),
        }
    }

    fn gpus(&mut self) -> Vec<GpuMetrics> {
        let Some(nvml) = &self.nvml else {
            return Vec::new();
        };
        let logged = &mut self.logged;
        let mut log_once = |index: u32, query: &'static str, e: nvml_wrapper::error::NvmlError| {
            if logged.insert((index, query)) {
                info!(target: "metrics", "GPU {index}: {query} failed ({e}); leaving it out of metrics");
            }
        };
        let count = nvml.device_count().unwrap_or(0);
        (0..count)
            .filter_map(|i| {
                let device = nvml
                    .device_by_index(i)
                    .map_err(|e| log_once(i, "device lookup", e))
                    .ok()?;
                let utilisation = device
                    .utilization_rates()
                    .map_err(|e| log_once(i, "utilisation", e))
                    .ok()?;
                let memory = device.memory_info().map_err(|e| log_once(i, "memory", e)).ok()?;
                Some(GpuMetrics {
                    name: device.name().unwrap_or_else(|_| format!("GPU {i}")),
                    utilisation_percent: utilisation.gpu.min(100) as f32,
                    vram_used_bytes: memory.used.min(memory.total),
                    vram_total_bytes: memory.total,
                    // Unsupported on many cards (and any error here) is just
                    // "no power reading", not a failed sample.
                    power_watts: device.power_usage().ok().map(|milliwatts| milliwatts as f32 / 1000.0),
                })
            })
            .collect()
    }
}

impl Source for SystemSource {
    fn read(&mut self) -> Option<Reading> {
        let system = self.system.as_mut()?;
        system.refresh_cpu_usage();
        // In sysinfo 0.40, available_memory() is MemAvailable on Linux (what
        // can be allocated without swapping, including reclaimable cache)
        // and ullAvailPhys on Windows; used_memory() is total - available.
        //
        // In Docker these are the host's numbers: /proc/meminfo isn't
        // namespaced, so a container memory limit (`mem_limit`) doesn't show
        // up here. That's what we want for a host-wide view; the limit would
        // be in the cgroup files, which we deliberately don't read.
        system.refresh_memory();
        let cpu = system.global_cpu_usage();
        if !cpu.is_finite() {
            return None;
        }
        Some(Reading {
            cpu_percent: cpu.clamp(0.0, 100.0),
            ram_total_bytes: system.total_memory(),
            ram_available_bytes: system.available_memory().min(system.total_memory()),
            swap_total_bytes: Some(system.total_swap()),
            swap_used_bytes: Some(system.used_swap().min(system.total_swap())),
            gpus: self.gpus(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(cpu: f32) -> Reading {
        Reading {
            cpu_percent: cpu,
            ram_total_bytes: 100,
            ram_available_bytes: 60,
            swap_total_bytes: Some(0),
            swap_used_bytes: Some(0),
            gpus: vec![],
        }
    }

    #[test]
    fn tick_metrics_carry_the_sample_age_or_nothing_when_stale() {
        let t0 = Instant::now();
        let snap = Snapshot {
            captured_at: t0,
            reading: reading(42.0),
        };
        let max_age = Duration::from_millis(3_000);
        let m = metrics_from(Some(&snap), t0 + Duration::from_millis(250), max_age).unwrap();
        assert_eq!(m.sample_age_ms, 250);
        assert_eq!(m.cpu_percent, 42.0);
        assert!(metrics_from(Some(&snap), t0 + max_age, max_age).is_some());
        assert!(metrics_from(Some(&snap), t0 + max_age + Duration::from_millis(1), max_age).is_none());
        assert!(metrics_from(None, t0, max_age).is_none());
    }

    struct Counting(u32);

    impl Source for Counting {
        fn read(&mut self) -> Option<Reading> {
            self.0 += 1;
            Some(reading(self.0 as f32))
        }
    }

    #[test]
    fn publishes_readings_and_applies_a_new_interval_live() {
        let sampler = Sampler::spawn_with(Duration::from_secs(3600), || Counting(0));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            sampler.metrics_for_tick(Duration::from_secs(10)).is_none(),
            "an hour-long interval hasn't produced anything yet"
        );
        sampler.set_interval(Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(500));
        let m = sampler
            .metrics_for_tick(Duration::from_secs(10))
            .expect("a reading after the change");
        assert!(m.cpu_percent >= 1.0);
    }

    /// One reading, then a source that hangs, like a wedged GPU driver.
    struct Stalls(bool);

    impl Source for Stalls {
        fn read(&mut self) -> Option<Reading> {
            if std::mem::replace(&mut self.0, true) {
                std::thread::sleep(Duration::from_secs(3600));
            }
            Some(reading(1.0))
        }
    }

    #[test]
    fn a_stalled_source_goes_stale_without_blocking_readers() {
        let sampler = Sampler::spawn_with(Duration::from_millis(200), || Stalls(false));
        std::thread::sleep(Duration::from_millis(300));
        assert!(sampler.metrics_for_tick(Duration::from_secs(10)).is_some());
        let started = Instant::now();
        assert!(sampler.metrics_for_tick(Duration::from_millis(50)).is_none());
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "reading the slot never waits on the source"
        );
    }

    #[test]
    fn real_machine_reports_cpu_and_ram() {
        let mut source = SystemSource::new();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let r = source.read().expect("a reading");
        assert!((0.0..=100.0).contains(&r.cpu_percent));
        assert!(r.ram_total_bytes > 0);
        assert!(r.ram_available_bytes <= r.ram_total_bytes);
    }
}
