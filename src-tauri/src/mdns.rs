use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::sync::Mutex;
use std::thread;
use tauri::{AppHandle, Emitter, Manager};
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DiscoveredNode {
    pub hostname: String,
    pub ip: String,
    pub port: u16,
}

pub struct MdnsState {
    pub daemon: ServiceDaemon,
    pub discovered_nodes: Mutex<Vec<DiscoveredNode>>,
}

impl MdnsState {
    pub fn new() -> Self {
        let daemon = ServiceDaemon::new().expect("Failed to create mDNS daemon");
        Self {
            daemon,
            discovered_nodes: Mutex::new(Vec::new()),
        }
    }
}

/// Registers our local DeskBridge service on the LAN and starts scanning for neighbors
pub fn start_mdns(app: AppHandle, local_port: u16) {
    let state = app.state::<MdnsState>();
    let daemon = state.daemon.clone();

    // 1. Get local hostname and IP address
    let hostname = gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "unknown-host".to_string());
    
    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());

    crate::kvm::log_write("INFO", &format!("Registering mDNS: Hostname: {}, IP: {}, Port: {}", hostname, local_ip, local_port));

    // Clean up hostname to make a valid service instance name
    let clean_hostname = hostname.replace('.', "_");
    let service_type = "_deskbridge._tcp.local.";
    let instance_name = format!("{}.{}", clean_hostname, service_type);
    let properties = [("app", "deskbridge")];

    // 2. Register service
    let my_service = ServiceInfo::new(
        service_type,
        &instance_name,
        &format!("{}.local.", clean_hostname),
        &local_ip,
        local_port,
        &properties[..],
    ).expect("Failed to construct mDNS service info");

    daemon.register(my_service).expect("Failed to register mDNS service");

    // 3. Scan for other DeskBridge instances
    let receiver = daemon.browse(service_type).expect("Failed to browse mDNS services");
    let app_clone = app.clone();

    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let ip = info.get_addresses().iter().next().map(|ip| ip.to_string()).unwrap_or_default();
                    let port = info.get_port();
                    
                    // Ignore our own registered service
                    if ip == local_ip && port == local_port {
                        continue;
                    }

                    // Extract short hostname
                    let resolved_hostname = info.get_fullname()
                        .split('.')
                        .next()
                        .unwrap_or(&info.get_fullname())
                        .to_string();

                    let node = DiscoveredNode {
                        hostname: resolved_hostname,
                        ip: ip.clone(),
                        port,
                    };

                    crate::kvm::log_write("INFO", &format!("mDNS: Resolved neighboring DeskBridge node: {:?}", node));

                    // Update local state list of nodes
                    let state = app_clone.state::<MdnsState>();
                    {
                        let mut nodes = state.discovered_nodes.lock().unwrap();
                        // Add if not already present (checking by IP and Port)
                        if !nodes.iter().any(|n| n.ip == node.ip && n.port == node.port) {
                            nodes.push(node.clone());
                        }
                    }

                    // Emit event to frontend React UI
                    let _ = app_clone.emit("node-resolved", node);
                }
                ServiceEvent::SearchStopped(_) => {
                    break;
                }
                _ => {}
            }
        }
    });
}
