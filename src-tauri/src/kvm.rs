use rdev::{simulate, display_size, Button, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime};
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::fs::OpenOptions;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};
use serde::Serialize;

// ============================================================================
// Platform-Specific Cursor Capture
// ============================================================================

/// Lock cursor to a 1x1 pixel rect at screen center (Windows)
/// Uses ClipCursor Win32 API — the standard approach used by lan-mouse, Barrier, etc.
#[cfg(target_os = "windows")]
fn platform_capture_cursor() {
    use windows::Win32::UI::WindowsAndMessaging::ClipCursor;
    use windows::Win32::Foundation::RECT;

    let (w, h) = get_screen_size();
    let cx = (w / 2.0) as i32;
    let cy = (h / 2.0) as i32;

    let rect = RECT {
        left: cx,
        top: cy,
        right: cx + 1,
        bottom: cy + 1,
    };
    unsafe {
        let _ = ClipCursor(Some(&rect));
    }
    log_write("INFO", &format!("KVM Host: Cursor clipped to center ({}, {})", cx, cy));
}

/// Release cursor clip (Windows)
#[cfg(target_os = "windows")]
fn platform_release_cursor() {
    use windows::Win32::UI::WindowsAndMessaging::ClipCursor;

    unsafe {
        let _ = ClipCursor(None);
    }
    log_write("INFO", "KVM Host: Cursor clip released.");
}

/// Dissociate mouse from cursor position (macOS)
/// Uses CGAssociateMouseAndMouseCursorPosition — the standard for lan-mouse/Barrier/Deskflow
#[cfg(target_os = "macos")]
fn platform_capture_cursor() {
    unsafe {
        core_graphics::ffi::CGAssociateMouseAndMouseCursorPosition(false as i32);
    }
    log_write("INFO", "KVM Host: macOS cursor dissociated (CGAssociateMouseAndMouseCursorPosition=NO).");
}

/// Re-associate mouse with cursor (macOS)
#[cfg(target_os = "macos")]
fn platform_release_cursor() {
    unsafe {
        core_graphics::ffi::CGAssociateMouseAndMouseCursorPosition(true as i32);
    }
    log_write("INFO", "KVM Host: macOS cursor re-associated.");
}

/// Fallback for other platforms — no-op
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_capture_cursor() {
    log_write("WARN", "KVM Host: No native cursor capture on this platform.");
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_release_cursor() {}

// ============================================================================
// Thread-safe Global File Logger
// ============================================================================

pub static LOG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

fn get_timestamp() -> String {
    if let Ok(elapsed) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        let secs = elapsed.as_secs();
        let hours = (secs / 3600) % 24;
        let mins = (secs / 60) % 60;
        let seconds = secs % 60;
        let millis = elapsed.subsec_millis();
        format!("{:02}:{:02}:{:02}.{:03}", hours, mins, seconds, millis)
    } else {
        "00:00:00.000".to_string()
    }
}

pub fn init_logger(app_handle: &AppHandle) {
    use tauri::Manager;
    let log_dir = app_handle.path().app_log_dir().unwrap_or_else(|_| {
        dirs::home_dir().map(|h| h.join(".deskbridge")).unwrap_or_else(|| PathBuf::from("."))
    });
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("deskbridge.log");
    
    // Back up previous log so crash info is never lost
    if log_file.exists() {
        let prev = log_dir.join("deskbridge.prev.log");
        let _ = std::fs::copy(&log_file, &prev);
    }
    
    let _ = LOG_FILE_PATH.set(log_file.clone());
    
    // APPEND, not truncate! Previous session's crash info is preserved above in .prev
    if let Ok(mut f) = OpenOptions::new().create(true).write(true).truncate(true).open(&log_file) {
        let _ = f.write_all(b"=== DeskBridge Session Started at UTC ===\n");
    }
    log_write("INFO", &format!("Logger initialized. Log path: {:?}", log_file));
}

