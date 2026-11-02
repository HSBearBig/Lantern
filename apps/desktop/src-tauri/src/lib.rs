use lantern_host::{HostCommand, HostConfig, HostStatus};
use lantern_platform::CaptureOptions;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, State, WebviewWindow, WindowEvent,
};
use tokio::sync::{mpsc, watch};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    server_url: String,
    device_name: String,
    autostart: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: "https://lantern.example.invalid".into(),
            device_name: "My computer".into(),
            autostart: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    server_url: String,
    device_name: String,
    autostart: bool,
    allow_insecure_localhost: bool,
}

#[derive(Default)]
struct ServiceInner {
    running: bool,
    generation: u64,
    command: Option<mpsc::Sender<HostCommand>>,
    status: Option<HostStatus>,
}
struct AppState {
    service: Arc<Mutex<ServiceInner>>,
    settings_path: PathBuf,
    host_data_dir: PathBuf,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceState {
    running: bool,
    status: Option<HostStatus>,
}

#[tauri::command]
fn load_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    read_settings(&state.settings_path)
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    validate_settings(&settings)?;
    write_settings(&state.settings_path, &settings)
}

#[tauri::command]
async fn start_service(state: State<'_, AppState>, request: StartRequest) -> Result<(), String> {
    let settings = Settings {
        server_url: request.server_url.clone(),
        device_name: request.device_name.clone(),
        autostart: request.autostart,
    };
    validate_settings(&settings)?;
    write_settings(&state.settings_path, &settings)?;
    start_host(state.service.clone(), state.host_data_dir.clone(), request).await
}

#[tauri::command]
async fn stop_service(state: State<'_, AppState>) -> Result<(), String> {
    send_command(&state.service, HostCommand::Shutdown).await
}

#[tauri::command]
async fn create_pairing(state: State<'_, AppState>) -> Result<(), String> {
    send_command(&state.service, HostCommand::Pair).await
}

#[tauri::command]
async fn disconnect_session(state: State<'_, AppState>) -> Result<(), String> {
    send_command(&state.service, HostCommand::Disconnect).await
}

#[tauri::command]
async fn set_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    send_command(&state.service, HostCommand::SetEnabled(enabled)).await
}

#[tauri::command]
async fn revoke_browser(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err("Invalid browser identity".into());
    }
    send_command(&state.service, HostCommand::Revoke(id)).await
}

#[tauri::command]
fn service_state(state: State<'_, AppState>) -> Result<ServiceState, String> {
    let service = state
        .service
        .lock()
        .map_err(|_| "Service state is unavailable")?;
    Ok(ServiceState {
        running: service.running,
        status: service.status.clone(),
    })
}

#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    if text.len() > 8_192 {
        return Err("Text is too long".into());
    }
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|error| format!("Cannot copy to clipboard: {error}"))
}

#[tauri::command]
async fn quit_app(app: AppHandle) -> Result<(), String> {
    shutdown_and_exit(app).await;
    Ok(())
}

async fn start_host(
    service: Arc<Mutex<ServiceInner>>,
    data_dir: PathBuf,
    request: StartRequest,
) -> Result<(), String> {
    let config = HostConfig {
        server: Url::parse(&request.server_url)
            .map_err(|error| format!("Invalid server URL: {error}"))?,
        name: request.device_name,
        data_dir,
        options: CaptureOptions::default(),
        pair_on_start: false,
        allow_insecure_localhost: request.allow_insecure_localhost,
    };
    config.validate().map_err(|error| error.to_string())?;
    let initial = HostStatus::starting(config.name.clone());
    let (command_tx, command_rx) = mpsc::channel(32);
    let (status_tx, mut status_rx) = watch::channel(initial.clone());
    let generation = {
        let mut inner = service.lock().map_err(|_| "Service state is unavailable")?;
        if inner.running {
            return Err("Host service is already running".into());
        }
        inner.running = true;
        inner.generation = inner.generation.wrapping_add(1);
        let generation = inner.generation;
        inner.command = Some(command_tx);
        inner.status = Some(initial);
        generation
    };

    let updates = service.clone();
    tauri::async_runtime::spawn(async move {
        while status_rx.changed().await.is_ok() {
            if let Ok(mut inner) = updates.lock() {
                let status = status_rx.borrow_and_update().clone();
                if inner.generation == generation && inner.running {
                    inner.status = Some(status);
                }
            }
        }
    });
    tauri::async_runtime::spawn(async move {
        let result = lantern_host::run(config, command_rx, status_tx).await;
        if let Ok(mut inner) = service.lock() {
            if inner.generation != generation {
                return;
            }
            inner.running = false;
            inner.command = None;
            let status = inner
                .status
                .get_or_insert_with(|| HostStatus::starting("Lantern".into()));
            status.online = false;
            status.session = None;
            status.pairing_url = None;
            status.pairing_expires_in = 0;
            status.enabled = false;
            if let Err(error) = result {
                status.error = Some(error.to_string());
            }
        }
    });
    Ok(())
}

