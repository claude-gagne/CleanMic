//! Lock-free single-producer single-consumer (SPSC) ring buffer for f32 audio
//! samples.
//!
//! Designed for real-time audio: no allocations after construction, no locks,
//! no syscalls. Uses `AtomicUsize` for the read/write cursors with
//! `Acquire`/`Release` ordering to guarantee visibility across threads.
//!
//! The buffer holds up to `capacity - 1` samples (one slot is always empty to
//! distinguish full from empty).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared state between [`RingBufWriter`] and [`RingBufReader`].
struct RingBufInner {
    buf: Box<[f32]>,
    /// Write cursor (only modified by the writer).
    write_pos: AtomicUsize,
    /// Read cursor (only modified by the reader).
    read_pos: AtomicUsize,
    /// Total allocated slots (always a power of two for fast modular arithmetic).
    capacity: usize,
}

/// Producer half of the SPSC ring buffer.
///
/// Only one thread may hold a `RingBufWriter` at a time.
pub struct RingBufWriter {
    inner: Arc<RingBufInner>,
}

/// Consumer half of the SPSC ring buffer.
///
/// Only one thread may hold a `RingBufReader` at a time.
pub struct RingBufReader {
    inner: Arc<RingBufInner>,
}

// SAFETY: Each half is used by exactly one thread. The atomic cursors provide
// the necessary synchronization.
unsafe impl Send for RingBufWriter {}
unsafe impl Send for RingBufReader {}

/// Create a new SPSC ring buffer pair with room for at least `min_capacity`
/// samples.
///
/// The actual capacity is rounded up to the next power of two. Returns a
/// `(writer, reader)` pair.
pub fn ring_buffer(min_capacity: usize) -> (RingBufWriter, RingBufReader) {
    // Round up to next power of two (minimum 2 so there is at least 1 usable
    // slot).
    let capacity = min_capacity.next_power_of_two().max(2);
    let buf = vec![0.0f32; capacity].into_boxed_slice();

    let inner = Arc::new(RingBufInner {
        buf,
        write_pos: AtomicUsize::new(0),
        read_pos: AtomicUsize::new(0),
        capacity,
    });

    (
        RingBufWriter {
            inner: Arc::clone(&inner),
        },
        RingBufReader { inner },
    )
}

impl RingBufWriter {
    /// Write samples into the ring buffer, returning the number of samples
    /// actually written (may be less than `samples.len()` if the buffer is
    /// full).
    ///
    /// This is safe to call from an RT thread: no allocations, no locks.
    pub fn write(&self, samples: &[f32]) -> usize {
        let inner = &*self.inner;
        let mask = inner.capacity - 1; // works because capacity is power of two
        let w = inner.write_pos.load(Ordering::Relaxed);
        let r = inner.read_pos.load(Ordering::Acquire);

        // Available space: capacity - 1 - (w - r) mod capacity
        let used = w.wrapping_sub(r) & mask;
        let free = inner.capacity - 1 - used;
        let n = samples.len().min(free);

        // SAFETY: We are the sole writer. We write `n` slots starting at `w`
        // (modulo capacity) and only then advance write_pos with Release so the
        // reader sees the new data.
        //
        // We use a raw pointer cast to bypass the shared reference immutability
        // of `inner.buf`. This is safe because:
        // 1. Writer and reader never access the same index simultaneously
        //    (guaranteed by the cursor protocol).
        // 2. Only one writer exists.
        let buf_ptr = inner.buf.as_ptr() as *mut f32;
        for (i, &sample) in samples.iter().enumerate().take(n) {
            let idx = (w + i) & mask;
            // SAFETY: idx < capacity, buf_ptr points to capacity elements.
            unsafe {
                buf_ptr.add(idx).write(sample);
            }
        }

        inner.write_pos.store((w + n) & mask, Ordering::Release);
        n
    }

    /// Number of samples currently available for reading.
    #[cfg(test)]
    pub fn available(&self) -> usize {
        let inner = &*self.inner;
        let mask = inner.capacity - 1;
        let w = inner.write_pos.load(Ordering::Relaxed);
        let r = inner.read_pos.load(Ordering::Acquire);
        w.wrapping_sub(r) & mask
    }
}

impl RingBufReader {
    /// Read up to `output.len()` samples from the ring buffer, returning the
    /// number actually read. Unread slots in `output` are untouched.
    ///
    /// This is safe to call from an RT thread: no allocations, no locks.
    pub fn read(&self, output: &mut [f32]) -> usize {
        let inner = &*self.inner;
        let mask = inner.capacity - 1;
        let r = inner.read_pos.load(Ordering::Relaxed);
        let w = inner.write_pos.load(Ordering::Acquire);

        let available = w.wrapping_sub(r) & mask;
        let n = output.len().min(available);

        for (i, slot) in output.iter_mut().enumerate().take(n) {
            let idx = (r + i) & mask;
            *slot = inner.buf[idx];
        }

        inner.read_pos.store((r + n) & mask, Ordering::Release);
        n
    }

