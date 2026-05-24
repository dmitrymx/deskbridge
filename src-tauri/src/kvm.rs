use rdev::{simulate, display_size, Button, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime};
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};
use serde::Serialize;

// --- Platform-Specific Cursor Capture ---

/// Lock cursor to a 1x1 pixel rect at screen center (Windows)
/// This prevents the physical cursor from moving while KVM is active.
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
/// After this call, mouse hardware generates delta events only,
/// and the on-screen cursor does NOT move.
#[cfg(target_os = "macos")]
fn platform_capture_cursor() {
    use core_graphics::event::CGEventSourceStateID;

    // CGAssociateMouseAndMouseCursorPosition(false) dissociates the mouse from the cursor
    // This is the standard approach used by lan-mouse, Barrier, Deskflow, etc.
    unsafe {
        core_graphics::ffi::CGAssociateMouseAndMouseCursorPosition(false as i32);
    }
    log_write("INFO", "KVM Host: macOS cursor dissociated from mouse (CGAssociateMouseAndMouseCursorPosition=NO).");
}

/// Re-associate mouse with cursor (macOS)
#[cfg(target_os = "macos")]
fn platform_release_cursor() {
    unsafe {
        core_graphics::ffi::CGAssociateMouseAndMouseCursorPosition(true as i32);
    }
    log_write("INFO", "KVM Host: macOS cursor re-associated with mouse.");
}

/// Fallback for other platforms (Linux, etc.) — no-op
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_capture_cursor() {
    log_write("WARN", "KVM Host: No native cursor capture available on this platform.");
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_release_cursor() {
    log_write("INFO", "KVM Host: No native cursor release needed on this platform.");
}

// --- Thread-safe Global File Logger ---
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
    let _ = LOG_FILE_PATH.set(log_file.clone());
    
    if let Ok(mut f) = File::create(&log_file) {
        let _ = f.write_all(format!("=== DeskBridge Session Started at UTC ===\n").as_bytes());
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

// --- Atomic State Variables ---
pub static KVM_ENABLED: AtomicBool = AtomicBool::new(false);
pub static KVM_ACTIVE: AtomicBool = AtomicBool::new(false); // Are we currently capturing and controlling the client?
pub static BORDER_X: AtomicU16 = AtomicU16::new(1919); // Edge boundary
pub static BORDER_DIRECTION: AtomicU8 = AtomicU8::new(1); // 1 = Right, 0 = Left
pub static SCREEN_WIDTH: AtomicU16 = AtomicU16::new(1920);
pub static SCREEN_HEIGHT: AtomicU16 = AtomicU16::new(1080);
pub static LAST_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static LAST_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);


// Configurable hotkey settings (Defaults to Ctrl + Alt + K)
pub static HOTKEY_CTRL: AtomicBool = AtomicBool::new(true);
pub static HOTKEY_ALT: AtomicBool = AtomicBool::new(true);
pub static HOTKEY_SHIFT: AtomicBool = AtomicBool::new(false);
pub static HOTKEY_KEY: AtomicU16 = AtomicU16::new(11); // 11 corresponds to KeyK

// Flag to prevent multiple concurrent connection attempts
pub static IS_CONNECTING: AtomicBool = AtomicBool::new(false);

// Modifier keys tracking for failsafe release hotkey and custom hotkey
static CTRL_PRESSED: AtomicBool = AtomicBool::new(false);
static ALT_PRESSED: AtomicBool = AtomicBool::new(false);
static SHIFT_PRESSED: AtomicBool = AtomicBool::new(false);

// We store the target IP address when KVM is triggered
static TARGET_IP: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

// Global AppHandle to allow callbacks to emit status updates and start sessions
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

// Dynamic screen size detector
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
    role: String, // "host" (controlling) or "client" (controlled) or "idle"
    target: String,
}

fn get_target_ip() -> String {
    let mutex = TARGET_IP.get_or_init(|| std::sync::Mutex::new(String::new()));
    mutex.lock().unwrap().clone()
}

fn set_target_ip(ip: String) {
    let mutex = TARGET_IP.get_or_init(|| std::sync::Mutex::new(String::new()));
    *mutex.lock().unwrap() = ip;
}

// --- macOS Accessibility Helper ---
#[cfg(target_os = "macos")]
pub fn check_accessibility() -> bool {
    macos_accessibility_client::accessibility::application_is_trusted_with_prompt()
}

#[cfg(not(target_os = "macos"))]
pub fn check_accessibility() -> bool {
    true // Not required on Windows
}

