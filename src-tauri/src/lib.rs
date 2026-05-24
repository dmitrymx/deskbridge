mod mdns;
mod kvm;
mod p2p;

use mdns::MdnsState;
use tauri::Manager;

use std::net::IpAddr;

fn is_virtual_interface(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("loopback") 
        || lower.contains("wsl") 
        || lower.contains("vbox") 
        || lower.contains("virtualbox") 
        || lower.contains("vmware") 
        || lower.contains("vethernet") 
        || lower.contains("pseudo")
        || lower.contains("teredo")
        || lower.contains("isatap")
}

fn get_robust_local_ip() -> String {
    if let Ok(ip) = local_ip_address::local_ip() {
        return ip.to_string();
    }
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in interfaces {
            if let IpAddr::V4(ipv4) = ip {
                if !ipv4.is_loopback() && !is_virtual_interface(&name) {
                    return ipv4.to_string();
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

#[tauri::command]
fn get_local_info() -> Result<serde_json::Value, String> {
    let hostname = gethostname::gethostname().into_string().unwrap_or_else(|_| "unknown".to_string());
    let ip = get_robust_local_ip();
    Ok(serde_json::json!({
        "hostname": hostname,
        "ip": ip
    }))
}

#[tauri::command]
fn get_network_interfaces() -> Result<Vec<serde_json::Value>, String> {
    let mut list = Vec::new();
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in interfaces {
            if let IpAddr::V4(ipv4) = ip {
                if !ipv4.is_loopback() {
                    list.push(serde_json::json!({
                        "name": name,
                        "ip": ipv4.to_string(),
                        "is_virtual": is_virtual_interface(&name)
                    }));
                }
            }
        }
    }
    Ok(list)
}

#[tauri::command]
fn get_discovered_nodes(app_handle: tauri::AppHandle) -> Result<Vec<mdns::DiscoveredNode>, String> {
    let state = app_handle.state::<MdnsState>();
    let nodes = state.discovered_nodes.lock().unwrap().clone();
    Ok(nodes)
}

#[tauri::command]
fn get_log_content() -> Result<String, String> {
    let log_path = kvm::LOG_FILE_PATH.get().ok_or("Logger not initialized")?;
    if !log_path.exists() {
        return Ok("No logs recorded yet.".to_string());
    }
    std::fs::read_to_string(log_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_logs() -> Result<(), String> {
    let log_path = kvm::LOG_FILE_PATH.get().ok_or("Logger not initialized")?;
    if log_path.exists() {
        let _ = std::fs::remove_file(log_path);
    }
    kvm::log_write("INFO", "Logs cleared by user.");
    Ok(())
}

#[tauri::command]
fn save_log_file(dest_path: String) -> Result<bool, String> {
    let log_path = kvm::LOG_FILE_PATH.get().ok_or("Logger not initialized")?.clone();
    if !log_path.exists() {
        return Err("Log file does not exist yet".to_string());
    }
    let dest = std::path::Path::new(&dest_path);
    std::fs::copy(&log_path, dest).map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(MdnsState::new())
        .setup(|app| {
            let handle = app.handle().clone();
            
            // Initialize global logger
            kvm::init_logger(&handle);
            
            // Install global panic hook — catches ALL crashes/panics in ANY thread
            // and writes full details to deskbridge.log before the app dies
            std::panic::set_hook(Box::new(|panic_info| {
                let location = panic_info.location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "unknown location".to_string());
                
                let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Unknown panic payload".to_string()
                };

                let backtrace = std::backtrace::Backtrace::force_capture();

                kvm::log_write("FATAL", &format!(
                    "PANIC at {}: {}\nBacktrace:\n{}", location, message, backtrace
                ));
            }));
            
            // Start mDNS scan and register local service on port 53200
            mdns::start_mdns(handle.clone(), 53200);
            
            // Initialize global OS input listener (runs on background thread)
            kvm::init_kvm_listener(handle.clone());
            
            // Start client KVM listening server
            kvm::start_kvm_client_server(handle.clone());
            
            // Start file transfer receiver listener
            p2p::start_p2p_file_server(handle.clone());
            
            // Start the Web Portal server for iOS transfers on port 53203
            p2p::start_web_portal(handle.clone());
            
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            kvm::configure_kvm,
            kvm::trigger_manual_control,
            kvm::release_manual_control,
            kvm::set_kvm_hotkey,
            p2p::send_file,
            p2p::cancel_file_transfer,
            p2p::pick_file_dialog,
            get_local_info,
            get_discovered_nodes,
            get_network_interfaces,
            get_log_content,
            clear_logs,
            save_log_file
        ])
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                kvm::log_write("INFO", "Window close requested. Releasing KVM and cleaning up...");
                // Ensure cursor is released before process exit
                kvm::release_manual_control().ok();
                kvm::log_write("INFO", "Cleanup complete. Force exiting process.");
                std::process::exit(0);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
