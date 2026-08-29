//! Signal handling shared by the daemon and the API server.
//!
//! SIGINT and SIGTERM ask for a clean stop, SIGHUP for a reload. Handlers may
//! only touch atomics, so they merely set a flag that the loops read.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        RELOAD.store(true, Ordering::SeqCst);
    } else {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
}

pub fn install() {
    let handler = on_signal as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

/// Has a stop been asked for?
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Has a reload been asked for? Reading clears the flag.
pub fn take_reload_request() -> bool {
    RELOAD.swap(false, Ordering::SeqCst)
}
