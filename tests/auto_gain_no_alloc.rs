//! Proves `AutoGain` never allocates on its hot path.
//!
//! Uses a test-only counting `#[global_allocator]` wrapping `std::alloc::System`
//! (every call — alloc/dealloc/realloc/alloc_zeroed — delegates unchanged and
//! only increments a thread-local counter while that counter is "armed" for
//! the current test thread). A global allocator is the only way to observe
//! allocations from *inside* a black-box call like `process_in_place` without
//! instrumenting `AutoGain` itself, and it is confined to this integration
//! test binary — never linked into the shipped `cleanmic` binary (see
//! `CLAUDE.md`'s unsafe-code list; this is a fifth, test-only entry). Every
//! method just forwards to `System`, so this binary's own allocations (the
//! test harness itself, `Vec` buffers built before arming) behave completely
//! normally; only the *counting* is gated on the armed flag.
//!
//! Run: `cargo test --all-features --test auto_gain_no_alloc`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use cleanmic::dsp::AutoGain;

/// Counting allocator: delegates every call to [`System`], incrementing a
/// thread-local counter while [`ARMED`] is set for the calling thread.
struct CountingAllocator;

thread_local! {
    /// Whether the current thread is being measured. `const`-initialized so
    /// this cell itself never allocates to come into existence.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Number of allocator calls observed while armed on this thread.
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Set the armed flag for the calling thread and reset its counter to zero.
fn arm() {
    ARMED.with(|a| a.set(true));
    COUNT.with(|c| c.set(0));
}

/// Clear the armed flag for the calling thread and return the final count.
fn disarm() -> u64 {
    ARMED.with(|a| a.set(false));
    COUNT.with(|c| c.get())
}

fn bump() {
    // `try_with` so a call happening during thread teardown (TLS destroyed)
    // never panics inside an allocator hook.
    let _ = ARMED.try_with(|a| {
        if a.get() {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump();
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// 10 s of 48 kHz audio in 480-sample (10 ms) chunks, exercising
/// `process_in_place`, `set_enabled(false)`, `set_enabled(true)` and
/// `reset()` — all with zero heap allocations while armed.
#[test]
fn process_in_place_never_allocates() {
    const SAMPLE_RATE: u32 = 48_000;
    const BLOCK: usize = 480;
    const SECONDS: usize = 10;

    // Pre-allocate everything BEFORE arming, so only the code under test is
    // measured.
    let mut ag = AutoGain::new(SAMPLE_RATE);
    let mut buf = [0.037f32; BLOCK];
    let total_blocks = SAMPLE_RATE as usize / BLOCK * SECONDS;

    arm();
    for i in 0..total_blocks {
        ag.process_in_place(&mut buf);
        if i == total_blocks / 4 {
            ag.set_enabled(false);
        }
        if i == total_blocks / 2 {
            ag.set_enabled(true);
        }
        if i == 3 * total_blocks / 4 {
            ag.reset();
        }
    }
    let count = disarm();

    assert_eq!(
        count, 0,
        "AutoGain::process_in_place/set_enabled/reset allocated {count} time(s)"
    );
}
