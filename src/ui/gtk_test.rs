//! Single shared GTK thread for widget-constructing unit tests.
//!
//! GTK may be initialized from exactly one OS thread per process, but `cargo
//! test` runs every `#[test]` on its own thread — so two tests that each call
//! `gtk4::init()` panic with "Attempted to initialize GTK from two different
//! threads". Before this module existed that constraint limited the whole
//! test binary to ONE GTK-touching test, which is why widget-level behavior
//! (e.g. what an `AdwComboRow` actually displays) went untested and the
//! blank Microphone row (debug session mic-row-blank-single-device) shipped.
//!
//! This module owns one lazily spawned thread that initializes GTK +
//! libadwaita once and then runs submitted closures one at a time, so any
//! number of tests can construct and inspect real widgets.
//!
//! Headless (no display server): initialization fails once and every [`run`]
//! call returns `None`, so callers skip cleanly — the same convention the
//! crate's GTK tests already followed.

use std::panic::{self, AssertUnwindSafe};
use std::sync::OnceLock;
use std::sync::mpsc;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// `None` once GTK initialization has failed (headless); otherwise the job
/// queue of the shared GTK thread.
static GTK_THREAD: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();

fn gtk_thread() -> Option<&'static mpsc::Sender<Job>> {
    GTK_THREAD
        .get_or_init(|| {
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let (ready_tx, ready_rx) = mpsc::channel::<bool>();
            std::thread::Builder::new()
                .name("gtk-test".into())
                .spawn(move || {
                    let ok = gtk4::init().is_ok() && libadwaita::init().is_ok();
                    let _ = ready_tx.send(ok);
                    if ok {
                        for job in job_rx {
                            job();
                        }
                    }
                })
                .expect("failed to spawn the shared GTK test thread");
            ready_rx.recv().unwrap_or(false).then_some(job_tx)
        })
        .as_ref()
}

/// Run `f` on the shared GTK test thread and return its result.
///
/// Returns `None` when GTK cannot be initialized (no display server), so the
/// caller can skip. A panic inside `f` (e.g. a failed assertion) is caught on
/// the GTK thread — keeping that thread alive for the remaining tests — and
/// re-raised on the calling test's thread, so the test fails normally rather
/// than being mistaken for a headless skip.
pub(crate) fn run<R, F>(f: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let tx = gtk_thread()?;
    let (res_tx, res_rx) = mpsc::channel();
    tx.send(Box::new(move || {
        let _ = res_tx.send(panic::catch_unwind(AssertUnwindSafe(f)));
    }))
    .expect("the shared GTK test thread has exited");
    match res_rx
        .recv()
        .expect("the shared GTK test thread dropped a job")
    {
        Ok(r) => Some(r),
        Err(payload) => panic::resume_unwind(payload),
    }
}
