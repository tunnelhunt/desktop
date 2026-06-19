use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::io::AsyncBufReadExt;
use tauri::{Manager, Emitter};
use tauri::tray::{TrayIconBuilder, TrayIconEvent, MouseButton, MouseButtonState};
use tauri_plugin_positioner::{WindowExt, Position};
use std::time::{Instant, Duration};

// Preset data structure
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub port: u16,
    #[serde(rename = "sshKeyPath")]
    pub ssh_key_path: Option<String>,
    #[serde(rename = "customSubdomain")]
    pub custom_subdomain: Option<String>,
}

// Tunnel states
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "status", content = "data")]
pub enum TunnelState {
    Inactive,
    Connecting,
    Active(String),
    Error(String),
}

// Process manager structure
pub struct TunnelProcess {
    pub preset_id: String,
    pub state: TunnelState,
    pub child_tx: Option<mpsc::Sender<()>>,
}

pub struct AppState {
    pub tunnels: Arc<Mutex<HashMap<String, TunnelProcess>>>,
}

// Check if a system tray watcher is active on Linux (D-Bus check)
#[cfg(target_os = "linux")]
fn check_system_tray_support() -> Result<bool, String> {
    use std::process::Command;

    // Check using gdbus (standard on GNOME/Ubuntu)
    let output = Command::new("gdbus")
        .args(&[
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.NameHasOwner",
            "org.kde.StatusNotifierWatcher",
        ])
        .output();

    if let Ok(ref out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("true") {
            return Ok(true);
        }
    }

    // Fallback to dbus-send
    let output2 = Command::new("dbus-send")
        .args(&[
            "--print-reply",
            "--dest=org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            "string:org.kde.StatusNotifierWatcher",
        ])
        .output();

    if let Ok(ref out) = output2 {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("boolean true") {
            return Ok(true);
        }
    }

    // Defensive check: if both dbus commands failed because they were not found,
    // we return true to avoid false-negatives on minimal window managers (like i3/sway)
    let gdbus_missing = output.is_err();
    let dbus_send_missing = output2.is_err();
    if gdbus_missing && dbus_send_missing {
        return Ok(true);
    }

    Ok(false)
}


// Get system specific home directory
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

// Get the presets.json path
fn get_presets_path(app_handle: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let mut path = app_handle.path().app_config_dir()
        .map_err(|e| format!("Не удалось получить путь конфигурации: {}", e))?;
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("Не удалось создать директорию конфигурации: {}", e))?;
    path.push("presets.json");
    Ok(path)
}

// Show desktop notification
fn show_notification(app_handle: &tauri::AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app_handle.notification()
        .builder()
        .title(&format!("Tunnelhunt: {}", title))
        .body(body)
        .show();
}

