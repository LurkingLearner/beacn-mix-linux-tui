//! Live audio level metering for the panel: one `parec` capture per metered
//! source (a channel sink's monitor, or a bound mic), each feeding a reader
//! thread that publishes a smoothed peak. A manager thread reconciles the set
//! of running captures against the desired targets *and* the sources that
//! actually exist right now.
//!
//! Why the existence check matters: `parec` on a source that is missing (or
//! that disappears mid-capture) does not exit — worse, pipewire-pulse can
//! migrate the stream onto the *default* source, so we would silently meter
//! the wrong device. The manager therefore only spawns a capture while its
//! source is present, and kills it as soon as the source vanishes; a dead or
//! stale meter simply reads as level 0 until the source is back.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Capture format: s16 mono at a low rate — plenty for peak metering, cheap on
/// CPU. 50 ms of it is one metering window, matching the panel update cadence.
const SAMPLE_RATE: u32 = 12_000;
const CHUNK_BYTES: usize = (SAMPLE_RATE as usize / 20) * 2; // 50 ms of s16 mono

/// Per-window decay: the published level is `max(window_peak, prev * DECAY)`,
/// so a transient falls back smoothly instead of blinking. This is sqrt(0.75),
/// which preserves the previous decay rate now that windows arrive twice as
/// often (0.866^2 ≈ 0.75 per 100 ms).
const DECAY: f32 = 0.866_025_4;

/// A meter whose reader hasn't delivered a window for this long reads as 0 —
/// covers a capture that stalls without exiting (e.g. its source just died).
const STALE_AFTER_MS: u64 = 700;

/// How often the manager reconciles captures with targets + present sources.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The linear peak corresponding to the bottom of the displayed range
/// (-50 dBFS). Below this the meter shows nothing.
const FLOOR_PEAK: f32 = 0.003_162_3; // 10^(-50/20)
const FLOOR_DB: f32 = -50.0;

/// Map a linear peak (0..=1) to the displayed meter fraction (0..=1) on a
/// dBFS scale over `FLOOR_DB..0` — linear peaks look dead on a gauge (speech
/// at a healthy level barely moves it), dB spread the useful range out.
pub fn display_fraction(peak: f32) -> f32 {
    if peak <= FLOOR_PEAK {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Owns the capture children + reader threads. Dropping it kills every child
/// (the readers then exit on EOF) and stops the manager thread.
pub struct LevelMeter {
    shared: Arc<Shared>,
}

struct Shared {
    /// Zero point for the `last_data` millisecond timestamps.
    epoch: Instant,
    stop: AtomicBool,
    /// Desired sources per channel (mics, or the channel's sink monitor).
    targets: Mutex<[Vec<String>; 4]>,
    /// Live captures, keyed by source name.
    meters: Mutex<HashMap<String, Meter>>,
}

impl Shared {
    fn now_millis(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

/// One running `parec` capture and the level its reader thread publishes.
struct Meter {
    child: Child,
    /// Smoothed linear peak (f32 bits), 0..=1.
    level: Arc<AtomicU32>,
    /// When the reader last delivered a window (ms since `Shared::epoch`).
    last_data: Arc<AtomicU64>,
}

impl LevelMeter {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            epoch: Instant::now(),
            stop: AtomicBool::new(false),
            targets: Mutex::new(std::array::from_fn(|_| Vec::new())),
            meters: Mutex::new(HashMap::new()),
        });
        let mgr = Arc::clone(&shared);
        std::thread::spawn(move || manager_loop(&mgr));
        Self { shared }
    }

    /// Replace the metered sources per channel. The manager reconciles the
    /// running captures within one sweep (~1 s). Cheap to call repeatedly.
    pub fn set_targets(&self, targets: [Vec<String>; 4]) {
        *self.shared.targets.lock().unwrap() = targets;
    }

    /// Current per-channel level: the max linear peak over the channel's
    /// sources. A source with no capture (absent) or a stale reader reads 0.
    pub fn levels(&self) -> [f32; 4] {
        let now = self.shared.now_millis();
        let targets = self.shared.targets.lock().unwrap().clone();
        let meters = self.shared.meters.lock().unwrap();
        std::array::from_fn(|i| {
            targets[i]
                .iter()
                .filter_map(|source| meters.get(source))
                .map(|m| {
                    if now.saturating_sub(m.last_data.load(Ordering::Relaxed)) > STALE_AFTER_MS {
                        0.0
                    } else {
                        f32::from_bits(m.level.load(Ordering::Relaxed))
                    }
                })
                .fold(0.0_f32, f32::max)
        })
    }
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LevelMeter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let mut meters = self.shared.meters.lock().unwrap();
        for (_, mut m) in meters.drain() {
            let _ = m.child.kill();
            let _ = m.child.wait();
        }
    }
}