async fn send_command(
    service: &Arc<Mutex<ServiceInner>>,
    command: HostCommand,
) -> Result<(), String> {
    let sender = service
        .lock()
        .map_err(|_| "Service state is unavailable")?
        .command
        .clone()
        .ok_or("Host service is not running")?;
    sender
        .send(command)
        .await
        .map_err(|_| "Host service stopped unexpectedly".into())
}

fn validate_settings(settings: &Settings) -> Result<(), String> {
    let url =
        Url::parse(&settings.server_url).map_err(|error| format!("Invalid server URL: {error}"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err("Server URL must use HTTPS".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Server URL cannot contain credentials, query, or fragment".into());
    }
    if settings.device_name.is_empty() || settings.device_name.len() > 100 {
        return Err("Device name must contain 1–100 bytes".into());
    }
    Ok(())
}

fn read_settings(path: &Path) -> Result<Settings, String> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| format!("Settings are corrupt: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("Cannot read settings: {error}")),
    }
}

fn write_settings(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create settings directory: {error}"))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Cannot save settings: {error}"))
}

async fn shutdown_and_exit(app: AppHandle) {
    let service = app.state::<AppState>().service.clone();
    let _ = send_command(&service, HostCommand::Shutdown).await;
    for _ in 0..60 {
        let running = service.lock().map(|state| state.running).unwrap_or(false);
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    app.exit(0);
}

fn configure_window(window: WebviewWindow) {
    let close_window = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = close_window.hide();
        }
    });
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Lantern", true, None::<&str>)?;
    let disconnect =
        MenuItem::with_id(app, "disconnect", "Disconnect session", true, None::<&str>)?;
    let disable = MenuItem::with_id(app, "disable", "Disable remote access", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Lantern", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &disconnect, &disable, &quit])?;
    TrayIconBuilder::new()
        .icon(tray_image())
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "disconnect" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let service = app.state::<AppState>().service.clone();
                    let _ = send_command(&service, HostCommand::Disconnect).await;
                });
            }
            "disable" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let service = app.state::<AppState>().service.clone();
                    let _ = send_command(&service, HostCommand::SetEnabled(false)).await;
                });
            }
            "quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn(shutdown_and_exit(app));
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn tray_image() -> Image<'static> {
    let mut rgba = vec![0u8; 16 * 16 * 4];
    for y in 2..14 {
        for x in 2..14 {
            let index = (y * 16 + x) * 4;
            rgba[index..index + 4].copy_from_slice(&[149, 221, 131, 255]);
        }
    }
    Image::new_owned(rgba, 16, 16)
}

pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            start_service,
            stop_service,
            create_pairing,
            disconnect_session,
            set_enabled,
            revoke_browser,
            service_state,
            copy_text,
            quit_app
        ])
        .setup(|app| {
            let app_data = app.path().app_data_dir()?;
            let settings_path = app_data.join("settings.json");
            let settings = read_settings(&settings_path).unwrap_or_default();
            app.manage(AppState {
                service: Arc::new(Mutex::new(ServiceInner::default())),
                settings_path,
                host_data_dir: app_data.join("host"),
            });
            if let Some(window) = app.get_webview_window("main") {
                configure_window(window);
            }
            build_tray(app.handle())?;
            if settings.autostart {
                let service = app.state::<AppState>().service.clone();
                let host_data_dir = app.state::<AppState>().host_data_dir.clone();
                tauri::async_runtime::spawn(async move {
                    let request = StartRequest {
                        server_url: settings.server_url,
                        device_name: settings.device_name,
                        autostart: true,
                        allow_insecure_localhost: false,
                    };
                    if let Err(error) = start_host(service.clone(), host_data_dir, request).await {
                        if let Ok(mut state) = service.lock() {
                            let mut status = HostStatus::starting("Lantern".into());
                            status.error = Some(error);
                            state.status = Some(status);
                        }
                    }
                });
            }
            Ok(())
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building Lantern desktop application");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            // The native Quit menu/Command-Q follows the same release-and-shutdown path.
            api.prevent_exit();
            tauri::async_runtime::spawn(shutdown_and_exit(app.clone()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_never_enables_autostart() {
        let settings = Settings::default();
        assert!(!settings.autostart);
        assert!(validate_settings(&settings).is_ok());
    }

    #[test]
    fn settings_reject_credentials_and_fragments() {
        let settings = Settings {
            server_url: "https://user:password@example.test/#secret".into(),
            ..Default::default()
        };
        assert!(validate_settings(&settings).is_err());
    }
}
