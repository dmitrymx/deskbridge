use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, Duration};
use tauri::{AppHandle, Emitter};
use serde::Serialize;
use sha2::{Sha256, Digest};

static TRANSFER_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
struct FileProgressUpdate {
    #[serde(rename = "transferId")]
    transfer_id: String,
    status: String, // "starting", "processing", "completed", "error", "cancelled"
    #[serde(rename = "fileName")]
    file_name: String,
    progress: f64, // 0.0 to 1.0
    speed: f64,    // MB/s
    error: Option<String>,
    #[serde(rename = "sha256Matches")]
    sha256_matches: Option<bool>,
}

/// Start the background P2P File Receiver server on port 53202
pub fn start_p2p_file_server(app_handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let listener = TcpListener::bind("0.0.0.0:53202").await.expect("Failed to bind P2P file receiver port");
        println!("P2P File Receiver listening on port 53202...");

        while let Ok((mut socket, peer_addr)) = listener.accept().await {
            let app_clone = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let peer_ip = peer_addr.ip().to_string();
                println!("P2P: Incoming file transfer connection from: {}", peer_ip);
                
                if let Err(e) = handle_incoming_file_transfer(app_clone, &mut socket).await {
                    eprintln!("P2P Receiver Error: {:?}", e);
                }
            });
        }
    });
}