/// Reconcile running captures against desired targets and present sources.
fn manager_loop(shared: &Arc<Shared>) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return; // Drop already killed the children.
        }

        let desired: HashSet<String> = shared
            .targets
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .cloned()
            .collect();

        // What exists right now (monitors included). On a pactl failure skip
        // the sweep rather than treating everything as vanished.
        match crate::pw::source_names_short() {
            Ok(names) => {
                let existing: HashSet<String> = names.into_iter().collect();
                let mut meters = shared.meters.lock().unwrap();

                // Kill captures that are unwanted, whose source vanished, or
                // whose child exited on its own (respawned next sweep).
                meters.retain(|source, m| {
                    let exited = m.child.try_wait().map(|s| s.is_some()).unwrap_or(true);
                    let keep = !exited && desired.contains(source) && existing.contains(source);
                    if !keep {
                        let _ = m.child.kill();
                        let _ = m.child.wait();
                        log::debug!("meter '{source}' stopped");
                    }
                    keep
                });

                // Start captures for wanted sources that are present.
                for source in &desired {
                    if existing.contains(source) && !meters.contains_key(source) {
                        match spawn_meter(shared, source) {
                            Ok(m) => {
                                log::debug!("meter '{source}' started");
                                meters.insert(source.clone(), m);
                            }
                            Err(e) => log::debug!("meter '{source}' spawn failed: {e}"),
                        }
                    }
                }
            }
            Err(e) => log::debug!("meter sweep: listing sources failed: {e}"),
        }

        std::thread::sleep(SWEEP_INTERVAL);
    }
}

/// Spawn one `parec` capture + its reader thread.
fn spawn_meter(shared: &Arc<Shared>, source: &str) -> std::io::Result<Meter> {
    let mut child = Command::new("parec")
        .arg("--raw")
        .arg(format!("--device={source}"))
        .arg("--format=s16le")
        .arg(format!("--rate={SAMPLE_RATE}"))
        .arg("--channels=1")
        // Ask Pulse to deliver data before a complete 50 ms meter window has
        // accumulated; this avoids adding another full display interval of
        // capture buffering ahead of the reader.
        .arg("--latency-msec=25")
        // Label the stream so it's identifiable in pavucontrol & co.
        .env("PULSE_PROP", "application.name=beacn-mix-meter")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let level = Arc::new(AtomicU32::new(0));
    let last_data = Arc::new(AtomicU64::new(0));
    let (lvl, last, epoch) = (Arc::clone(&level), Arc::clone(&last_data), shared.epoch);
    std::thread::spawn(move || reader_loop(stdout, epoch, &lvl, &last));
    Ok(Meter {
        child,
        level,
        last_data,
    })
}

/// Read 50 ms windows until EOF (child killed / source died), publishing the
/// smoothed peak after each one.
fn reader_loop(mut stdout: ChildStdout, epoch: Instant, level: &AtomicU32, last_data: &AtomicU64) {
    let mut buf = [0u8; CHUNK_BYTES];
    let mut smoothed = 0.0_f32;
    while stdout.read_exact(&mut buf).is_ok() {
        smoothed = chunk_peak(&buf).max(smoothed * DECAY);
        level.store(smoothed.to_bits(), Ordering::Relaxed);
        last_data.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// Max absolute sample of a raw s16le chunk, as a linear 0..=1 peak.
fn chunk_peak(buf: &[u8]) -> f32 {
    buf.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
        .max()
        .unwrap_or(0) as f32
        / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_peak_finds_max_abs_sample() {
        // Samples: 0, +1000, -2000 → peak 2000/32768.
        let mut buf = Vec::new();
        for s in [0i16, 1000, -2000] {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        let p = chunk_peak(&buf);
        assert!((p - 2000.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn chunk_peak_of_silence_is_zero() {
        assert_eq!(chunk_peak(&[0u8; 64]), 0.0);
        assert_eq!(chunk_peak(&[]), 0.0);
    }

    #[test]
    fn chunk_peak_handles_i16_min_without_overflow() {
        let buf = i16::MIN.to_le_bytes();
        assert!((chunk_peak(&buf) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn display_fraction_maps_dbfs_range() {
        // Silence and everything below -50 dBFS → 0.
        assert_eq!(display_fraction(0.0), 0.0);
        assert_eq!(display_fraction(0.001), 0.0);
        // Full scale → 1.
        assert!((display_fraction(1.0) - 1.0).abs() < 1e-6);
        // -25 dBFS (peak ≈ 0.0562) → half the range.
        let half = display_fraction(0.056_234);
        assert!((half - 0.5).abs() < 0.01, "got {half}");
        // Monotonic.
        assert!(display_fraction(0.1) < display_fraction(0.5));
    }
}