// Get default SSH host & port from environment variables or defaults
fn get_default_host_port() -> (String, u16) {
    let host = std::env::var("TUNNELHUNT_HOST")
        .unwrap_or_else(|_| {
            if cfg!(debug_assertions) {
                "localhost".to_string()
            } else {
                "tunnelhunt.ru".to_string()
            }
        });
    let port = std::env::var("TUNNELHUNT_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(2222);
    (host, port)
}

// Parse SSH stdout to extract https url
fn extract_url(line: &str) -> Option<String> {
    if let Some(idx) = line.find("https://") {
        let sub = &line[idx..];
        let url_end = sub.find(|c: char| c.is_whitespace() || c.is_control() || c == '"' || c == '\'' || c == '`');
        let url = match url_end {
            Some(end) => &sub[..end],
            None => sub,
        };
        return Some(url.trim().to_string());
    }
    None
}

fn update_tray_icon_status(
    app_handle: &tauri::AppHandle,
    state_mutex: &Arc<Mutex<HashMap<String, TunnelProcess>>>,
) {
    let has_active = {
        let map = state_mutex.lock().unwrap();
        map.values().any(|t| matches!(t.state, TunnelState::Active(_)))
    };

    if let Some(tray) = app_handle.tray_by_id("main-tray") {
        let icon_bytes = if has_active {
            include_bytes!("../icons/64x64_active.png").as_ref()
        } else {
            include_bytes!("../icons/64x64.png").as_ref()
        };
        if let Ok(icon) = tauri::image::Image::from_bytes(icon_bytes) {
            let _ = tray.set_icon(Some(icon));
        }
    }
}

// Update local state and emit event to frontend
fn update_tunnel_state(
    preset_id: &str,
    state: TunnelState,
    state_mutex: &Arc<Mutex<HashMap<String, TunnelProcess>>>,
    app_handle: &tauri::AppHandle,
) {
    {
        let mut map = state_mutex.lock().unwrap();
        if let Some(tunnel) = map.get_mut(preset_id) {
            tunnel.state = state.clone();
        }
    }
    let _ = app_handle.emit("tunnel-state-changed", serde_json::json!({
        "id": preset_id,
        "state": state
    }));
    update_tray_icon_status(app_handle, state_mutex);
}

// Async task to monitor the SSH process and handle automatic reconnects
async fn monitor_tunnel(
    preset: Preset,
    app_handle: tauri::AppHandle,
    state_mutex: Arc<Mutex<HashMap<String, TunnelProcess>>>,
    mut kill_rx: mpsc::Receiver<()>,
) {
    let preset_id = preset.id.clone();
    let name = preset.name.clone();
    let (host, ssh_port) = get_default_host_port();
    let mut attempts = 0;

    loop {
        update_tunnel_state(&preset_id, TunnelState::Connecting, &state_mutex, &app_handle);

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(&[
            "-tt",
            "-p", &ssh_port.to_string(),
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "BatchMode=yes",
        ]);

        if let Some(key) = &preset.ssh_key_path {
            cmd.args(&["-i", key]);
        }

        cmd.args(&[
            "-R", &format!("80:localhost:{}", preset.port),
            &host,
        ]);

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("Ошибка запуска SSH: {}", e);
                update_tunnel_state(&preset_id, TunnelState::Error(err_msg), &state_mutex, &app_handle);
                break;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        // Prevent child.wait() from automatically closing stdin, which causes SSH to exit
        let _stdin = child.stdin.take();

        let mut stdout_reader = tokio::io::BufReader::new(stdout);
        let mut stderr_reader = tokio::io::BufReader::new(stderr);

        let mut last_error_msg: Option<String> = None;
        let mut limit_reached = false;
        let mut dashboard_closed = false;
        let mut is_connected = false;

        let mut stdout_line = String::new();
        let mut stderr_line = String::new();

        // 8 seconds connection timeout
        let timeout_duration = std::time::Duration::from_secs(8);
        let connect_timeout = tokio::time::sleep(timeout_duration);
        tokio::pin!(connect_timeout);

        loop {
            stdout_line.clear();
            stderr_line.clear();

            tokio::select! {
                res = stdout_reader.read_line(&mut stdout_line) => {
                    match res {
                        Ok(0) | Err(_) => break, // EOF or error
                        Ok(_) => {
                            let line = stdout_line.trim();
                            if !line.is_empty() {
                                println!("[{}] SSH STDOUT: {}", name, line);
                            }
                            if stdout_line.contains("[Limit]") || stdout_line.contains("lifetime limit reached") {
                                limit_reached = true;
                            }
                            if stdout_line.contains("[Closed]") || stdout_line.contains("Tunnel closed from dashboard") {
                                dashboard_closed = true;
                            }
                            if let Some(url) = extract_url(&stdout_line) {
                                is_connected = true;
                                update_tunnel_state(&preset_id, TunnelState::Active(url.clone()), &state_mutex, &app_handle);
                                println!("[{}] URL parsed: {}", name, url);
                                show_notification(&app_handle, &name, &format!("Туннель запущен: {}", url));
                            }
                        }
                    }
                }
                res = stderr_reader.read_line(&mut stderr_line) => {
                    match res {
                        Ok(0) | Err(_) => {},
                        Ok(_) => {
                            let clean = stderr_line.trim().to_string();
                            if !clean.is_empty() {
                                println!("[{}] SSH STDERR: {}", name, clean);
                                last_error_msg = Some(clean);
                            }
                        }
                    }
                }
                _ = kill_rx.recv() => {
                    let _ = child.kill().await;
                    update_tunnel_state(&preset_id, TunnelState::Inactive, &state_mutex, &app_handle);
                    return;
                }
                _ = &mut connect_timeout, if !is_connected => {
                    let _ = child.kill().await;
                    last_error_msg = Some("Таймаут подключения (8с)".to_string());
                    break;
                }
                status = child.wait() => {
                    println!("[{}] SSH завершился с кодом {:?}", name, status);
                    break;
                }
            }
        }

        let _ = child.kill().await;

        if limit_reached {
            let msg = "Отключено по таймауту для бесплатных тарифов".to_string();
            update_tunnel_state(&preset_id, TunnelState::Error(msg), &state_mutex, &app_handle);
            break;
        }

        if dashboard_closed {
            let msg = "Туннель отключен через дашборд".to_string();
            update_tunnel_state(&preset_id, TunnelState::Error(msg), &state_mutex, &app_handle);
            break;
        }

        // Reconnect attempts
        if attempts < 5 {
            attempts += 1;
            let msg = format!("Попытка переподключения {}/5...", attempts);
            update_tunnel_state(&preset_id, TunnelState::Connecting, &state_mutex, &app_handle);
            show_notification(&app_handle, &name, &format!("Туннель упал. {}", msg));

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {}
                _ = kill_rx.recv() => {
                    update_tunnel_state(&preset_id, TunnelState::Inactive, &state_mutex, &app_handle);
                    return;
                }
            }
        } else {
            let mut err_msg = last_error_msg.unwrap_or_else(|| "Соединение с сервером разорвано".to_string());
            if err_msg.contains("Pseudo-terminal will not be allocated") {
                err_msg = "Ошибка подключения (проверьте порт SSH/сервер)".to_string();
            }
            update_tunnel_state(&preset_id, TunnelState::Error(err_msg), &state_mutex, &app_handle);
            break;
        }
    }
}

