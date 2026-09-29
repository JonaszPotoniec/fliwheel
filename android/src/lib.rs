#[macro_use]
extern crate log;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use fliwheel_core::gui::{ButtonCallback, RenderCallback, ScrollCallback, TakeControls};
use fliwheel_core::sys::eapp::{Eapp, EappAudioEventQueue, EappKey};
use jni::objects::{JClass, JIntArray, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

struct EmulatorSession {
    render_cb: RenderCallback,
    key_callbacks: HashMap<EappKey, ButtonCallback>,
    scroll_cb: Option<ScrollCallback>,
    audio_queue: EappAudioEventQueue,
    stop_signal: Arc<AtomicBool>,
    frame_seq: Arc<AtomicU64>,
    last_rendered_seq: u64,
    emu_thread: Option<thread::JoinHandle<()>>,
    frame_buffer: Vec<u32>,
}

fn map_android_keycode(key_code: i32) -> Option<EappKey> {
    match key_code {
        4 | 19 | 24 => Some(EappKey::Up),     // KEYCODE_BACK (physical top button on Y2) / KEYCODE_DPAD_UP / KEYCODE_VOLUME_UP
        20 | 25 => Some(EappKey::Down),       // KEYCODE_DPAD_DOWN / KEYCODE_VOLUME_DOWN
        21 => Some(EappKey::Left),            // KEYCODE_DPAD_LEFT
        22 => Some(EappKey::Right),           // KEYCODE_DPAD_RIGHT
        23 | 66 => Some(EappKey::Action),     // KEYCODE_DPAD_CENTER / KEYCODE_ENTER
        82 => Some(EappKey::Menu),            // KEYCODE_MENU
        85 => Some(EappKey::Down),            // KEYCODE_MEDIA_PLAY_PAUSE (bottom button)
        88 => Some(EappKey::Left),            // KEYCODE_MEDIA_PREVIOUS (left button)
        87 => Some(EappKey::Right),           // KEYCODE_MEDIA_NEXT (right button)
        _ => None,
    }
}

fn append_game_log(msg: &str) {
    use std::io::Write;
    for path in [
        "/storage/sdcard0/y1_game_log.txt",
        "/storage/sdcard1/y1_game_log.txt",
        "/sdcard/y1_game_log.txt",
    ] {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[FliwheelNative] {}", msg);
            break;
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    bundle_path: JString,
) -> jlong {
    #[cfg(target_os = "android")]
    {
        let _ = android_logger::init_once(
            android_logger::Config::default()
                .with_tag("FliwheelCore")
                .with_max_level(log::LevelFilter::Warn),
        );
    }

    let bundle_path_str: String = match env.get_string(&bundle_path) {
        Ok(s) => s.into(),
        Err(e) => {
            let err = format!("nativeInit: Failed to get bundle_path string: {:?}", e);
            error!("{}", err);
            append_game_log(&err);
            return 0;
        }
    };

    append_game_log(&format!("nativeInit called with bundle_path: {}", bundle_path_str));

    let path = PathBuf::from(&bundle_path_str);
    if !path.exists() {
        let err = format!("nativeInit: Bundle directory does not exist: {}", bundle_path_str);
        error!("{}", err);
        append_game_log(&err);
        return 0;
    }

    let mut system = match Eapp::from_bundle_dir(&path) {
        Ok(s) => s,
        Err(e) => {
            let err = format!("nativeInit: Failed to load eapp from {}: {:?}", bundle_path_str, e);
            error!("{}", err);
            append_game_log(&err);
            return 0;
        }
    };

    let update_fb = system.render_callback();
    let audio_events = system.audio_event_queue();
    let controls = match system.take_controls() {
        Some(c) => c,
        None => {
            error!("nativeInit: Failed to take eapp controls");
            return 0;
        }
    };

    let stop_signal = system.stop_signal();
    let frame_seq = system.frame_seq();

    let log_path = bundle_path_str.clone();
    let frame_seq_clone = Arc::clone(&frame_seq);
    let stop_signal_clone = Arc::clone(&stop_signal);
    let emu_thread = thread::Builder::new()
        .name("FliwheelEmu".to_string())
        .spawn(move || {
            append_game_log(&format!("Fliwheel emulator worker thread started for {}", log_path));
            let start_time = std::time::Instant::now();
            let mut last_log_time = std::time::Instant::now();
            let mut last_seq = 0u64;

            while !stop_signal_clone.load(Ordering::Relaxed) {
                match system.run_cycles(1_000_000) {
                    Ok(()) => {}
                    Err(err) => {
                        let err_msg = format!("Fliwheel fatal error in emulation thread: {:#010x?}", err);
                        error!("{}", err_msg);
                        append_game_log(&err_msg);
                        break;
                    }
                }
                if last_log_time.elapsed().as_secs() >= 5 {
                    let cur_seq = frame_seq_clone.load(Ordering::Relaxed);
                    let total_secs = start_time.elapsed().as_secs();
                    let d_seq = cur_seq.saturating_sub(last_seq);
                    append_game_log(&format!(
                        "Emu running: uptime={}s, frame_seq={}, +{} frames in last 5s",
                        total_secs, cur_seq, d_seq
                    ));
                    last_log_time = std::time::Instant::now();
                    last_seq = cur_seq;
                }
            }
            append_game_log("Fliwheel emulator worker thread exited");
        })
        .ok();

    let session = Box::new(EmulatorSession {
        render_cb: update_fb,
        key_callbacks: controls.keys,
        scroll_cb: controls.wheel,
        audio_queue: audio_events,
        stop_signal,
        frame_seq,
        last_rendered_seq: 0,
        emu_thread,
        frame_buffer: vec![0; 320 * 240],
    });

    info!("nativeInit: Emulator initialized successfully for {}", bundle_path_str);
    Box::into_raw(session) as jlong
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativeRenderFrame(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    out_pixels: JIntArray,
) -> jboolean {
    if handle == 0 {
        return JNI_FALSE;
    }
    let session = unsafe { &mut *(handle as *mut EmulatorSession) };

    let current_seq = session.frame_seq.load(Ordering::Acquire);
    if current_seq == 0 || current_seq == session.last_rendered_seq {
        return JNI_FALSE;
    }

    let (_width, _height) = (session.render_cb)(&mut session.frame_buffer);
    if session.frame_buffer.is_empty() {
        return JNI_FALSE;
    }

    let expected_len = (320 * 240) as usize;
    let raw_slice: &[jint] = unsafe {
        std::slice::from_raw_parts(
            session.frame_buffer.as_ptr() as *const jint,
            expected_len.min(session.frame_buffer.len()),
        )
    };

    if let Err(e) = env.set_int_array_region(&out_pixels, 0, raw_slice) {
        error!("nativeRenderFrame: set_int_array_region failed: {:?}", e);
        return JNI_FALSE;
    }

    session.last_rendered_seq = current_seq;
    JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativeSendKey(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    key_code: jint,
    is_down: jboolean,
) {
    if handle == 0 {
        return;
    }
    let session = unsafe { &mut *(handle as *mut EmulatorSession) };
    if let Some(eapp_key) = map_android_keycode(key_code) {
        if let Some(cb) = session.key_callbacks.get_mut(&eapp_key) {
            cb(is_down == JNI_TRUE);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativeSendWheel(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    delta: jfloat,
) {
    if handle == 0 {
        return;
    }
    let session = unsafe { &mut *(handle as *mut EmulatorSession) };
    if let Some(cb) = session.scroll_cb.as_mut() {
        cb((0.0, delta));
    }
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativePollAudio(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return std::ptr::null_mut();
    }
    let session = unsafe { &mut *(handle as *mut EmulatorSession) };

    let next_path = {
        let mut queue = session.audio_queue.lock().unwrap();
        queue.pop_front().and_then(|event| event.host_path)
    };

    if let Some(path) = next_path {
        let s = path.to_string_lossy();
        match env.new_string(s) {
            Ok(j_str) => j_str.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    } else {
        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "system" fn Java_com_themoon_y1_games_FliwheelBridge_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let mut session = unsafe { Box::from_raw(handle as *mut EmulatorSession) };

    info!("nativeDestroy: stopping emulator thread");
    session.stop_signal.store(true, Ordering::Relaxed);

    if let Some(thread) = session.emu_thread.take() {
        let _ = thread.join();
    }
    info!("nativeDestroy: emulator shutdown complete");
}

/// Stub for dl_iterate_phdr which is missing in Android KitKat (API 19) libc.so.
/// Rust std backtrace references dl_iterate_phdr. Defining it here satisfies the dynamic linker
/// and avoids UnsatisfiedLinkError on KitKat.
#[no_mangle]
pub unsafe extern "C" fn dl_iterate_phdr(
    _callback: Option<unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut std::ffi::c_void) -> std::os::raw::c_int>,
    _data: *mut std::ffi::c_void,
) -> std::os::raw::c_int {
    0
}