    /// Number of samples currently available for reading.
    pub fn available(&self) -> usize {
        let inner = &*self.inner;
        let mask = inner.capacity - 1;
        let r = inner.read_pos.load(Ordering::Relaxed);
        let w = inner.write_pos.load(Ordering::Acquire);
        w.wrapping_sub(r) & mask
    }

    /// Drop up to `n` of the OLDEST queued samples without copying them,
    /// returning how many were dropped.
    ///
    /// SPSC-safe: only the reader ever moves `read_pos`, and we never move it
    /// past the write cursor we observed. RT-safe: two atomics, no allocation.
    pub fn discard(&self, n: usize) -> usize {
        let inner = &*self.inner;
        let mask = inner.capacity - 1;
        let r = inner.read_pos.load(Ordering::Relaxed);
        let w = inner.write_pos.load(Ordering::Acquire);
        let n = n.min(w.wrapping_sub(r) & mask);
        inner.read_pos.store((r + n) & mask, Ordering::Release);
        n
    }

    /// Drop everything currently queued (skip to the writer's cursor),
    /// returning how many samples were dropped.
    pub fn discard_all(&self) -> usize {
        self.discard(usize::MAX)
    }
}

/// Observation window of [`BacklogLimiter`], in samples requested by the
/// consumer (1 s at 48 kHz).
const LIMITER_WINDOW: usize = 48_000;
/// Standing backlog tolerated before [`BacklogLimiter`] sheds (20 ms).
///
/// Whatever stands below this is added to every call's latency for as long as
/// the stream runs, so it is kept small: a late consumer start (the audio
/// thread runs ~60 ms before the output stream streams), an engine-swap
/// stall or a shrinking quantum routinely leave 15-30 ms of slack, which a
/// 50 ms tolerance used to keep forever.
const LIMITER_MAX_STANDING: usize = 960;
/// Backlog [`BacklogLimiter`] leaves in place after shedding, as jitter
/// headroom (10 ms) on top of the worst case seen in the last window.
const LIMITER_KEEP: usize = 480;

/// Consumer-side latency bound for a ring that bridges two independently
/// clocked PipeWire graphs (e.g. the ALSA-mic-driven audio thread feeding the
/// CleanMic-null-sink-driven output stream, or the Bluetooth-sink-driven
/// monitor stream).
///
/// Without it, any transient that lets the producer run ahead (a stalled
/// audio thread catching up, clock drift) leaves a backlog that both ends
/// then preserve forever, because each runs at exactly real time — latency
/// only ever ratchets up, until the ring saturates (65535 samples = 1.37 s).
///
/// It sheds only *standing* backlog: the minimum fill seen right after each
/// read over a whole window. Normal burstiness (a producer delivering a full
/// PipeWire quantum at once, a consumer pulling a large buffer) raises the
/// peaks, not that minimum, so it never trims healthy jitter headroom. And it
/// cannot oscillate: if shedding ever leaves too little headroom, the next
/// underrun drives the window minimum to ~0, which never triggers a shed.
pub struct BacklogLimiter {
    window: usize,
    max_standing: usize,
    keep: usize,
    /// Samples requested by the consumer since the window started.
    elapsed: usize,
    /// Lowest post-read fill seen in the current window.
    min_fill: usize,
    /// Total samples shed so far (diagnostics/tests).
    shed_total: usize,
}

impl BacklogLimiter {
    /// Create a limiter with the production tuning (1 s window, shed when
    /// more than 20 ms stood unused for the whole window, keep 10 ms).
    pub fn new() -> Self {
        Self::with_params(LIMITER_WINDOW, LIMITER_MAX_STANDING, LIMITER_KEEP)
    }

    /// Create a limiter with explicit tuning (all values in samples).
    pub fn with_params(window: usize, max_standing: usize, keep: usize) -> Self {
        assert!(window > 0 && keep <= max_standing);
        Self {
            window,
            max_standing,
            keep,
            elapsed: 0,
            min_fill: usize::MAX,
            shed_total: 0,
        }
    }