pub fn log_write(level: &str, message: &str) {
    let ts = get_timestamp();
    let line = format!("[{}] [{}] {}\n", ts, level, message);
    print!("{}", line);
    
    if let Some(path) = LOG_FILE_PATH.get() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

// ============================================================================
// Atomic State Variables
// ============================================================================

pub static KVM_ENABLED: AtomicBool = AtomicBool::new(false);
pub static KVM_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static BORDER_X: AtomicU16 = AtomicU16::new(1919);
pub static BORDER_DIRECTION: AtomicU8 = AtomicU8::new(1); // 1 = Right, 0 = Left
pub static SCREEN_WIDTH: AtomicU16 = AtomicU16::new(1920);
pub static SCREEN_HEIGHT: AtomicU16 = AtomicU16::new(1080);

// Last known cursor position (for delta computation)
pub static LAST_X: AtomicI32 = AtomicI32::new(0);
pub static LAST_Y: AtomicI32 = AtomicI32::new(0);

// ============================================================================
// Delta Accumulator (lock-free, for high polling rate mice)
//
// At 8000Hz, grab_callback is called 8000x/sec. We CANNOT do any blocking
// operations (mpsc::send, Mutex::lock) in the callback for MouseMove.
// Instead, we atomically accumulate deltas here, and a separate sender
// thread drains them at ~120Hz — reducing 8000 events to 120 packets.
// ============================================================================
static ACCUM_DX: AtomicI32 = AtomicI32::new(0);
static ACCUM_DY: AtomicI32 = AtomicI32::new(0);

// Configurable hotkey (defaults to Ctrl + Alt + K)
pub static HOTKEY_CTRL: AtomicBool = AtomicBool::new(true);
pub static HOTKEY_ALT: AtomicBool = AtomicBool::new(true);
pub static HOTKEY_SHIFT: AtomicBool = AtomicBool::new(false);
pub static HOTKEY_KEY: AtomicU16 = AtomicU16::new(11); // KeyK

// Connection guard
pub static IS_CONNECTING: AtomicBool = AtomicBool::new(false);

// Modifier tracking
static CTRL_PRESSED: AtomicBool = AtomicBool::new(false);
static ALT_PRESSED: AtomicBool = AtomicBool::new(false);
static SHIFT_PRESSED: AtomicBool = AtomicBool::new(false);

// Target IP storage
static TARGET_IP: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

// Global AppHandle
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

// ============================================================================
// Helpers
// ============================================================================

pub fn get_screen_size() -> (f64, f64) {
    if let Ok((w, h)) = display_size() {
        (w as f64, h as f64)
    } else {
        (SCREEN_WIDTH.load(Ordering::SeqCst) as f64, SCREEN_HEIGHT.load(Ordering::SeqCst) as f64)
    }
}

#[derive(Serialize, Clone)]
struct KvmStatusUpdate {
    active: bool,
    role: String,
    target: String,
}

fn get_target_ip() -> String {
    TARGET_IP.get_or_init(|| std::sync::Mutex::new(String::new()))
        .lock().unwrap().clone()
}

fn set_target_ip(ip: String) {
    *TARGET_IP.get_or_init(|| std::sync::Mutex::new(String::new()))
        .lock().unwrap() = ip;
}

#[cfg(target_os = "macos")]
pub fn check_accessibility() -> bool {
    macos_accessibility_client::accessibility::application_is_trusted_with_prompt()
}

#[cfg(not(target_os = "macos"))]
pub fn check_accessibility() -> bool {
    true
}

/// Deactivate KVM and release cursor. Safe to call multiple times (idempotent).
fn deactivate_kvm_host() {
    if KVM_ACTIVE.swap(false, Ordering::SeqCst) {
        platform_release_cursor();
        release_stuck_modifiers();
        // Set cooldown to prevent instant re-trigger from edge detection
        DEACTIVATION_COOLDOWN.store(true, Ordering::SeqCst);
        let _ = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(1500));
            DEACTIVATION_COOLDOWN.store(false, Ordering::SeqCst);
        });
        log_write("INFO", "KVM Host: Deactivated, cursor released, modifiers reset.");
    }
}

static DEACTIVATION_COOLDOWN: AtomicBool = AtomicBool::new(false);

/// Send synthetic KeyUp for all modifiers to prevent sticky keys after crash/deactivation
#[cfg(target_os = "windows")]
fn release_stuck_modifiers() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
    unsafe {
        keybd_event(0xA2, 0, KEYEVENTF_KEYUP, 0); // Left Ctrl
        keybd_event(0xA3, 0, KEYEVENTF_KEYUP, 0); // Right Ctrl
        keybd_event(0xA4, 0, KEYEVENTF_KEYUP, 0); // Left Alt
        keybd_event(0xA5, 0, KEYEVENTF_KEYUP, 0); // Right Alt
        keybd_event(0xA0, 0, KEYEVENTF_KEYUP, 0); // Left Shift
        keybd_event(0xA1, 0, KEYEVENTF_KEYUP, 0); // Right Shift
    }
    CTRL_PRESSED.store(false, Ordering::SeqCst);
    ALT_PRESSED.store(false, Ordering::SeqCst);
    SHIFT_PRESSED.store(false, Ordering::SeqCst);
    log_write("INFO", "KVM Host: All modifier keys released.");
}

