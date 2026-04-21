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
