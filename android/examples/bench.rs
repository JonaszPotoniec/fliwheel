//! Headless speed check for the device: runs a bundle for a fixed number of
//! guest instructions and prints how long it took.
//!
//! ```text
//! adb push target/armv7-linux-androideabi/release/examples/bench /data/local/tmp/fliwheel_bench
//! adb shell FLIWHEEL_EAPP_FIXED_CLOCK=1 /data/local/tmp/fliwheel_bench /storage/sdcard1/Games/Tetris/Tetris 30000000
//! ```
//!
//! `FLIWHEEL_EAPP_FIXED_CLOCK=1` makes the guest clock advance by a fixed
//! step, so two builds render the same frames and the times compare.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use fliwheel_core::sys::eapp::Eapp;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(bundle), Some(cycles)) = (args.next(), args.next().and_then(|n| n.parse::<usize>().ok())) else {
        eprintln!("usage: bench <bundle dir> <cycles>");
        std::process::exit(2);
    };
    let mut system = match Eapp::from_bundle_dir(PathBuf::from(&bundle)) {
        Ok(system) => system,
        Err(err) => {
            eprintln!("could not load {}: {:?}", bundle, err);
            std::process::exit(1);
        }
    };
    let frames = system.frame_seq();
    let started = Instant::now();
    if let Err(err) = system.run_cycles(cycles) {
        eprintln!("emulation stopped: {:#010x?}", err);
    }
    let elapsed = started.elapsed();
    let frames = frames.load(Ordering::Relaxed);
    println!(
        "{} cycles in {} ms, {} frames ({:.1} ms per frame, {:.2} MIPS)",
        cycles,
        elapsed.as_millis(),
        frames,
        elapsed.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        cycles as f64 / elapsed.as_secs_f64() / 1.0e6,
    );
}

/// Android 4.4's libc has no `dl_iterate_phdr`, which Rust's std references.
#[no_mangle]
pub unsafe extern "C" fn dl_iterate_phdr(
    _callback: Option<unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut std::ffi::c_void) -> std::os::raw::c_int>,
    _data: *mut std::ffi::c_void,
) -> std::os::raw::c_int {
    0
}

/// Android 4.4 only has `signal` as a header inline over `bsd_signal`; Rust's
/// startup code calls it.
#[cfg(target_os = "android")]
#[no_mangle]
pub unsafe extern "C" fn signal(signum: std::os::raw::c_int, handler: usize) -> usize {
    extern "C" {
        fn bsd_signal(signum: std::os::raw::c_int, handler: usize) -> usize;
    }
    bsd_signal(signum, handler)
}