#[cfg(not(target_os = "windows"))]
fn release_stuck_modifiers() {
    // On macOS, simulate key releases via rdev
    let _ = simulate(&EventType::KeyRelease(Key::ControlLeft));
    let _ = simulate(&EventType::KeyRelease(Key::Alt));
    let _ = simulate(&EventType::KeyRelease(Key::ShiftLeft));
    CTRL_PRESSED.store(false, Ordering::SeqCst);
    ALT_PRESSED.store(false, Ordering::SeqCst);
    SHIFT_PRESSED.store(false, Ordering::SeqCst);
    log_write("INFO", "KVM Host: All modifier keys released (macOS).");
}

// ============================================================================
// Key Serialization
// ============================================================================

fn key_to_u16(key: Key) -> u16 {
    match key {
        Key::KeyA => 1, Key::KeyB => 2, Key::KeyC => 3, Key::KeyD => 4, Key::KeyE => 5,
        Key::KeyF => 6, Key::KeyG => 7, Key::KeyH => 8, Key::KeyI => 9, Key::KeyJ => 10,
        Key::KeyK => 11, Key::KeyL => 12, Key::KeyM => 13, Key::KeyN => 14, Key::KeyO => 15,
        Key::KeyP => 16, Key::KeyQ => 17, Key::KeyR => 18, Key::KeyS => 19, Key::KeyT => 20,
        Key::KeyU => 21, Key::KeyV => 22, Key::KeyW => 23, Key::KeyX => 24, Key::KeyY => 25,
        Key::KeyZ => 26,
        Key::Num0 => 27, Key::Num1 => 28, Key::Num2 => 29, Key::Num3 => 30, Key::Num4 => 31,
        Key::Num5 => 32, Key::Num6 => 33, Key::Num7 => 34, Key::Num8 => 35, Key::Num9 => 36,
        Key::Space => 37, Key::Return => 38, Key::Escape => 39, Key::Backspace => 40,
        Key::Tab => 41, Key::ShiftLeft => 42, Key::ShiftRight => 43, Key::ControlLeft => 44,
        Key::ControlRight => 45, Key::Alt => 46, Key::AltGr => 47, Key::MetaLeft => 48,
        Key::MetaRight => 49, Key::CapsLock => 50,
        Key::UpArrow => 51, Key::DownArrow => 52, Key::LeftArrow => 53, Key::RightArrow => 54,
        Key::Delete => 55, Key::Insert => 56, Key::Home => 57, Key::End => 58,
        Key::PageUp => 59, Key::PageDown => 60,
        Key::F1 => 61, Key::F2 => 62, Key::F3 => 63, Key::F4 => 64, Key::F5 => 65,
        Key::F6 => 66, Key::F7 => 67, Key::F8 => 68, Key::F9 => 69, Key::F10 => 70,
        Key::F11 => 71, Key::F12 => 72,
        Key::SemiColon => 73, Key::Equal => 74, Key::Comma => 75, Key::Minus => 76,
        Key::Dot => 77, Key::Slash => 78, Key::BackQuote => 79, Key::LeftBracket => 80,
        Key::BackSlash => 81, Key::RightBracket => 82, Key::Quote => 83,
        Key::Unknown(code) => 0x8000 | (code as u16 & 0x7FFF),
        _ => 0,
    }
}

fn u16_to_key(val: u16) -> Key {
    if (val & 0x8000) != 0 {
        return Key::Unknown((val & 0x7FFF) as u32);
    }
    match val {
        1 => Key::KeyA, 2 => Key::KeyB, 3 => Key::KeyC, 4 => Key::KeyD, 5 => Key::KeyE,
        6 => Key::KeyF, 7 => Key::KeyG, 8 => Key::KeyH, 9 => Key::KeyI, 10 => Key::KeyJ,
        11 => Key::KeyK, 12 => Key::KeyL, 13 => Key::KeyM, 14 => Key::KeyN, 15 => Key::KeyO,
        16 => Key::KeyP, 17 => Key::KeyQ, 18 => Key::KeyR, 19 => Key::KeyS, 20 => Key::KeyT,
        21 => Key::KeyU, 22 => Key::KeyV, 23 => Key::KeyW, 24 => Key::KeyX, 25 => Key::KeyY,
        26 => Key::KeyZ,
        27 => Key::Num0, 28 => Key::Num1, 29 => Key::Num2, 30 => Key::Num3, 31 => Key::Num4,
        32 => Key::Num5, 33 => Key::Num6, 34 => Key::Num7, 35 => Key::Num8, 36 => Key::Num9,
        37 => Key::Space, 38 => Key::Return, 39 => Key::Escape, 40 => Key::Backspace,
        41 => Key::Tab, 42 => Key::ShiftLeft, 43 => Key::ShiftRight, 44 => Key::ControlLeft,
        45 => Key::ControlRight, 46 => Key::Alt, 47 => Key::AltGr, 48 => Key::MetaLeft,
        49 => Key::MetaRight, 50 => Key::CapsLock,
        51 => Key::UpArrow, 52 => Key::DownArrow, 53 => Key::LeftArrow, 54 => Key::RightArrow,
        55 => Key::Delete, 56 => Key::Insert, 57 => Key::Home, 58 => Key::End,
        59 => Key::PageUp, 60 => Key::PageDown,
        61 => Key::F1, 62 => Key::F2, 63 => Key::F3, 64 => Key::F4, 65 => Key::F5,
        66 => Key::F6, 67 => Key::F7, 68 => Key::F8, 69 => Key::F9, 70 => Key::F10,
        71 => Key::F11, 72 => Key::F12,
        73 => Key::SemiColon, 74 => Key::Equal, 75 => Key::Comma, 76 => Key::Minus,
        77 => Key::Dot, 78 => Key::Slash, 79 => Key::BackQuote, 80 => Key::LeftBracket,
        81 => Key::BackSlash, 82 => Key::RightBracket, 83 => Key::Quote,
        _ => Key::Unknown(0),
    }
}