/// Release KVM and restore cursor — called from multiple deactivation paths.
/// Safe to call multiple times (idempotent).
fn deactivate_kvm_host() {
    if KVM_ACTIVE.swap(false, Ordering::SeqCst) {
        // Only release if we were actually active (swap returns previous value)
        platform_release_cursor();
        log_write("INFO", "KVM Host: Deactivated and cursor released.");
    }
}

// --- Key Serialization Mapping (to keep payload small) ---
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

// --- Grab Callback (runs on OS thread) ---
fn grab_callback(event: Event) -> Option<Event> {
    if !KVM_ENABLED.load(Ordering::SeqCst) {
        return Some(event);
    }

    // --- Track modifier key states globally (active or inactive) ---
    match event.event_type {
        EventType::KeyPress(key) => {
            if key == Key::ControlLeft || key == Key::ControlRight {
                CTRL_PRESSED.store(true, Ordering::SeqCst);
            } else if key == Key::Alt || key == Key::AltGr {
                ALT_PRESSED.store(true, Ordering::SeqCst);
            } else if key == Key::ShiftLeft || key == Key::ShiftRight {
                SHIFT_PRESSED.store(true, Ordering::SeqCst);
            }
            
            // Failsafe key check: Ctrl + Alt + Escape (only when active)
            if KVM_ACTIVE.load(Ordering::SeqCst) && key == Key::Escape && CTRL_PRESSED.load(Ordering::SeqCst) && ALT_PRESSED.load(Ordering::SeqCst) {
                log_write("INFO", "KVM Host: Failsafe release hotkey Ctrl+Alt+Esc triggered!");
                deactivate_kvm_host();
                return None;
            }

            // Custom toggle hotkey check
            let code = key_to_u16(key);
            let target_code = HOTKEY_KEY.load(Ordering::SeqCst);
            if code == target_code
                && CTRL_PRESSED.load(Ordering::SeqCst) == HOTKEY_CTRL.load(Ordering::SeqCst)
                && ALT_PRESSED.load(Ordering::SeqCst) == HOTKEY_ALT.load(Ordering::SeqCst)
                && SHIFT_PRESSED.load(Ordering::SeqCst) == HOTKEY_SHIFT.load(Ordering::SeqCst)
            {
                if KVM_ACTIVE.load(Ordering::SeqCst) {
                    log_write("INFO", "KVM Host: Custom hotkey triggered - Releasing control.");
                    deactivate_kvm_host();
                } else {
                    let target = get_target_ip();
                    if !target.is_empty() && !IS_CONNECTING.load(Ordering::SeqCst) {
                        log_write("INFO", "KVM Host: Custom hotkey triggered - Initiating KVM session.");
                        IS_CONNECTING.store(true, Ordering::SeqCst);
                        let ip = target.clone();
                        if let Some(app_handle) = APP_HANDLE.get() {
                            let app_clone = app_handle.clone();
                            thread::spawn(move || {
                                initiate_kvm_control_session(app_clone, ip);
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

    if !KVM_ACTIVE.load(Ordering::SeqCst) {
        // --- Edge Detection ---
        if let EventType::MouseMove { x, y: _ } = event.event_type {
            if !IS_CONNECTING.load(Ordering::SeqCst) {
                let (w, _) = get_screen_size();
                let direction = BORDER_DIRECTION.load(Ordering::SeqCst); // 1 = Right, 0 = Left

                let hit = if direction == 1 {
                    x >= w - 2.0
                } else {
                    x <= 2.0
                };

                if hit {
                    let target = get_target_ip();
                    if !target.is_empty() {
                        IS_CONNECTING.store(true, Ordering::SeqCst);
                        let ip = target.clone();
                        if let Some(app_handle) = APP_HANDLE.get() {
                            let app_clone = app_handle.clone();
                            thread::spawn(move || {
                                initiate_kvm_control_session(app_clone, ip);
                            });
                        }
                    }
                }
            }
        }
        return Some(event);
    }

    // --- Active Session Input Capturing ---
    let mut payload = Vec::with_capacity(9);
    let mut is_kvm_event = true;

    #[allow(unreachable_patterns)]
    match event.event_type {
        EventType::MouseMove { x, y } => {
            // With native cursor capture (ClipCursor/CGAssociate), the cursor is locked.
            // We compute deltas from LAST_X/LAST_Y and send them to the client.
            // No need for "smart warp" — the cursor physically cannot escape.
            let last_x = LAST_X.load(Ordering::SeqCst) as f64;
            let last_y = LAST_Y.load(Ordering::SeqCst) as f64;

            let dx = (x - last_x) as f32;
            let dy = (y - last_y) as f32;

            // Update last known mouse coordinate
            LAST_X.store(x as i32, Ordering::SeqCst);
            LAST_Y.store(y as i32, Ordering::SeqCst);

            // Skip zero-delta events (can happen from ClipCursor bouncing)
            if dx.abs() < 0.001 && dy.abs() < 0.001 {
                return None;
            }

            // Serialize: Type = 0, DX (f32), DY (f32)
            payload.push(0);
            payload.extend_from_slice(&dx.to_le_bytes());
            payload.extend_from_slice(&dy.to_le_bytes());
        }
        EventType::KeyPress(key) => {
            // Modifiers are already tracked globally above
            let key_code = key_to_u16(key);
            payload.push(1); // 1 = PressKey
            payload.extend_from_slice(&key_code.to_le_bytes());
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // padding
        }
        EventType::KeyRelease(key) => {
            // Modifiers are already tracked globally above
            let key_code = key_to_u16(key);
            payload.push(2); // 2 = ReleaseKey
            payload.extend_from_slice(&key_code.to_le_bytes());
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // padding
        }
        EventType::ButtonPress(btn) => {
            let btn_id = match btn {
                Button::Left => 0,
                Button::Right => 1,
                Button::Middle => 2,
                Button::Unknown(code) => code,
            };
            payload.push(3); // 3 = ClickMouse
            payload.push(btn_id);
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // padding
        }
        EventType::ButtonRelease(btn) => {
            let btn_id = match btn {
                Button::Left => 0,
                Button::Right => 1,
                Button::Middle => 2,
                Button::Unknown(code) => code,
            };
            payload.push(4); // 4 = ReleaseMouse
            payload.push(btn_id);
            payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // padding
        }
        EventType::Wheel { delta_x, delta_y } => {
            payload.push(5); // 5 = Scroll
            payload.extend_from_slice(&(delta_x as i32).to_le_bytes());
            payload.extend_from_slice(&(delta_y as i32).to_le_bytes());
        }
        _ => {
            is_kvm_event = false;
        }
    }

    if is_kvm_event {
        // Send binary payload to the TCP socket sender channel
        if let Some(sender) = get_active_sender() {
            let _ = sender.send(payload);
        }
        None // Swallow event: local computer won't move cursor or press keys
    } else {
        Some(event)
    }
}

// --- Initialize Background OS Listener (Runs once) ---
pub fn init_kvm_listener(app_handle: AppHandle) {
    let _ = APP_HANDLE.set(app_handle);
    thread::spawn(|| {
        log_write("INFO", "Starting global input capture listener thread...");
        if let Err(e) = rdev::grab(grab_callback) {
            log_write("ERROR", &format!("Failed to register rdev grab hook: {:?}", e));
        }
    });
}

// Let's rewrite the static communication channel to make it replaceable:
static ACTIVE_SENDER: OnceLock<std::sync::Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>> = OnceLock::new();

fn get_active_sender() -> Option<std::sync::mpsc::Sender<Vec<u8>>> {
    ACTIVE_SENDER.get_or_init(|| std::sync::Mutex::new(None))
        .lock().unwrap().clone()
}

fn set_active_sender(sender: Option<std::sync::mpsc::Sender<Vec<u8>>>) {
    *ACTIVE_SENDER.get_or_init(|| std::sync::Mutex::new(None))
        .lock().unwrap() = sender;
}

// Let's correct initiate_kvm_control_session:
fn initiate_kvm_control_session(app_handle: AppHandle, ip: String) {
    let address = format!("{}:53201", ip);
    log_write("INFO", &format!("KVM Host: Connecting to Client at {}...", address));

    match TcpStream::connect_timeout(&address.parse().unwrap(), Duration::from_secs(3)) {
        Ok(stream) => {
            stream.set_nodelay(true).unwrap();
            
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            set_active_sender(Some(tx));

            // Set KVM Active state
            KVM_ACTIVE.store(true, Ordering::SeqCst);
            IS_CONNECTING.store(false, Ordering::SeqCst);
            
            let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                active: true,
                role: "host".to_string(),
                target: ip.clone(),
            });

            // Warp mouse to center BEFORE capturing, so ClipCursor has a good anchor point
            let (w, h) = get_screen_size();
            let cx = w / 2.0;
            let cy = h / 2.0;
            let _ = simulate(&EventType::MouseMove { x: cx, y: cy });
            // Small delay to let the OS process the warp before we clip
            thread::sleep(Duration::from_millis(50));
            
            // Set initial tracking position
            LAST_X.store(cx as i32, Ordering::SeqCst);
            LAST_Y.store(cy as i32, Ordering::SeqCst);

            // Now capture the cursor using platform-native API
            platform_capture_cursor();

            let mut writer_socket = stream.try_clone().unwrap();
            
            // Spawn a socket writer thread
            let app_clone = app_handle.clone();
            thread::spawn(move || {
                let mut idle_ticks = 0;
                while KVM_ACTIVE.load(Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(data) => {
                            idle_ticks = 0;
                            if let Err(e) = writer_socket.write_all(&data) {
                                log_write("ERROR", &format!("KVM Host: Error writing to socket: {:?}", e));
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            idle_ticks += 1;
                            if idle_ticks >= 10 { // 1 second
                                idle_ticks = 0;
                                let keepalive = [7u8, 0, 0, 0, 0, 0, 0, 0, 0];
                                if let Err(e) = writer_socket.write_all(&keepalive) {
                                    log_write("ERROR", &format!("KVM Host: Keepalive write failed: {:?}", e));
                                    break;
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break;
                        }
                    }
                }
                
                // Connection closed or KVM stopped — ensure cursor is released
                log_write("INFO", "KVM Host: Control session ended.");
                deactivate_kvm_host(); // This releases the cursor via platform API
                set_active_sender(None);
                
                // Warp local mouse slightly away from the border to prevent instant re-trigger
                let direction = BORDER_DIRECTION.load(Ordering::SeqCst);
                let (w, h) = get_screen_size();
                let release_x = if direction == 1 { w - 20.0 } else { 20.0 };
                let _ = simulate(&EventType::MouseMove { x: release_x, y: h / 2.0 });

                let _ = app_clone.emit("kvm-status", KvmStatusUpdate {
                    active: false,
                    role: "idle".to_string(),
                    target: "".to_string(),
                });
            });

            // Spawn a socket reader thread (to listen for ReleaseControl from Client)
            let mut read_socket = stream;
            thread::spawn(move || {
                let mut buf = [0u8; 9];
                loop {
                    match read_socket.read_exact(&mut buf) {
                        Ok(_) => {
                            if buf[0] == 6 {
                                // Received ReleaseControl!
                                log_write("INFO", "KVM Host: Client requested release of control.");
                                deactivate_kvm_host();
                                break;
                            }
                        }
                        Err(e) => {
                            log_write("INFO", &format!("KVM Host Reader: Socket disconnected: {:?}", e));
                            deactivate_kvm_host();
                            break;
                        }
                    }
                }
            });
        }
        Err(e) => {
            log_write("ERROR", &format!("KVM Host: Failed to connect to Client {}: {:?}", address, e));
            IS_CONNECTING.store(false, Ordering::SeqCst);
        }
    }
}

// --- KVM Client (Receiver) Listening Server ---
pub fn start_kvm_client_server(app_handle: AppHandle) {
    thread::spawn(move || {
        let listener = TcpListener::bind("0.0.0.0:53201");
        if let Err(e) = &listener {
            log_write("ERROR", &format!("Failed to bind KVM Client port 53201: {:?}", e));
            return;
        }
        let listener = listener.unwrap();
        log_write("INFO", "KVM Client: Listening for host KVM connections on port 53201...");

        for stream in listener.incoming() {
            match stream {
                Ok(mut socket) => {
                    socket.set_nodelay(true).unwrap();
                    // Increased timeout from 3s to 30s to handle brief network hiccups
                    // without dropping the session. Keepalives arrive every 1s, so
                    // 30s gives ample headroom.
                    socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();

                    let peer_addr = socket.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
                    log_write("INFO", &format!("KVM Client: Connected by Host at {}", peer_addr));

                    let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                        active: true,
                        role: "client".to_string(),
                        target: peer_addr.clone(),
                    });

                    // Set client mouse coordinates to center of client screen initially
                    let (w, h) = get_screen_size();
                    let mut current_x = w / 2.0;
                    let mut current_y = h / 2.0;
                    if let Err(e) = simulate(&EventType::MouseMove { x: current_x, y: current_y }) {
                        log_write("ERROR", &format!("KVM Client: Failed to simulate initial MouseMove: {:?}", e));
                    }

                    let mut write_socket = socket.try_clone().unwrap();
                    let mut buf = [0u8; 9];
                    
                    let mut controlled = true;
                    log_write("INFO", "KVM Client: Control session started.");

                    while controlled {
                        match socket.read_exact(&mut buf) {
                            Ok(_) => {
                                let event_type = buf[0];
                                match event_type {
                                    0 => {
                                         // MouseMove: DX (f32), DY (f32)
                                         let dx = f32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
                                         let dy = f32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);

                                         // We NO LONGER query Mouse::get_mouse_position() because it can fail or return (0, 0)
                                         // on macOS due to lack of window focus or sandbox restrictions, causing coordinate corruptions.
                                         // Relying purely on accumulated memory coordinates is 100% stable and smooth.
                                         current_x += dx as f64;
                                         current_y += dy as f64;

                                         // Boundary Clamping and Release Control Check
                                         let direction = BORDER_DIRECTION.load(Ordering::SeqCst);
                                         let (w, h) = get_screen_size();
                                         let client_limit_triggered = if direction == 1 {
                                             // Host screen is on the LEFT, client screen is on the RIGHT.
                                             // If remote mouse moves off the LEFT border, return control to host.
                                             current_x <= 2.0 && dx < 0.0
                                         } else {
                                             // Host screen is on the RIGHT, client screen is on the LEFT.
                                             // If remote mouse moves off the RIGHT border, return control.
                                             current_x >= w - 2.0 && dx > 0.0
                                         };

                                         if client_limit_triggered {
                                             log_write("INFO", "KVM Client: Screen boundary reached. Releasing control back to Host.");
                                             // Send ReleaseControl command back to Host
                                             let release_packet = [6u8, 0, 0, 0, 0, 0, 0, 0, 0];
                                             let _ = write_socket.write_all(&release_packet);
                                             controlled = false;
                                         } else {
                                             // Clamp values to screen size
                                             current_x = current_x.clamp(0.0, w);
                                             current_y = current_y.clamp(0.0, h);
                                             if let Err(e) = simulate(&EventType::MouseMove { x: current_x, y: current_y }) {
                                                 log_write("ERROR", &format!("KVM Client: Failed to simulate MouseMove: {:?}", e));
                                             }
                                         }
                                    }
                                    1 => {
                                         // KeyPress
                                         let val = u16::from_le_bytes([buf[1], buf[2]]);
                                         let key = u16_to_key(val);
                                         if let Err(e) = simulate(&EventType::KeyPress(key)) {
                                             log_write("ERROR", &format!("KVM Client: Failed to simulate KeyPress: {:?}", e));
                                         }
                                    }
                                    2 => {
                                         // KeyRelease
                                         let val = u16::from_le_bytes([buf[1], buf[2]]);
                                         let key = u16_to_key(val);
                                         if let Err(e) = simulate(&EventType::KeyRelease(key)) {
                                             log_write("ERROR", &format!("KVM Client: Failed to simulate KeyRelease: {:?}", e));
                                         }
                                    }
                                    3 => {
                                         // ButtonPress
                                         let btn_id = buf[1];
                                         let btn = match btn_id {
                                             0 => Button::Left,
                                             1 => Button::Right,
                                             2 => Button::Middle,
                                             code => Button::Unknown(code),
                                         };
                                         if let Err(e) = simulate(&EventType::ButtonPress(btn)) {
                                             log_write("ERROR", &format!("KVM Client: Failed to simulate ButtonPress: {:?}", e));
                                         }
                                    }
                                    4 => {
                                         // ButtonRelease
                                         let btn_id = buf[1];
                                         let btn = match btn_id {
                                             0 => Button::Left,
                                             1 => Button::Right,
                                             2 => Button::Middle,
                                             code => Button::Unknown(code),
                                         };
                                         if let Err(e) = simulate(&EventType::ButtonRelease(btn)) {
                                             log_write("ERROR", &format!("KVM Client: Failed to simulate ButtonRelease: {:?}", e));
                                         }
                                    }
                                    5 => {
                                         // Wheel
                                         let dx = i32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
                                         let dy = i32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]);
                                         if let Err(e) = simulate(&EventType::Wheel { delta_x: dx as i64, delta_y: dy as i64 }) {
                                             log_write("ERROR", &format!("KVM Client: Failed to simulate Wheel: {:?}", e));
                                         }
                                    }
                                    7 => {
                                         // Heartbeat keepalive, do nothing
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                log_write("INFO", &format!("KVM Client: Host disconnected or read timeout: {:?}", e));
                                controlled = false;
                            }
                        }
                    }

                    log_write("INFO", "KVM Client: Control session ended.");
                    let _ = app_handle.emit("kvm-status", KvmStatusUpdate {
                        active: false,
                        role: "idle".to_string(),
                        target: "".to_string(),
                    });
                }
                Err(e) => {
                    log_write("ERROR", &format!("KVM Client: Connection accept error: {:?}", e));
                }
            }
        }
    });
}

// --- Tauri Commands Exposed to React ---

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
