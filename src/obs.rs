//! Клиент obs-websocket v5 (встроен в OBS 28+): нужен только для режима «за стрим».
//! Слушает событие `StreamStateChanged` и сбрасывает счётчик при старте трансляции.
//! Поток спит, пока режим не выбран; без OBS пробует подключиться раз в несколько секунд.
//! Порт и пароль по умолчанию берутся из настроек самого OBS — вписывать их руками не нужно.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::{Message, WebSocket};

use crate::config::{Config, DEFAULT_OBS_URL, ResetMode};
use crate::counter::SharedCounter;
use crate::status::{Obs, SharedStatus};
use crate::win;

/// Подписка только на категорию Outputs (1 << 6) — других событий OBS не шлёт.
const EVENT_SUB_OUTPUTS: u32 = 1 << 6;
/// Как часто заблокированное чтение просыпается, чтобы заметить смену настроек.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
struct Settings {
    enabled: bool,
    url: String,
    password: String,
}

pub struct ObsClient {
    settings: Arc<Mutex<Settings>>,
    thread: Thread,
}

impl ObsClient {
    pub fn spawn(cfg: &Config, counter: SharedCounter, status: SharedStatus) -> ObsClient {
        let settings = Arc::new(Mutex::new(settings_of(cfg)));
        let s = settings.clone();
        let handle = thread::Builder::new()
            .name("obs".into())
            .spawn(move || run(s, counter, status))
            .expect("spawn obs");
        ObsClient {
            settings,
            thread: handle.thread().clone(),
        }
    }

    pub fn apply_config(&self, cfg: &Config) {
        let new = settings_of(cfg);
        let mut cur = self.settings.lock().unwrap();
        if *cur != new {
            *cur = new;
            self.thread.unpark();
        }
    }
}

fn settings_of(cfg: &Config) -> Settings {
    Settings {
        enabled: cfg.reset_mode == ResetMode::Stream,
        url: cfg.obs_url.clone(),
        password: cfg.obs_password.clone(),
    }
}

fn set_status(status: &SharedStatus, s: Obs) {
    let mut st = status.lock().unwrap();
    if st.obs != s {
        #[cfg(debug_assertions)]
        eprintln!("[obs] {s:?}");
        st.obs = s;
        drop(st);
        win::notify_main(win::EV_REFRESH);
    }
}

fn run(settings: Arc<Mutex<Settings>>, counter: SharedCounter, status: SharedStatus) {
    win::lower_current_thread();
    loop {
        let s = settings.lock().unwrap().clone();
        if !s.enabled {
            set_status(&status, Obs::Off);
            thread::park();
            continue;
        }
        set_status(&status, Obs::Connecting);
        match session(&s, &settings, &counter, &status) {
            // Настройки поменялись — переподключаемся сразу.
            Ok(()) => continue,
            Err(e) => set_status(&status, Obs::Error(e)),
        }
        // Попытка подключиться к закрытому локальному порту почти ничего не стоит, а короткий
        // интервал гарантирует, что мы успеем подключиться до нажатия «Начать трансляцию».
        thread::park_timeout(RETRY);
    }
}

/// Настройки WebSocket-сервера из конфига OBS (`%APPDATA%\obs-studio\plugin_config\obs-websocket`).
struct LocalObs {
    enabled: bool,
    port: u16,
    password: Option<String>,
}

fn local_obs() -> Option<LocalObs> {
    let path = std::path::PathBuf::from(std::env::var_os("APPDATA")?)
        .join("obs-studio/plugin_config/obs-websocket/config.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(LocalObs {
        enabled: v["server_enabled"].as_bool().unwrap_or(false),
        port: v["server_port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(4455),
        password: v["auth_required"].as_bool().unwrap_or(false).then(|| {
            v["server_password"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }),
    })
}

/// `Ok(())` — сессию закрыли мы из-за смены настроек; `Err` — проблема соединения.
fn session(
    s: &Settings,
    settings: &Mutex<Settings>,
    counter: &SharedCounter,
    status: &SharedStatus,
) -> Result<(), String> {
    let local = local_obs();
    let mut url = s.url.clone();
    let mut password = s.password.clone();
    if let Some(l) = &local {
        // Адрес по умолчанию — значит, OBS на этом же компьютере: порт и пароль знаем сами.
        if url == DEFAULT_OBS_URL {
            url = format!("ws://127.0.0.1:{}", l.port);
        }
        if password.is_empty() {
            password = l.password.clone().unwrap_or_default();
        }
    }
    let mut ws = connect(&url).map_err(|e| match &local {
        Some(l) if !l.enabled => {
            "в OBS выключен WebSocket-сервер (Инструменты → Настройки сервера WebSocket)".into()
        }
        _ => e,
    })?;

    let hello = read_json(&mut ws)?;
    let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": EVENT_SUB_OUTPUTS });
    if let Some(auth) = hello.pointer("/d/authentication") {
        if password.is_empty() {
            return Err("OBS требует пароль (obs_password в настройках)".into());
        }
        let challenge = auth["challenge"].as_str().unwrap_or_default();
        let salt = auth["salt"].as_str().unwrap_or_default();
        identify["authentication"] = auth_string(&password, salt, challenge).into();
    }
    send_json(&mut ws, &json!({ "op": 1, "d": identify }))?;
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                if *settings.lock().unwrap() != *s {
                    let _ = ws.close(None);
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(format!("связь с OBS потеряна: {e}")),
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(if code == 4009 {
                    "неверный пароль OBS".into()
                } else {
                    "OBS закрыл соединение".into()
                });
            }
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        match v["op"].as_u64() {
            Some(2) => set_status(status, Obs::Connected),
            Some(5)
                if v.pointer("/d/eventType").and_then(Value::as_str)
                    == Some("StreamStateChanged")
                    && v.pointer("/d/eventData/outputState")
                        .and_then(Value::as_str)
                        == Some("OBS_WEBSOCKET_OUTPUT_STARTED") =>
            {
                counter.lock().unwrap().on_stream_started();
            }
            _ => {}
        }
    }
}

fn connect(url: &str) -> Result<WebSocket<TcpStream>, String> {
    let host = url
        .strip_prefix("ws://")
        .ok_or("obs_url должен начинаться с ws://")?
        .split('/')
        .next()
        .unwrap_or_default();
    let addr = host
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("неверный адрес OBS: {host}"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|_| "OBS не запущен".to_string())?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let (ws, _) = tungstenite::client(url, tcp).map_err(|e| format!("рукопожатие с OBS: {e}"))?;
    Ok(ws)
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| format!("OBS: {e}"))? {
            Message::Text(t) => return serde_json::from_str(t.as_str()).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("OBS закрыл соединение".into()),
            _ => {}
        }
    }
}

fn send_json(ws: &mut WebSocket<TcpStream>, v: &Value) -> Result<(), String> {
    ws.send(Message::text(v.to_string()))
        .map_err(|e| format!("OBS: {e}"))
}

/// base64(sha256(base64(sha256(password + salt)) + challenge)) — по протоколу obs-websocket v5.
fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let secret = B64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    B64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::auth_string;

    #[test]
    fn obs_auth_is_deterministic() {
        let a = auth_string("pass", "salt", "challenge");
        assert_eq!(a, auth_string("pass", "salt", "challenge"));
        assert_ne!(a, auth_string("pass2", "salt", "challenge"));
        assert_eq!(a.len(), 44); // base64 от 32 байт
    }
}