// ============================================================================
// Grab Callback (runs on OS hook thread — MUST be as fast as possible!)
//
// At 8KHz mouse polling, this is called 8000x/sec.
// Windows kills WH_MOUSE_LL hooks if callback exceeds LowLevelHooksTimeout.
// For MouseMove: ONLY atomic operations (fetch_add) — NO channel send,
// NO Mutex, NO allocation. Keyboard/button events are rare (~50/sec)
// so channel send is fine for those.
// ============================================================================

fn grab_callback(event: Event) -> Option<Event> {
    if !KVM_ENABLED.load(Ordering::SeqCst) {
        return Some(event);
    }

    // --- Track modifier states (atomic stores — instant) ---
    match event.event_type {
        EventType::KeyPress(key) => {
            if key == Key::ControlLeft || key == Key::ControlRight {
                CTRL_PRESSED.store(true, Ordering::SeqCst);
            } else if key == Key::Alt || key == Key::AltGr {
                ALT_PRESSED.store(true, Ordering::SeqCst);
            } else if key == Key::ShiftLeft || key == Key::ShiftRight {
                SHIFT_PRESSED.store(true, Ordering::SeqCst);
            }
            
            // Failsafe: Ctrl+Alt+Escape always releases
            if KVM_ACTIVE.load(Ordering::SeqCst)
                && key == Key::Escape
                && CTRL_PRESSED.load(Ordering::SeqCst)
                && ALT_PRESSED.load(Ordering::SeqCst)
            {
                log_write("INFO", "KVM Host: Failsafe Ctrl+Alt+Esc triggered!");
                deactivate_kvm_host();
                return None;
            }

            // Custom hotkey toggle
            let code = key_to_u16(key);
            if code == HOTKEY_KEY.load(Ordering::SeqCst)
                && CTRL_PRESSED.load(Ordering::SeqCst) == HOTKEY_CTRL.load(Ordering::SeqCst)
                && ALT_PRESSED.load(Ordering::SeqCst) == HOTKEY_ALT.load(Ordering::SeqCst)
                && SHIFT_PRESSED.load(Ordering::SeqCst) == HOTKEY_SHIFT.load(Ordering::SeqCst)
            {
                if KVM_ACTIVE.load(Ordering::SeqCst) {
                    log_write("INFO", "KVM Host: Hotkey — releasing control.");
                    deactivate_kvm_host();
                } else {
                    let target = get_target_ip();
                    if !target.is_empty() && !IS_CONNECTING.load(Ordering::SeqCst) {
                        log_write("INFO", "KVM Host: Hotkey — initiating KVM session.");
                        IS_CONNECTING.store(true, Ordering::SeqCst);
                        if let Some(app_handle) = APP_HANDLE.get() {
                            let app_clone = app_handle.clone();
                            thread::spawn(move || {
                                initiate_kvm_control_session(app_clone, target);
                            });
                        }
                    }
                }
                return None; // swallow hotkey
            }
        }
        EventType::KeyRelease(key) => {
            if key == Key::ControlLeft || key == Key::ControlRight {
                CTRL_PRESSED.store(false, Ordering::SeqCst);
            } else if key == Key::Alt || key == Key::AltGr {
                ALT_PRESSED.store(false, Ordering::SeqCst);
            } else if key == Key::ShiftLeft || key == Key::ShiftRight {
                SHIFT_PRESSED.store(false, Ordering::SeqCst);
            }
        }
        _ => {}
    }

    // --- Not active: only do edge detection ---
    if !KVM_ACTIVE.load(Ordering::SeqCst) {
        if let EventType::MouseMove { x, .. } = event.event_type {
            // Don't reconnect during cooldown (prevents auto-reconnect loop)
            if !IS_CONNECTING.load(Ordering::SeqCst) && !DEACTIVATION_COOLDOWN.load(Ordering::SeqCst) {
                let (w, _) = get_screen_size();
                let direction = BORDER_DIRECTION.load(Ordering::SeqCst);

                let hit = if direction == 1 { x >= w - 2.0 } else { x <= 2.0 };

                if hit {
                    let target = get_target_ip();
                    if !target.is_empty() {
                        IS_CONNECTING.store(true, Ordering::SeqCst);
                        if let Some(app_handle) = APP_HANDLE.get() {
                            let app_clone = app_handle.clone();
                            thread::spawn(move || {
                                initiate_kvm_control_session(app_clone, target);
                            });
                        }
                    }
                }
            }
        }
        return Some(event);
    }

    // ===================================================================
    // Active Session: Check hotkey FIRST, then capture input
    // ===================================================================

    // Check for deactivation hotkey (e.g. Ctrl+Alt+K)
    if let EventType::KeyPress(key) = event.event_type {
        let key_code = key_to_u16(key);
        let hotkey_key = HOTKEY_KEY.load(Ordering::SeqCst);
        let need_ctrl = HOTKEY_CTRL.load(Ordering::SeqCst);
        let need_alt = HOTKEY_ALT.load(Ordering::SeqCst);
        let need_shift = HOTKEY_SHIFT.load(Ordering::SeqCst);

        let ctrl_ok = !need_ctrl || CTRL_PRESSED.load(Ordering::SeqCst);
        let alt_ok = !need_alt || ALT_PRESSED.load(Ordering::SeqCst);
        let shift_ok = !need_shift || SHIFT_PRESSED.load(Ordering::SeqCst);

        if key_code == hotkey_key && ctrl_ok && alt_ok && shift_ok {
            log_write("INFO", "KVM Host: Hotkey pressed — deactivating KVM.");
            deactivate_kvm_host();
            set_active_sender(None);
            if let Some(app_handle) = APP_HANDLE.get() {
                let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                    active: false, role: "idle".to_string(), target: "".to_string(),
                });
            }
            return Some(event); // Let the key through to Windows
        }

        // Also check Ctrl+Alt+Escape as emergency release
        if key == Key::Escape && CTRL_PRESSED.load(Ordering::SeqCst) && ALT_PRESSED.load(Ordering::SeqCst) {
            log_write("INFO", "KVM Host: Emergency release (Ctrl+Alt+Esc).");
            deactivate_kvm_host();
            set_active_sender(None);
            if let Some(app_handle) = APP_HANDLE.get() {
                let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                    active: false, role: "idle".to_string(), target: "".to_string(),
                });
            }
            return Some(event);
        }
    }

    #[allow(unreachable_patterns)]
    match event.event_type {
        EventType::MouseMove { x, y } => {
            // *** CRITICAL: Only atomic operations here! ***
            let last_x = LAST_X.load(Ordering::SeqCst);
            let last_y = LAST_Y.load(Ordering::SeqCst);

            let dx = x as i32 - last_x;
            let dy = y as i32 - last_y;

            LAST_X.store(x as i32, Ordering::SeqCst);
            LAST_Y.store(y as i32, Ordering::SeqCst);

            if dx != 0 || dy != 0 {
                ACCUM_DX.fetch_add(dx, Ordering::Relaxed);
                ACCUM_DY.fetch_add(dy, Ordering::Relaxed);
            }

            return None;
        }

        EventType::KeyPress(key) => {
            let key_code = key_to_u16(key);
            let mut payload = Vec::with_capacity(9);
            payload.push(1);
            payload.extend_from_slice(&key_code.to_le_bytes());
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            if let Some(sender) = get_active_sender() {
                let _ = sender.send(payload);
            }
            return None;
        }

        EventType::KeyRelease(key) => {
            let key_code = key_to_u16(key);
            let mut payload = Vec::with_capacity(9);
            payload.push(2);
            payload.extend_from_slice(&key_code.to_le_bytes());
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            if let Some(sender) = get_active_sender() {
                let _ = sender.send(payload);
            }
            return None;
        }

        EventType::ButtonPress(btn) => {
            let btn_id = match btn {
                Button::Left => 0, Button::Right => 1, Button::Middle => 2,
                Button::Unknown(code) => code,
            };
            let mut payload = Vec::with_capacity(9);
            payload.push(3);
            payload.push(btn_id);
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
            if let Some(sender) = get_active_sender() {
                let _ = sender.send(payload);
            }
            return None;
        }

        EventType::ButtonRelease(btn) => {
            let btn_id = match btn {
                Button::Left => 0, Button::Right => 1, Button::Middle => 2,
                Button::Unknown(code) => code,
            };
            let mut payload = Vec::with_capacity(9);
            payload.push(4);
            payload.push(btn_id);
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
            if let Some(sender) = get_active_sender() {
                let _ = sender.send(payload);
            }
            return None;
        }

        EventType::Wheel { delta_x, delta_y } => {
            let mut payload = Vec::with_capacity(9);
            payload.push(5);
            payload.extend_from_slice(&(delta_x as i32).to_le_bytes());
            payload.extend_from_slice(&(delta_y as i32).to_le_bytes());
            if let Some(sender) = get_active_sender() {
                let _ = sender.send(payload);
            }
            return None;
        }

        _ => {
            return Some(event);
        }
    }
}