    /// Consumer read: fill `out` from `reader`, zero-pad any shortfall
    /// (underrun), shed standing backlog if a window just completed, and
    /// return the number of real samples read.
    ///
    /// On an underrun (less than `out.len()` queued) with more than `keep`
    /// queued, `keep` samples are left in the ring: the gap is `keep` longer,
    /// but playback resumes with that much headroom. Without it, recovery
    /// restored only the exact shortfall, so a producer delivering in
    /// jittery 480-sample blocks (a heavy engine) sat at zero margin and
    /// every new jitter peak became another burst of small underruns — a
    /// click train instead of one gap. Nothing is dropped: the kept samples
    /// are the next ones played.
    ///
    /// RT-safe: no allocation, no locks, no syscalls.
    pub fn read_padded(&mut self, reader: &RingBufReader, out: &mut [f32]) -> usize {
        let queued = reader.available();
        let want = if queued < out.len() && queued > self.keep {
            queued - self.keep
        } else {
            out.len()
        };
        let read = reader.read(&mut out[..want]);
        for s in &mut out[read..] {
            *s = 0.0;
        }

        self.min_fill = self.min_fill.min(reader.available());
        self.elapsed += out.len();
        if self.elapsed >= self.window {
            let standing = self.min_fill;
            self.elapsed = 0;
            self.min_fill = usize::MAX;
            if standing > self.max_standing {
                // Everything above `standing` was consumed at some point in
                // the window, so dropping `standing - keep` of the oldest
                // samples removes only audio that was never needed in time.
                self.shed_total += reader.discard(standing - self.keep);
            }
        }
        read
    }

    /// Total number of samples shed since creation.
    pub fn shed_total(&self) -> usize {
        self.shed_total
    }
}

