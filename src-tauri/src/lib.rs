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
fn select_file() -> Result<Option<String>, String> {
    let file = rfd::FileDialog::new()
        .pick_file();
    Ok(file.map(|f| f.to_string_lossy().to_string()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(MdnsState::new())
        .setup(|app| {
            let handle = app.handle().clone();
            
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
            p2p::send_file,
            p2p::cancel_file_transfer,
            get_local_info,
            get_discovered_nodes,
            select_file,
            get_network_interfaces
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