// ============================================================================
// Background OS Input Hook (runs once at startup)
// ============================================================================

pub fn init_kvm_listener(app_handle: AppHandle) {
    let _ = APP_HANDLE.set(app_handle);
    thread::spawn(|| {
        log_write("INFO", "Starting global input capture listener thread...");
        if let Err(e) = rdev::grab(grab_callback) {
            log_write("ERROR", &format!("Failed to register rdev grab hook: {:?}", e));
        }
    });
}

// ============================================================================
// Channel for keyboard/button/wheel events (NOT mouse — mouse uses atomics)
// ============================================================================

static ACTIVE_SENDER: OnceLock<std::sync::Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>> = OnceLock::new();

fn get_active_sender() -> Option<std::sync::mpsc::Sender<Vec<u8>>> {
    ACTIVE_SENDER.get_or_init(|| std::sync::Mutex::new(None))
        .lock().unwrap().clone()
}

fn set_active_sender(sender: Option<std::sync::mpsc::Sender<Vec<u8>>>) {
    *ACTIVE_SENDER.get_or_init(|| std::sync::Mutex::new(None))
        .lock().unwrap() = sender;
}

// ============================================================================
// KVM Host: Initiate Control Session
//
// CRITICAL: Correct initialization order to prevent delta explosion:
//   1. Connect TCP
//   2. Warp mouse to center
//   3. Set LAST_X/LAST_Y to center
//   4. Capture cursor (ClipCursor / CGAssociate)
//   5. Reset accumulators
//   6. Setup channel + sender
//   7. Spawn threads
//   8. KVM_ACTIVE = true   ← LAST! Only now grab_callback starts capturing
// ============================================================================