impl Default for BacklogLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_read_returns_zero() {
        let (_w, r) = ring_buffer(1024);
        let mut buf = [0.0f32; 64];
        assert_eq!(r.read(&mut buf), 0);
    }

    #[test]
    fn write_then_read_round_trips() {
        let (w, r) = ring_buffer(1024);
        let input: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        assert_eq!(w.write(&input), 100);

        let mut output = vec![0.0f32; 100];
        assert_eq!(r.read(&mut output), 100);
        for (a, b) in input.iter().zip(output.iter()) {
            assert!((a - b).abs() < 1e-9, "mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn write_more_than_capacity_drops_excess() {
        let (w, r) = ring_buffer(8); // rounds up to 8, usable = 7
        let input = [1.0f32; 10];
        let written = w.write(&input);
        assert_eq!(written, 7); // only 7 usable slots

        let mut output = [0.0f32; 10];
        let read = r.read(&mut output);
        assert_eq!(read, 7);
    }

    #[test]
    fn multiple_write_read_cycles() {
        let (w, r) = ring_buffer(64);
        for cycle in 0..20 {
            let val = cycle as f32;
            let input = [val; 16];
            assert_eq!(w.write(&input), 16);

            let mut output = [0.0f32; 16];
            assert_eq!(r.read(&mut output), 16);
            for &s in &output {
                assert!((s - val).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn wrap_around_correctness() {
        let (w, r) = ring_buffer(8); // capacity = 8, usable = 7

        // Fill 5, read 5, fill 5 again (forces wrap-around).
        let input1 = [1.0f32; 5];
        assert_eq!(w.write(&input1), 5);

        let mut out1 = [0.0f32; 5];
        assert_eq!(r.read(&mut out1), 5);

        let input2 = [2.0f32; 5];
        assert_eq!(w.write(&input2), 5);

        let mut out2 = [0.0f32; 5];
        assert_eq!(r.read(&mut out2), 5);
        for &s in &out2 {
            assert!((s - 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn available_reflects_state() {
        let (w, r) = ring_buffer(64);
        assert_eq!(r.available(), 0);

        w.write(&[0.0; 10]);
        assert_eq!(r.available(), 10);

        let mut buf = [0.0f32; 4];
        r.read(&mut buf);
        assert_eq!(r.available(), 6);
    }

    #[test]
    fn discard_drops_oldest_and_is_clamped_to_available() {
        let (w, r) = ring_buffer(64);
        let input: Vec<f32> = (1..=10).map(|i| i as f32).collect();
        w.write(&input);
        assert_eq!(r.discard(4), 4);
        let mut out = [0.0f32; 2];
        r.read(&mut out);
        assert_eq!(out, [5.0, 6.0], "discard must drop the OLDEST samples");
        assert_eq!(r.discard(100), 4, "discard is clamped to what is queued");
        assert_eq!(r.available(), 0);
    }

    #[test]
    fn discard_all_skips_to_writer_across_wrap() {
        let (w, r) = ring_buffer(8); // usable 7
        w.write(&[1.0; 5]);
        let mut out = [0.0f32; 5];
        r.read(&mut out);
        w.write(&[2.0; 6]); // wraps
        assert_eq!(r.discard_all(), 6);
        assert_eq!(r.available(), 0);
        w.write(&[3.0; 3]);
        let mut out = [0.0f32; 3];
        assert_eq!(r.read(&mut out), 3);
        assert_eq!(out, [3.0; 3], "ring stays coherent after discard_all");
    }

    /// Drive `limiter` for `callbacks` consumer reads of `read_len` samples,
    /// with the producer adding `produce(i)` samples before read `i`.
    fn drive(
        limiter: &mut BacklogLimiter,
        w: &RingBufWriter,
        r: &RingBufReader,
        read_len: usize,
        callbacks: usize,
        produce: impl Fn(usize) -> usize,
    ) -> usize {
        let mut out = vec![0.0f32; read_len];
        let mut underruns = 0;
        for i in 0..callbacks {
            w.write(&vec![0.5f32; produce(i)]);
            if limiter.read_padded(r, &mut out) < read_len {
                underruns += 1;
            }
        }
        underruns
    }

    #[test]
    fn limiter_read_padded_zero_fills_underrun() {
        let (w, r) = ring_buffer(64);
        let mut limiter = BacklogLimiter::new();
        w.write(&[1.0; 3]);
        let mut out = [9.0f32; 5];
        assert_eq!(limiter.read_padded(&r, &mut out), 3);
        assert_eq!(out, [1.0, 1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn limiter_sheds_standing_backlog_down_to_keep() {
        // 200 ms standing backlog (a stalled producer that caught up), then a
        // steady producer/consumer at 1024 per callback.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        w.write(&vec![0.5f32; 9_600]);
        let underruns = drive(&mut limiter, &w, &r, 1024, 100, |_| 1024);
        assert_eq!(underruns, 0);
        assert_eq!(limiter.shed_total(), 9_600 - LIMITER_KEEP);
        assert_eq!(r.available(), LIMITER_KEEP);
    }

    #[test]
    fn limiter_sheds_small_standing_slack_left_by_a_late_consumer_start() {
        // The live pipeline's audio thread starts ~57 ms before the output
        // stream begins streaming, and the consumer then settles with ~29 ms
        // (1376 samples) that is never read in time — measured on a real
        // PipeWire graph (debug session base-latency-330ms). A limiter that
        // tolerates that forever adds it to every call's latency.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        w.write(&vec![0.5f32; 1_376]);
        let underruns = drive(&mut limiter, &w, &r, 1024, 100, |_| 1024);
        assert_eq!(underruns, 0, "shedding must never starve the consumer");
        assert_eq!(limiter.shed_total(), 1_376 - LIMITER_KEEP);
        assert_eq!(r.available(), LIMITER_KEEP);
        assert!(
            LIMITER_KEEP <= 480,
            "headroom kept after a shed ({LIMITER_KEEP} samples) must stay within 10 ms"
        );
    }

    #[test]
    fn limiter_underrun_resumes_with_keep_of_headroom() {
        // 1000 queued, the consumer wants 1024: an underrun either way. The
        // limiter must hold `LIMITER_KEEP` back (one slightly longer gap) so
        // playback resumes with headroom instead of running on empty and
        // underrunning again on the next late block.
        let (w, r) = ring_buffer(4096);
        let mut limiter = BacklogLimiter::new();
        w.write(&[0.5f32; 1000]);
        let mut out = [9.0f32; 1024];
        let read = limiter.read_padded(&r, &mut out);
        assert_eq!(read, 1000 - LIMITER_KEEP);
        assert!(out[..read].iter().all(|&s| s == 0.5));
        assert!(out[read..].iter().all(|&s| s == 0.0));
        assert_eq!(
            r.available(),
            LIMITER_KEEP,
            "headroom kept for the next read"
        );
    }

    #[test]
    fn limiter_underrun_with_at_most_keep_queued_plays_it_all() {
        // Boundary neighbours of the hold-back: with `keep` or fewer samples
        // queued there is no headroom to keep — play what there is.
        for queued in [LIMITER_KEEP, LIMITER_KEEP - 1] {
            let (w, r) = ring_buffer(4096);
            let mut limiter = BacklogLimiter::new();
            w.write(&vec![0.5f32; queued]);
            let mut out = [9.0f32; 1024];
            assert_eq!(limiter.read_padded(&r, &mut out), queued);
            assert_eq!(r.available(), 0);
        }
        // One more than `keep`: exactly one real sample, `keep` stay queued.
        let (w, r) = ring_buffer(4096);
        let mut limiter = BacklogLimiter::new();
        w.write(&vec![0.5f32; LIMITER_KEEP + 1]);
        let mut out = [9.0f32; 1024];
        assert_eq!(limiter.read_padded(&r, &mut out), 1);
        assert_eq!(r.available(), LIMITER_KEEP);
    }

    #[test]
    fn limiter_does_not_turn_block_jitter_into_a_train_of_underruns() {
        // A heavy engine (DPDFNet-8, ~7.5 ms per 480-sample block) delivers
        // its output in 480-sample blocks whose completion jitters; the
        // consumer pulls one 1024-frame quantum per callback. Measured live
        // (debug session base-latency-330ms): with zero margin every new
        // jitter peak produced a burst of 64-sample underruns, one per
        // quantum. Simulated in sample time: block k is ready at
        // (k + 1) * 480 + a deterministic pseudo-random delay of 0 or 470.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        let mut out = vec![0.0f32; 1024];
        let block = [0.5f32; 480];
        let mut lcg: u32 = 12_345;
        let mut next_block_ready = 480u64;
        let mut underrun_events = 0;
        for cb in 1..=5_000u64 {
            let now = cb * 1024;
            while next_block_ready <= now {
                w.write(&block);
                lcg = lcg.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let jitter = if (lcg >> 16) % 8 == 0 { 470 } else { 0 };
                next_block_ready = next_block_ready - (next_block_ready % 480) + 480 + jitter;
            }
            if limiter.read_padded(&r, &mut out) < out.len() {
                underrun_events += 1;
            }
        }
        assert!(
            underrun_events <= 3,
            "{underrun_events} underruns: block jitter must be absorbed by kept headroom, not replayed as a click train"
        );
    }

    #[test]
    fn limiter_ignores_bursty_but_healthy_traffic() {
        // Producer delivers 8192 at once every 8th callback (large PipeWire
        // quantum), consumer pulls 1024: fill swings 0..8192 but nothing
        // stands unused — must never shed.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        drive(&mut limiter, &w, &r, 1024, 2_000, |i| {
            if i % 8 == 0 { 8192 } else { 0 }
        });
        assert_eq!(limiter.shed_total(), 0);

        // Mirror case: consumer pulls a whole 8192-sample buffer every 8th
        // quantum (live callbacks read `maxsize`), producer trickles 1024 per
        // quantum, consumer phase offset by 3 quanta.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        let mut out = vec![0.0f32; 8192];
        let mut underruns = 0;
        for q in 0..4_000 {
            w.write(&[0.5f32; 1024]);
            if q % 8 == 3 && limiter.read_padded(&r, &mut out) < out.len() {
                underruns += 1;
            }
        }
        assert_eq!(limiter.shed_total(), 0);
        assert_eq!(
            underruns, 1,
            "only the very first (phase-offset) read is short"
        );
    }

    #[test]
    fn limiter_does_not_shed_while_underrunning() {
        // Producer slower than consumer (drift in the draining direction, or
        // pipeline stopped): the ring keeps running dry — never shed.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        let underruns = drive(&mut limiter, &w, &r, 1024, 500, |i| {
            if i % 10 == 0 { 0 } else { 1024 }
        });
        assert!(underruns > 0);
        assert_eq!(limiter.shed_total(), 0);
    }

    #[test]
    fn limiter_bounds_drift_accumulation() {
        // Producer 1% faster than consumer: without shedding the fill would
        // grow by ~10 samples per callback forever.
        let (w, r) = ring_buffer(65_536);
        let mut limiter = BacklogLimiter::new();
        drive(&mut limiter, &w, &r, 1000, 20_000, |_| 1010);
        assert!(
            r.available() <= LIMITER_MAX_STANDING + 1010,
            "fill {} not bounded",
            r.available()
        );
        assert!(limiter.shed_total() > 0);
    }

    #[test]
    fn cross_thread_smoke_test() {
        let (w, r) = ring_buffer(4096);
        let n = 10_000usize;

        let writer = std::thread::spawn(move || {
            let mut written = 0;
            let chunk = [0.5f32; 64];
            while written < n {
                let w_count = w.write(&chunk[..64.min(n - written)]);
                written += w_count;
                if w_count == 0 {
                    std::thread::yield_now();
                }
            }
        });

        let reader = std::thread::spawn(move || {
            let mut total_read = 0;
            let mut chunk = [0.0f32; 64];
            while total_read < n {
                let r_count = r.read(&mut chunk);
                for &s in &chunk[..r_count] {
                    assert!((s - 0.5).abs() < 1e-6);
                }
                total_read += r_count;
                if r_count == 0 {
                    std::thread::yield_now();
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