/// Handle receiving a file from a remote peer
async fn handle_incoming_file_transfer(app_handle: AppHandle, socket: &mut TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(socket);

    // 1. Read metadata header
    // - File size: u64 (8 bytes)
    // - Name length: u16 (2 bytes)
    let mut header = [0u8; 10];
    reader.read_exact(&mut header).await?;

    let file_size = u64::from_le_bytes([
        header[0], header[1], header[2], header[3],
        header[4], header[5], header[6], header[7]
    ]);
    let name_len = u16::from_le_bytes([header[8], header[9]]) as usize;

    let mut name_buf = vec![0u8; name_len];
    reader.read_exact(&mut name_buf).await?;
    let raw_file_name = String::from_utf8(name_buf)?;
    
    // Sanitize file name (remove path traversals)
    let file_name = Path::new(&raw_file_name)
        .file_name()
        .ok_or("Invalid file name")?
        .to_str()
        .ok_or("Failed to convert filename")?
        .to_string();

    println!("P2P Receiver: Receiving file: '{}' ({} bytes)", file_name, file_size);

    // 2. Resolve Downloads folder path
    // Get downloads directory natively
    let download_dir = dirs::download_dir().unwrap_or_else(|| {
        dirs::home_dir().map(|h| h.join("Downloads")).unwrap_or_else(|| PathBuf::from("."))
    });
    
    let mut target_path = download_dir.join(&file_name);
    
    // Rename if file already exists (e.g. file.txt -> file_1.txt)
    let mut count = 1;
    let stem = Path::new(&file_name).file_stem().unwrap_or_default().to_str().unwrap_or("");
    let extension = Path::new(&file_name).extension().unwrap_or_default().to_str().unwrap_or("");
    while target_path.exists() {
        let new_name = if extension.is_empty() {
            format!("{}_{}", stem, count)
        } else {
            format!("{}_{}.{}", stem, count, extension)
        };
        target_path = download_dir.join(new_name);
        count += 1;
    }

    println!("Saving file to: {:?}", target_path);

    // 3. Prepare transmission loop & progress tracker
    let file = File::create(&target_path).await?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    
    let transfer_id = format!("recv_{}", Instant::now().elapsed().as_nanos());
    
    let _ = app_handle.emit("file-progress", FileProgressUpdate {
        transfer_id: transfer_id.clone(),
        status: "starting".to_string(),
        file_name: file_name.clone(),
        progress: 0.0,
        speed: 0.0,
        error: None,
        sha256_matches: None,
    });

    let mut buffer = [0u8; 65536]; // 64 KB buffer
    let mut received_bytes: u64 = 0;
    
    let start_time = Instant::now();
    let mut last_emit = Instant::now();
    
    TRANSFER_CANCELLED.store(false, Ordering::SeqCst);

    while received_bytes < file_size {
        if TRANSFER_CANCELLED.load(Ordering::SeqCst) {
            let _ = app_handle.emit("file-progress", FileProgressUpdate {
                transfer_id: transfer_id.clone(),
                status: "cancelled".to_string(),
                file_name: file_name.clone(),
                progress: (received_bytes as f64 / file_size as f64),
                speed: 0.0,
                error: Some("Transfer cancelled by user".to_string()),
                sha256_matches: None,
            });
            return Err("cancelled".into());
        }

        // Read up to 64KB
        let bytes_to_read = std::cmp::min(buffer.len() as u64, file_size - received_bytes) as usize;
        let bytes_read = reader.read_exact(&mut buffer[..bytes_to_read]).await?;
        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read]).await?;
        hasher.update(&buffer[..bytes_read]);
        
        received_bytes += bytes_read as u64;

        // Emit progress every 200ms
        if last_emit.elapsed() >= Duration::from_millis(200) {
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                (received_bytes as f64 / (1024.0 * 1024.0)) / elapsed
            } else {
                0.0
            };

            let _ = app_handle.emit("file-progress", FileProgressUpdate {
                transfer_id: transfer_id.clone(),
                status: "processing".to_string(),
                file_name: file_name.clone(),
                progress: (received_bytes as f64 / file_size as f64),
                speed,
                error: None,
                sha256_matches: None,
            });
            last_emit = Instant::now();
        }
    }

    writer.flush().await?;

    // 4. Verify SHA-256
    let local_hash = hasher.finalize();
    let mut remote_hash = [0u8; 32];
    reader.read_exact(&mut remote_hash).await?;

    let is_match = local_hash[..] == remote_hash[..];
    println!("P2P: Received file verification. Checksum matched: {}", is_match);

    let total_elapsed = start_time.elapsed().as_secs_f64();
    let speed = if total_elapsed > 0.0 {
        (file_size as f64 / (1024.0 * 1024.0)) / total_elapsed
    } else {
        0.0
    };

    let _ = app_handle.emit("file-progress", FileProgressUpdate {
        transfer_id: transfer_id.clone(),
        status: if is_match { "completed".to_string() } else { "error".to_string() },
        file_name: file_name.clone(),
        progress: 1.0,
        speed,
        error: if is_match { None } else { Some("SHA-256 hash mismatch".to_string()) },
        sha256_matches: Some(is_match),
    });

    Ok(())
}