fn initiate_kvm_control_session(app_handle: AppHandle, ip: String) {
    let address = format!("{}:53201", ip);
    log_write("INFO", &format!("KVM Host: Connecting to Client at {}...", address));

    match TcpStream::connect_timeout(&address.parse().unwrap(), Duration::from_secs(3)) {
        Ok(stream) => {
            stream.set_nodelay(true).unwrap();

            // Step 1: Warp mouse to screen center
            let (w, h) = get_screen_size();
            let cx = w / 2.0;
            let cy = h / 2.0;
            let _ = simulate(&EventType::MouseMove { x: cx, y: cy });
            thread::sleep(Duration::from_millis(50));

            // Step 2: Set tracking position BEFORE anything else
            LAST_X.store(cx as i32, Ordering::SeqCst);
            LAST_Y.store(cy as i32, Ordering::SeqCst);

            // Step 3: Capture cursor with platform-native API
            platform_capture_cursor();

            // Step 4: Reset delta accumulators (clear any stale events)
            ACCUM_DX.store(0, Ordering::SeqCst);
            ACCUM_DY.store(0, Ordering::SeqCst);

            // Step 5: Setup communication channel
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            let tx_for_keys = tx.clone();
            let tx_for_mouse = tx; // delta sender thread uses this
            set_active_sender(Some(tx_for_keys));

            // Step 6: ACTIVATE NOW — AFTER LAST_X/LAST_Y are set, BEFORE threads start
            // This is critical: threads check KVM_ACTIVE in their loops,
            // so it must be true before they start. And LAST_X/LAST_Y must
            // be set before this to prevent delta explosion.
            KVM_ACTIVE.store(true, Ordering::SeqCst);
            IS_CONNECTING.store(false, Ordering::SeqCst);

            let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                active: true, role: "host".to_string(), target: ip.clone(),
            });

            log_write("INFO", "KVM Host: Session active. Delta batching at 120Hz.");

            // Step 7: Spawn writer thread (reads from rx, writes to TCP)
            let mut writer_socket = stream.try_clone().unwrap();
            let app_writer = app_handle.clone();
            thread::spawn(move || {
                let mut idle_ticks: u32 = 0;
                loop {
                    if !KVM_ACTIVE.load(Ordering::SeqCst) {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(data) => {
                            idle_ticks = 0;
                            if let Err(e) = writer_socket.write_all(&data) {
                                log_write("ERROR", &format!("KVM Host: Socket write error: {:?}", e));
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            idle_ticks += 1;
                            if idle_ticks >= 10 { // 1 second
                                idle_ticks = 0;
                                let keepalive = [7u8, 0, 0, 0, 0, 0, 0, 0, 0];
                                if let Err(e) = writer_socket.write_all(&keepalive) {
                                    log_write("ERROR", &format!("KVM Host: Keepalive failed: {:?}", e));
                                    break;
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                log_write("INFO", "KVM Host: Writer thread ended.");
                deactivate_kvm_host();
                set_active_sender(None);

                // Warp cursor to CENTER of screen to prevent instant re-trigger
                let (w, h) = get_screen_size();
                let _ = simulate(&EventType::MouseMove { x: w / 2.0, y: h / 2.0 });

                let _ = app_writer.emit("kvm-status", KvmStatusUpdate {
                    active: false, role: "idle".to_string(), target: "".to_string(),
                });
            });

            // Step 8: Spawn delta sender thread (drains ACCUM at ~120Hz)
            thread::spawn(move || {
                log_write("INFO", "KVM Host: Delta sender thread started (120Hz).");
                while KVM_ACTIVE.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_micros(8333)); // ~120Hz

                    let dx = ACCUM_DX.swap(0, Ordering::Relaxed);
                    let dy = ACCUM_DY.swap(0, Ordering::Relaxed);

                    if dx != 0 || dy != 0 {
                        let mut payload = Vec::with_capacity(9);
                        payload.push(0); // MouseMove type
                        payload.extend_from_slice(&(dx as f32).to_le_bytes());
                        payload.extend_from_slice(&(dy as f32).to_le_bytes());
                        if tx_for_mouse.send(payload).is_err() {
                            break;
                        }
                    }
                }
                log_write("INFO", "KVM Host: Delta sender thread ended.");
            });

            // Step 9: Spawn reader thread (listens for ReleaseControl from client)
            let mut read_socket = stream;
            thread::spawn(move || {
                let mut buf = [0u8; 9];
                loop {
                    match read_socket.read_exact(&mut buf) {
                        Ok(_) => {
                            if buf[0] == 6 {
                                log_write("INFO", "KVM Host: Client requested release.");
                                deactivate_kvm_host();
                                break;
                            }
                        }
                        Err(e) => {
                            log_write("INFO", &format!("KVM Host Reader: Disconnected: {:?}", e));
                            deactivate_kvm_host();
                            break;
                        }
                    }
                }
            });
        }
        Err(e) => {
            log_write("ERROR", &format!("KVM Host: Failed to connect to {}: {:?}", address, e));
            IS_CONNECTING.store(false, Ordering::SeqCst);
        }
    }
}

// ============================================================================
// KVM Client (Receiver) Server
// ============================================================================

pub fn start_kvm_client_server(app_handle: AppHandle) {
    thread::spawn(move || {
        let listener = match TcpListener::bind("0.0.0.0:53201") {
            Ok(l) => l,
            Err(e) => {
                log_write("ERROR", &format!("Failed to bind KVM Client port 53201: {:?}", e));
                return;
            }
        };
        log_write("INFO", "KVM Client: Listening for host KVM connections on port 53201...");

        for stream in listener.incoming() {
            match stream {
                Ok(mut socket) => {
                    socket.set_nodelay(true).unwrap();
                    socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();

                    let peer_addr = socket.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
                    log_write("INFO", &format!("KVM Client: Connected by Host at {}", peer_addr));

                    let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                        active: true, role: "client".to_string(), target: peer_addr.clone(),
                    });

                    let (w, h) = get_screen_size();
                    let mut current_x = w / 2.0;
                    let mut current_y = h / 2.0;
                    if let Err(e) = simulate(&EventType::MouseMove { x: current_x, y: current_y }) {
                        log_write("ERROR", &format!("KVM Client: Initial MouseMove failed: {:?}", e));
                    }

                    let mut write_socket = socket.try_clone().unwrap();
                    let mut buf = [0u8; 9];
                    let mut controlled = true;

                    log_write("INFO", &format!("KVM Client: Control session started. Accessibility: {}", check_accessibility()));

                    while controlled {
                        match socket.read_exact(&mut buf) {
                            Ok(_) => {
                                match buf[0] {
                                    0 => {
                                        // MouseMove delta
                                        let dx = f32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
                                        let dy = f32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);

                                        current_x += dx as f64;
                                        current_y += dy as f64;

                                        // Check if cursor should return to host
                                        let direction = BORDER_DIRECTION.load(Ordering::SeqCst);
                                        let (cw, ch) = get_screen_size();
                                        let release = if direction == 1 {
                                            current_x <= 2.0 && dx < 0.0
                                        } else {
                                            current_x >= cw - 2.0 && dx > 0.0
                                        };

                                        if release {
                                            log_write("INFO", "KVM Client: Boundary reached, releasing.");
                                            let packet = [6u8, 0, 0, 0, 0, 0, 0, 0, 0];
                                            let _ = write_socket.write_all(&packet);
                                            controlled = false;
                                        } else {
                                            current_x = current_x.clamp(0.0, cw);
                                            current_y = current_y.clamp(0.0, ch);
                                            if let Err(e) = simulate(&EventType::MouseMove { x: current_x, y: current_y }) {
                                                log_write("ERROR", &format!("KVM Client: MouseMove failed: {:?}", e));
                                            }
                                        }
                                    }
                                    1 => {
                                        let val = u16::from_le_bytes([buf[1], buf[2]]);
                                        let _ = simulate(&EventType::KeyPress(u16_to_key(val)));
                                    }
                                    2 => {
                                        let val = u16::from_le_bytes([buf[1], buf[2]]);
                                        let _ = simulate(&EventType::KeyRelease(u16_to_key(val)));
                                    }
                                    3 => {
                                        let btn = match buf[1] {
                                            0 => Button::Left, 1 => Button::Right,
                                            2 => Button::Middle, c => Button::Unknown(c),
                                        };
                                        let _ = simulate(&EventType::ButtonPress(btn));
                                    }
                                    4 => {
                                        let btn = match buf[1] {
                                            0 => Button::Left, 1 => Button::Right,
                                            2 => Button::Middle, c => Button::Unknown(c),
                                        };
                                        let _ = simulate(&EventType::ButtonRelease(btn));
                                    }
                                    5 => {
                                        let dx = i32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
                                        let dy = i32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);
                                        let _ = simulate(&EventType::Wheel { delta_x: dx as i64, delta_y: dy as i64 });
                                    }
                                    7 => {} // Keepalive
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                log_write("INFO", &format!("KVM Client: Disconnected: {:?}", e));
                                controlled = false;
                            }
                        }
                    }

                    log_write("INFO", "KVM Client: Control session ended.");
                    let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                        active: false, role: "idle".to_string(), target: "".to_string(),
                    });
                }
                Err(e) => {
                    log_write("ERROR", &format!("KVM Client: Accept error: {:?}", e));
                }
            }
        }
    });
}