// --- commands ---

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn get_presets(app_handle: tauri::AppHandle) -> Result<Vec<Preset>, String> {
    let path = get_presets_path(&app_handle)?;
    if !path.exists() {
        return Ok(vec![
            Preset {
                id: "7a8b9c1d-2e3f-4a5b-6c7d-8e9f0a1b2c3d".to_string(),
                name: "Next.js Frontend".to_string(),
                port: 3000,
                ssh_key_path: None,
                custom_subdomain: None,
            },
            Preset {
                id: "8a9b0c1d-2e3f-4a5b-6c7d-8e9f0a1b2c3d".to_string(),
                name: "FastAPI Backend".to_string(),
                port: 8000,
                ssh_key_path: None,
                custom_subdomain: None,
            },
        ]);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Ошибка чтения файла пресетов: {}", e))?;
    let presets: Vec<Preset> = serde_json::from_str(&content)
        .map_err(|e| format!("Ошибка парсинга пресетов: {}", e))?;
    Ok(presets)
}

#[tauri::command]
fn save_presets(app_handle: tauri::AppHandle, presets: Vec<Preset>) -> Result<(), String> {
    let path = get_presets_path(&app_handle)?;
    let content = serde_json::to_string_pretty(&presets)
        .map_err(|e| format!("Ошибка сериализации пресетов: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Ошибка записи файла пресетов: {}", e))?;
    Ok(())
}

#[tauri::command]
fn select_key_file() -> Result<Option<String>, String> {
    let mut dialog = rfd::FileDialog::new()
        .set_title("Выберите приватный SSH-ключ");

    if let Some(home) = home_dir() {
        let ssh_dir = home.join(".ssh");
        if ssh_dir.exists() {
            dialog = dialog.set_directory(ssh_dir);
        } else {
            dialog = dialog.set_directory(home);
        }
    }

    let file_handle = dialog.pick_file();
    Ok(file_handle.map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
fn start_tunnel_cmd(
    preset: Preset,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let preset_id = preset.id.clone();
    let mut map = state.tunnels.lock().unwrap();

    // Stop if already running
    if let Some(tunnel) = map.get(&preset_id) {
        if let Some(tx) = &tunnel.child_tx {
            let _ = tx.blocking_send(());
        }
    }

    let (tx, rx) = mpsc::channel(1);

    map.insert(preset_id.clone(), TunnelProcess {
        preset_id: preset_id.clone(),
        state: TunnelState::Connecting,
        child_tx: Some(tx),
    });

    let app_handle_clone = app_handle.clone();
    let tunnels_clone = state.tunnels.clone();

    tauri::async_runtime::spawn(async move {
        monitor_tunnel(preset, app_handle_clone, tunnels_clone, rx).await;
    });

    Ok(())
}

#[tauri::command]
fn stop_tunnel_cmd(
    preset_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut map = state.tunnels.lock().unwrap();
    if let Some(tunnel) = map.get_mut(&preset_id) {
        if let Some(tx) = &tunnel.child_tx {
            let _ = tx.blocking_send(());
        }
        tunnel.child_tx = None;
        tunnel.state = TunnelState::Inactive;
    }
    Ok(())
}

#[tauri::command]
fn stop_all_tunnels_cmd(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut map = state.tunnels.lock().unwrap();
    for tunnel in map.values_mut() {
        if let Some(tx) = &tunnel.child_tx {
            let _ = tx.blocking_send(());
        }
        tunnel.child_tx = None;
        tunnel.state = TunnelState::Inactive;
    }
    Ok(())
}

#[tauri::command]
fn get_tunnel_states(
    state: tauri::State<'_, AppState>,
) -> Result<HashMap<String, TunnelState>, String> {
    let map = state.tunnels.lock().unwrap();
    let res = map.iter()
        .map(|(k, v)| (k.clone(), v.state.clone()))
        .collect();
    Ok(res)
}

#[tauri::command]
fn exit_app(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) {
    let mut map = state.tunnels.lock().unwrap();
    for tunnel in map.values_mut() {
        if let Some(tx) = &tunnel.child_tx {
            let _ = tx.blocking_send(());
        }
        tunnel.child_tx = None;
    }
    app_handle.exit(0);
}

#[tauri::command]
fn open_url(
    url: String,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app_handle.opener().open_url(&url, None::<&str>)
        .map_err(|e| format!("Ошибка открытия ссылки: {}", e))?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    println!("Starting Tunnelhunt Desktop Client...");
    // Check tray support first on Linux
    #[cfg(target_os = "linux")]
    {
        println!("Checking system tray support...");
        match check_system_tray_support() {
            Ok(false) => {
                eprintln!("Error: System tray is not supported or active (StatusNotifierWatcher not found).");
                rfd::MessageDialog::new()
                    .set_title("Ошибка: Системный трей не поддерживается")
                    .set_description(
                        "Системный трей не запущен в вашем окружении.\n\n\
                        Если вы используете GNOME, пожалуйста, установите расширение 'AppIndicator and KStatusNotifierItem Support'.\n\n\
                        Инструкция по установке: https://github.com/ubuntu/gnome-shell-extension-appindicator"
                    )
                    .set_buttons(rfd::MessageButtons::Ok)
                    .set_level(rfd::MessageLevel::Error)
                    .show();
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error checking system tray: {}", e);
            }
            _ => {
                println!("System tray support verified successfully.");
            }
        }
    }

    let app_state = AppState {
        tunnels: Arc::new(Mutex::new(HashMap::new())),
    };

    tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            get_presets,
            save_presets,
            select_key_file,
            start_tunnel_cmd,
            stop_tunnel_cmd,
            stop_all_tunnels_cmd,
            get_tunnel_states,
            exit_app,
            open_url
        ])
        .setup(|app| {
            // Hide macOS Dock icon (Accessory mode)
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Initialize positioner plugin
            let _ = app.handle().plugin(tauri_plugin_positioner::init());

            let window = app.get_webview_window("main").unwrap();

            // Track last blur time to avoid double toggling when clicking tray icon
            let last_blur = Arc::new(Mutex::new(None::<Instant>));

            // Hide window on focus loss (blur)
            let last_blur_clone = last_blur.clone();
            let w_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Focused(false) = event {
                    let mut lock = last_blur_clone.lock().unwrap();
                    *lock = Some(Instant::now());
                    let _ = w_clone.hide();
                }
            });

            // Set up system tray icon
            let tray_icon = tauri::image::Image::from_bytes(include_bytes!("../icons/64x64.png")).unwrap();

            let last_blur_tray = last_blur.clone();
            #[allow(unused_mut)]
            let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                .icon(tray_icon)
                .tooltip("Tunnelhunt");

            #[cfg(target_os = "linux")]
            {
                if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
                    let appindicator_dir = std::path::PathBuf::from(runtime_dir).join("appindicator");
                    if appindicator_dir.exists() {
                        tray_builder = tray_builder.temp_dir_path(appindicator_dir);
                    }
                }
            }

            let _tray = tray_builder
                .on_tray_icon_event(move |tray, event| {
                    // Update positioner with tray details
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Check cooldown to avoid double toggling (if blur just hid it)
                        let is_cooldown = {
                            let lock = last_blur_tray.lock().unwrap();
                            if let Some(instant) = *lock {
                                instant.elapsed() < Duration::from_millis(250)
                            } else {
                                false
                            }
                        };

                        if is_cooldown {
                            return;
                        }

                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let is_visible = window.is_visible().unwrap_or(false);
                            if is_visible {
                                let _ = window.hide();
                            } else {
                                // Position window center-aligned below tray icon
                                let _ = window.move_window(Position::TrayCenter);
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