/// Send a local file to a remote node
#[tauri::command]
pub async fn send_file(app_handle: AppHandle, target_ip: String, file_path: String) -> Result<String, String> {
    let path = Path::new(&file_path);
    if !path.exists() {
        return Err("File does not exist".to_string());
    }

    let file_name = path.file_name()
        .ok_or_else(|| "Invalid file name".to_string())?
        .to_str()
        .ok_or_else(|| "Filename coding error".to_string())?
        .to_string();

    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let file_size = meta.len();

    let transfer_id = format!("send_{}", Instant::now().elapsed().as_nanos());
    
    // Connect to remote peer's file receiver port (53202)
    let address = format!("{}:53202", target_ip);
    let mut socket = TcpStream::connect(&address).await.map_err(|e| format!("Failed to connect to remote: {}", e))?;
    
    let mut writer = BufWriter::new(&mut socket);

    // 1. Send header: size (8 bytes) + name length (2 bytes) + file name
    let name_bytes = file_name.as_bytes();
    let name_len = name_bytes.len() as u16;

    let mut header = Vec::with_capacity(10 + name_bytes.len());
    header.extend_from_slice(&file_size.to_le_bytes());
    header.extend_from_slice(&name_len.to_le_bytes());
    header.extend_from_slice(name_bytes);
    
    writer.write_all(&header).await.map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())?;

    // 2. Stream file content
    let mut file = File::open(path).await.map_err(|e| e.to_string())?;
    let mut buffer = [0u8; 65536]; // 64 KB
    let mut bytes_sent: u64 = 0;
    
    let start_time = Instant::now();
    let mut last_emit = Instant::now();
    let mut hasher = Sha256::new();

    let _ = app_handle.emit("file-progress", FileProgressUpdate {
        transfer_id: transfer_id.clone(),
        status: "starting".to_string(),
        file_name: file_name.clone(),
        progress: 0.0,
        speed: 0.0,
        error: None,
        sha256_matches: None,
    });

    TRANSFER_CANCELLED.store(false, Ordering::SeqCst);

    loop {
        if TRANSFER_CANCELLED.load(Ordering::SeqCst) {
            let _ = app_handle.emit("file-progress", FileProgressUpdate {
                transfer_id: transfer_id.clone(),
                status: "cancelled".to_string(),
                file_name: file_name.clone(),
                progress: (bytes_sent as f64 / file_size as f64),
                speed: 0.0,
                error: Some("Transfer cancelled by user".to_string()),
                sha256_matches: None,
            });
            return Err("cancelled".to_string());
        }

        let bytes_read = file.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read]).await.map_err(|e| e.to_string())?;
        hasher.update(&buffer[..bytes_read]);

        bytes_sent += bytes_read as u64;

        if last_emit.elapsed() >= Duration::from_millis(200) {
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                (bytes_sent as f64 / (1024.0 * 1024.0)) / elapsed
            } else {
                0.0
            };

            let _ = app_handle.emit("file-progress", FileProgressUpdate {
                transfer_id: transfer_id.clone(),
                status: "processing".to_string(),
                file_name: file_name.clone(),
                progress: (bytes_sent as f64 / file_size as f64),
                speed,
                error: None,
                sha256_matches: None,
            });
            last_emit = Instant::now();
        }
    }

    writer.flush().await.map_err(|e| e.to_string())?;

    // 3. Send SHA-256 hash (32 bytes) for integrity verification
    let hash_result = hasher.finalize();
    writer.write_all(&hash_result).await.map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())?;

    let total_elapsed = start_time.elapsed().as_secs_f64();
    let speed = if total_elapsed > 0.0 {
        (file_size as f64 / (1024.0 * 1024.0)) / total_elapsed
    } else {
        0.0
    };

    let _ = app_handle.emit("file-progress", FileProgressUpdate {
        transfer_id: transfer_id.clone(),
        status: "completed".to_string(),
        file_name: file_name.clone(),
        progress: 1.0,
        speed,
        error: None,
        sha256_matches: None,
    });

    Ok("file_sent_successfully".to_string())
}

#[tauri::command]
pub fn cancel_file_transfer() -> Result<String, String> {
    TRANSFER_CANCELLED.store(true, Ordering::SeqCst);
    Ok("cancelled".to_string())
}

/// Start the background Web Portal server on port 53203
pub fn start_web_portal(app_handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let listener = TcpListener::bind("0.0.0.0:53203").await.expect("Failed to bind Web Portal port");
        println!("Web Portal listening on port 53203...");

        while let Ok((socket, peer_addr)) = listener.accept().await {
            let app_clone = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let peer_ip = peer_addr.ip().to_string();
                println!("Web Portal: Connection from: {}", peer_ip);
                
                if let Err(e) = handle_web_request(app_clone, socket).await {
                    eprintln!("Web Portal Request Error: {:?}", e);
                }
            });
        }
    });
}