// ============================================================================
// Tauri Commands
// ============================================================================

#[tauri::command]
pub fn configure_kvm(enabled: bool, target_ip: String, screen_w: u16, screen_h: u16, border_x: u16, direction: u8) -> Result<String, String> {
    if enabled && !check_accessibility() {
        return Err("accessibility_not_granted".to_string());
    }

    KVM_ENABLED.store(enabled, Ordering::SeqCst);
    set_target_ip(target_ip);
    SCREEN_WIDTH.store(screen_w, Ordering::SeqCst);
    SCREEN_HEIGHT.store(screen_h, Ordering::SeqCst);
    BORDER_X.store(border_x, Ordering::SeqCst);
    BORDER_DIRECTION.store(direction, Ordering::SeqCst);

    log_write("INFO", &format!("KVM Configured: Enabled: {}, Target: {}, Screen: {}x{}, BorderX: {}, Dir: {}",
             enabled, get_target_ip(), screen_w, screen_h, border_x, direction));

    Ok("success".to_string())
}

#[tauri::command]
pub fn trigger_manual_control(app_handle: AppHandle) -> Result<String, String> {
    if !check_accessibility() {
        return Err("accessibility_not_granted".to_string());
    }

    let target = get_target_ip();
    if target.is_empty() {
        return Err("no_target_ip_configured".to_string());
    }

    thread::spawn(move || {
        initiate_kvm_control_session(app_handle, target);
    });

    Ok("session_initiated".to_string())
}

#[tauri::command]
pub fn release_manual_control() -> Result<String, String> {
    deactivate_kvm_host();
    Ok("control_released".to_string())
}

#[tauri::command]
pub fn set_kvm_hotkey(ctrl: bool, alt: bool, shift: bool, key_code: u16) -> Result<(), String> {
    HOTKEY_CTRL.store(ctrl, Ordering::SeqCst);
    HOTKEY_ALT.store(alt, Ordering::SeqCst);
    HOTKEY_SHIFT.store(shift, Ordering::SeqCst);
    HOTKEY_KEY.store(key_code, Ordering::SeqCst);
    log_write("INFO", &format!("KVM Hotkey updated: Ctrl={}, Alt={}, Shift={}, KeyCode={}", ctrl, alt, shift, key_code));
    Ok(())
}