async fn handle_web_request(app_handle: AppHandle, mut socket: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut header_buf = Vec::new();
    let mut temp_buf = [0u8; 1024];
    let mut header_end_pos = None;

    loop {
        let n = socket.read(&mut temp_buf).await?;
        if n == 0 {
            break;
        }
        header_buf.extend_from_slice(&temp_buf[..n]);
        
        if let Some(pos) = find_subsequence(&header_buf, b"\r\n\r\n") {
            header_end_pos = Some(pos);
            break;
        }

        if header_buf.len() > 16384 {
            let response = "HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).await?;
            return Ok(());
        }
    }

    let header_end = match header_end_pos {
        Some(pos) => pos,
        None => return Ok(()),
    };

    let headers_part = &header_buf[..header_end];
    let initial_body = &header_buf[header_end + 4..];

    let header_str = String::from_utf8_lossy(headers_part);
    let mut lines = header_str.lines();
    
    let req_line = match lines.next() {
        Some(line) => line,
        None => return Ok(()),
    };
    
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    
    let method = parts[0];
    let uri = parts[1];

    if method == "GET" && (uri == "/" || uri == "/index.html") {
        let html = get_portal_html();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
    } else if method == "POST" && uri.starts_with("/upload") {
        let name_param = if let Some(query_idx) = uri.find('?') {
            let query = &uri[query_idx + 1..];
            let mut name = None;
            for param in query.split('&') {
                let kv: Vec<&str> = param.split('=').collect();
                if kv.len() == 2 && (kv[0] == "name" || kv[0] == "filename") {
                    name = Some(url_decode(kv[1]));
                    break;
                }
            }
            name
        } else {
            None
        };

        let raw_file_name = name_param.unwrap_or_else(|| "uploaded_file".to_string());
        
        let file_name = Path::new(&raw_file_name)
            .file_name()
            .ok_or("Invalid file name")?
            .to_str()
            .ok_or("Failed to convert filename")?
            .to_string();

        let mut content_length = 0;
        for line in lines {
            if line.to_lowercase().starts_with("content-length:") {
                if let Some(val_str) = line.split(':').nth(1) {
                    content_length = val_str.trim().parse::<u64>().unwrap_or(0);
                }
            }
        }

        if content_length == 0 {
            let response = "HTTP/1.1 411 Length Required\r\nConnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).await?;
            return Ok(());
        }

        let download_dir = dirs::download_dir().unwrap_or_else(|| {
            dirs::home_dir().map(|h| h.join("Downloads")).unwrap_or_else(|| PathBuf::from("."))
        });
        
        let mut target_path = download_dir.join(&file_name);
        
        let mut count = 1;
        let stem = Path::new(&file_name).file_stem().unwrap_or_default().to_str().unwrap_or("");
        let extension = Path::new(&file_name).extension().unwrap_or_default().to_str().unwrap_or("");
        while target_path.exists() {
            let new_name = if extension.is_empty() {
                format!("{}_{}", stem, count)
            } else {
                format!("{}_{}.{}", stem, count, extension)
            };
            target_path = download_dir.join(new_name);
            count += 1;
        }

        println!("Web Portal saving file to: {:?}", target_path);

        let file = File::create(&target_path).await?;
        let mut writer = BufWriter::new(file);
        
        let transfer_id = format!("web_recv_{}", Instant::now().elapsed().as_nanos());
        
        let _ = app_handle.emit("file-progress", FileProgressUpdate {
            transfer_id: transfer_id.clone(),
            status: "starting".to_string(),
            file_name: file_name.clone(),
            progress: 0.0,
            speed: 0.0,
            error: None,
            sha256_matches: None,
        });

        let mut received_bytes = 0;
        
        let initial_write_len = std::cmp::min(initial_body.len() as u64, content_length) as usize;
        if initial_write_len > 0 {
            writer.write_all(&initial_body[..initial_write_len]).await?;
            received_bytes += initial_write_len as u64;
        }

        let start_time = Instant::now();
        let mut last_emit = Instant::now();
        let mut buffer = [0u8; 65536];

        while received_bytes < content_length {
            let bytes_to_read = std::cmp::min(buffer.len() as u64, content_length - received_bytes) as usize;
            let bytes_read = socket.read(&mut buffer[..bytes_to_read]).await?;
            if bytes_read == 0 {
                break;
            }

            writer.write_all(&buffer[..bytes_read]).await?;
            received_bytes += bytes_read as u64;

            if last_emit.elapsed() >= Duration::from_millis(200) {
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (received_bytes as f64 / (1024.0 * 1024.0)) / elapsed
                } else {
                    0.0
                };

                let _ = app_handle.emit("file-progress", FileProgressUpdate {
                    transfer_id: transfer_id.clone(),
                    status: "processing".to_string(),
                    file_name: file_name.clone(),
                    progress: (received_bytes as f64 / content_length as f64),
                    speed,
                    error: None,
                    sha256_matches: None,
                });
                last_emit = Instant::now();
            }
        }

        writer.flush().await?;

        let total_elapsed = start_time.elapsed().as_secs_f64();
        let speed = if total_elapsed > 0.0 {
            (content_length as f64 / (1024.0 * 1024.0)) / total_elapsed
        } else {
            0.0
        };

        let _ = app_handle.emit("file-progress", FileProgressUpdate {
            transfer_id: transfer_id.clone(),
            status: "completed".to_string(),
            file_name: file_name.clone(),
            progress: 1.0,
            speed,
            error: None,
            sha256_matches: Some(true),
        });

        let response = "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
    } else {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
    }

    Ok(())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::new();
    let mut i = 0;
    let s_bytes = s.as_bytes();
    while i < s_bytes.len() {
        if s_bytes[i] == b'%' && i + 2 < s_bytes.len() {
            let hex_str = match std::str::from_utf8(&s_bytes[i + 1..i + 3]) {
                Ok(h) => h,
                Err(_) => "",
            };
            if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                bytes.push(val);
                i += 3;
                continue;
            }
        }
        if s_bytes[i] == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(s_bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn get_portal_html() -> &'static str {
    r##"<!DOCTYPE html>
<html lang="ru">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>DeskBridge Web Portal</title>
    <style>
        :root {
            --bg-color: #0d0e12;
            --card-bg: #15161c;
            --accent-color: #6366f1;
            --accent-glow: rgba(99, 102, 241, 0.15);
            --text-color: #f3f4f6;
            --text-muted: #9ca3af;
            --border-color: #27272a;
            --success-color: #10b981;
            --error-color: #ef4444;
        }
        * {
            box-sizing: border-box;
            margin: 0;
            padding: 0;
        }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            background-color: var(--bg-color);
            color: var(--text-color);
            min-height: 100vh;
            display: flex;
            flex-direction: column;
            align-items: center;
            justify-content: center;
            padding: 20px;
            overflow-x: hidden;
            position: relative;
        }
        body::before {
            content: "";
            position: absolute;
            top: -10%;
            left: -10%;
            width: 300px;
            height: 300px;
            border-radius: 50%;
            background: rgba(99, 102, 241, 0.08);
            filter: blur(80px);
            pointer-events: none;
            z-index: 0;
        }
        body::after {
            content: "";
            position: absolute;
            bottom: -10%;
            right: -10%;
            width: 300px;
            height: 300px;
            border-radius: 50%;
            background: rgba(147, 51, 234, 0.08);
            filter: blur(80px);
            pointer-events: none;
            z-index: 0;
        }
        .container {
            width: 100%;
            max-width: 480px;
            background-color: var(--card-bg);
            border: 1px solid var(--border-color);
            border-radius: 24px;
            padding: 30px;
            box-shadow: 0 10px 30px -10px rgba(0, 0, 0, 0.5);
            z-index: 1;
            backdrop-filter: blur(10px);
        }
        header {
            text-align: center;
            margin-bottom: 25px;
        }
        .logo {
            display: inline-flex;
            align-items: center;
            justify-content: center;
            width: 48px;
            height: 48px;
            border-radius: 14px;
            background: linear-gradient(135deg, #6366f1 0%, #a855f7 100%);
            color: white;
            font-weight: bold;
            font-size: 20px;
            margin-bottom: 12px;
            box-shadow: 0 4px 14px var(--accent-glow);
        }
        h1 {
            font-size: 22px;
            font-weight: 800;
            background: linear-gradient(to right, #ffffff, #d1d5db);
            -webkit-background-clip: text;
            -webkit-text-fill-color: transparent;
            margin-bottom: 6px;
        }
        .subtitle {
            font-size: 13px;
            color: var(--text-muted);
        }
        .dropzone {
            border: 2px dashed #3f3f46;
            border-radius: 18px;
            padding: 40px 20px;
            text-align: center;
            cursor: pointer;
            transition: all 0.2s ease;
            background-color: rgba(255, 255, 255, 0.01);
            margin-bottom: 20px;
            position: relative;
            display: block;
        }
        .dropzone:hover, .dropzone.dragover {
            border-color: var(--accent-color);
            background-color: rgba(99, 102, 241, 0.04);
        }
        .dropzone svg {
            width: 40px;
            height: 40px;
            color: var(--accent-color);
            margin-bottom: 12px;
            transition: transform 0.2s ease;
        }
        .dropzone:hover svg {
            transform: translateY(-2px);
        }
        .dropzone p {
            font-size: 14px;
            font-weight: 600;
            color: #e5e7eb;
            margin-bottom: 4px;
        }
        .dropzone span {
            font-size: 11px;
            color: var(--text-muted);
        }
        #file-input {
            display: none;
        }
        .progress-container {
            display: none;
            background: rgba(0, 0, 0, 0.2);
            border: 1px solid var(--border-color);
            border-radius: 16px;
            padding: 16px;
            margin-bottom: 20px;
        }
        .progress-info {
            display: flex;
            justify-content: space-between;
            font-size: 12px;
            margin-bottom: 8px;
        }
        .file-name {
            font-weight: 600;
            max-width: 70%;
            overflow: hidden;
            text-overflow: ellipsis;
            white-space: nowrap;
        }
        .progress-bar-bg {
            width: 100%;
            height: 6px;
            background-color: #27272a;
            border-radius: 3px;
            overflow: hidden;
            margin-bottom: 8px;
        }
        .progress-bar-fill {
            width: 0%;
            height: 100%;
            background: linear-gradient(90deg, #6366f1, #a855f7);
            border-radius: 3px;
            transition: width 0.1s ease;
        }
        .progress-stats {
            display: flex;
            justify-content: space-between;
            font-size: 11px;
            color: var(--text-muted);
            font-family: monospace;
        }
        .alert {
            display: none;
            padding: 12px 16px;
            border-radius: 12px;
            font-size: 12px;
            font-weight: 500;
            margin-bottom: 20px;
            align-items: center;
            gap: 8px;
        }
        .alert-success {
            background-color: rgba(16, 185, 129, 0.1);
            border: 1px solid rgba(16, 185, 129, 0.2);
            color: #34d399;
        }
        .alert-error {
            background-color: rgba(239, 68, 68, 0.1);
            border: 1px solid rgba(239, 68, 68, 0.2);
            color: #f87171;
        }
        .developer-section {
            border-top: 1px solid var(--border-color);
            padding-top: 20px;
            margin-top: 10px;
            text-align: center;
        }
        .developer-title {
            font-size: 12px;
            text-transform: uppercase;
            letter-spacing: 0.05em;
            color: var(--text-muted);
            margin-bottom: 8px;
            font-weight: 700;
        }
        .developer-name {
            font-size: 15px;
            font-weight: 600;
            color: #f3f4f6;
            margin-bottom: 12px;
        }
        .links-container {
            display: flex;
            gap: 12px;
            justify-content: center;
        }
        .btn {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 10px 18px;
            border-radius: 12px;
            font-size: 13px;
            font-weight: 600;
            text-decoration: none;
            transition: all 0.2s ease;
            border: 1px solid transparent;
            cursor: pointer;
        }
        .btn-tg {
            background-color: #24A1DE;
            color: white;
            box-shadow: 0 4px 10px rgba(36, 161, 222, 0.2);
        }
        .btn-tg:hover {
            background-color: #208ec4;
            transform: translateY(-1px);
        }
        .btn-site {
            background-color: rgba(255, 255, 255, 0.05);
            border-color: var(--border-color);
            color: #e5e7eb;
        }
        .btn-site:hover {
            background-color: rgba(255, 255, 255, 0.1);
            border-color: #3f3f46;
            color: white;
            transform: translateY(-1px);
        }
        footer {
            margin-top: 25px;
            font-size: 10px;
            color: #52525b;
            font-family: monospace;
            text-align: center;
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <div class="logo">DB</div>
            <h1>DeskBridge Portal</h1>
            <p class="subtitle">Отправляйте файлы на компьютер по локальной сети</p>
        </header>

        <div class="alert alert-success" id="success-alert">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"></path><polyline points="22 4 12 14.01 9 11.01"></polyline></svg>
            <span style="margin-left: 8px;">Файл успешно загружен в папку "Загрузки"!</span>
        </div>

        <div class="alert alert-error" id="error-alert">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"></circle><line x1="15" y1="9" x2="9" y2="15"></line><line x1="9" y1="9" x2="15" y2="15"></line></svg>
            <span id="error-message" style="margin-left: 8px;">Ошибка при передаче файла.</span>
        </div>

        <label class="dropzone" id="dropzone">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path>
                <polyline points="17 8 12 3 7 8"></polyline>
                <line x1="12" y1="3" x2="12" y2="15"></line>
            </svg>
            <p>Выбрать или перетащить файл</p>
            <span>Работает напрямую с iPhone и Android</span>
            <input type="file" id="file-input">
        </label>

        <div class="progress-container" id="progress-container">
            <div class="progress-info">
                <span class="file-name" id="progress-file-name">file.jpg</span>
                <span id="progress-percent">0%</span>
            </div>
            <div class="progress-bar-bg">
                <div class="progress-bar-fill" id="progress-bar-fill"></div>
            </div>
            <div class="progress-stats">
                <span id="progress-loaded">0 MB из 0 MB</span>
                <span id="progress-speed">0 MB/s</span>
            </div>
        </div>

        <div class="developer-section">
            <div class="developer-title">О разработчике</div>
            <div class="developer-name">Максимов Д.А.</div>
            <div class="links-container">
                <a href="https://t.me/dmitrymx" class="btn btn-tg" target="_blank">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" style="vertical-align: middle;"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm4.64 6.8c-.15 1.58-.8 5.42-1.13 7.19-.14.75-.42 1-.68 1.03-.58.05-1.02-.38-1.58-.75-.88-.58-1.38-.94-2.23-1.5-.99-.65-.35-1.01.22-1.59.15-.15 2.71-2.48 2.76-2.69.01-.03.01-.14-.07-.2-.08-.06-.19-.04-.28-.02-.11.02-1.93 1.23-5.46 3.62-.51.35-.98.53-1.39.51-.46-.01-1.35-.26-2.01-.48-.81-.27-1.46-.42-1.4-.88.03-.24.37-.49 1.03-.75 4.04-1.76 6.74-2.92 8.09-3.48 3.85-1.6 4.64-1.88 5.17-1.89.11 0 .37.03.54.17.14.12.18.28.2.45-.02.07-.02.13-.03.2z"/></svg>
                    Telegram
                </a>
                <a href="https://mxmvdev.ru" class="btn btn-site" target="_blank">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="vertical-align: middle;"><circle cx="12" cy="12" r="10"></circle><line x1="2" y1="12" x2="22" y2="12"></line><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"></path></svg>
                    Сайт mxmvdev.ru
                </a>
            </div>
        </div>
    </div>
    <footer>
        DeskBridge Portal • Локальное соединение без серверов
    </footer>

    <script>
        const dropzone = document.getElementById('dropzone');
        const fileInput = document.getElementById('file-input');
        const progressContainer = document.getElementById('progress-container');
        const progressBarFill = document.getElementById('progress-bar-fill');
        const progressFileName = document.getElementById('progress-file-name');
        const progressPercent = document.getElementById('progress-percent');
        const progressLoaded = document.getElementById('progress-loaded');
        const progressSpeed = document.getElementById('progress-speed');
        const successAlert = document.getElementById('success-alert');
        const errorAlert = document.getElementById('error-alert');
        const errorMessage = document.getElementById('error-message');

        ['dragenter', 'dragover', 'dragleave', 'drop'].forEach(eventName => {
            dropzone.addEventListener(eventName, preventDefaults, false);
            document.body.addEventListener(eventName, preventDefaults, false);
        });

        function preventDefaults(e) {
            e.preventDefault();
            e.stopPropagation();
        }

        ['dragenter', 'dragover'].forEach(eventName => {
            dropzone.addEventListener(eventName, () => dropzone.classList.add('dragover'), false);
        });

        ['dragleave', 'drop'].forEach(eventName => {
            dropzone.addEventListener(eventName, () => dropzone.classList.remove('dragover'), false);
        });

        dropzone.addEventListener('drop', (e) => {
            const dt = e.dataTransfer;
            const files = dt.files;
            if (files.length > 0) {
                handleFile(files[0]);
            }
        });

        fileInput.addEventListener('change', (e) => {
            if (fileInput.files.length > 0) {
                handleFile(fileInput.files[0]);
            }
        });

        function formatBytes(bytes, decimals = 2) {
            if (bytes === 0) return '0 B';
            const k = 1024;
            const dm = decimals < 0 ? 0 : decimals;
            const sizes = ['B', 'KB', 'MB', 'GB'];
            const i = Math.floor(Math.log(bytes) / Math.log(k));
            return parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + ' ' + sizes[i];
        }

        function handleFile(file) {
            successAlert.style.display = 'none';
            errorAlert.style.display = 'none';
            progressContainer.style.display = 'block';
            dropzone.style.display = 'none';

            progressFileName.textContent = file.name;
            
            const xhr = new XMLHttpRequest();
            const startTime = Date.now();

            xhr.upload.addEventListener('progress', (e) => {
                if (e.lengthComputable) {
                    const percent = Math.round((e.loaded / e.total) * 100);
                    progressBarFill.style.width = percent + '%';
                    progressPercent.textContent = percent + '%';
                    
                    const elapsed = (Date.now() - startTime) / 1000;
                    const speed = elapsed > 0 ? (e.loaded / (1024 * 1024)) / elapsed : 0;
                    
                    progressLoaded.textContent = formatBytes(e.loaded) + ' из ' + formatBytes(e.total);
                    progressSpeed.textContent = speed.toFixed(2) + ' MB/s';
                }
            });

            xhr.addEventListener('load', () => {
                progressContainer.style.display = 'none';
                dropzone.style.display = 'block';
                fileInput.value = '';

                if (xhr.status >= 200 && xhr.status < 300) {
                    successAlert.style.display = 'block';
                } else {
                    errorMessage.textContent = 'Ошибка сервера: код ' + xhr.status;
                    errorAlert.style.display = 'block';
                }
            });

            xhr.addEventListener('error', () => {
                progressContainer.style.display = 'none';
                dropzone.style.display = 'block';
                fileInput.value = '';
                errorMessage.textContent = 'Ошибка сети при передаче файла.';
                errorAlert.style.display = 'block';
            });

            const filenameEncoded = encodeURIComponent(file.name);
            xhr.open('POST', '/upload?name=' + filenameEncoded, true);
            xhr.send(file);
        }
    </script>
</body>
</html>"##
}

