//! Конфиг (`config.json`, правится руками или из меню трея) и сохранённое состояние (`state.json`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const DEFAULT_OBS_URL: &str = "ws://127.0.0.1:4455";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    /// Только вручную.
    #[default]
    Manual,
    /// При смене дня (с учётом `day_start_hour`).
    Daily,
    /// При старте трансляции в OBS (через obs-websocket).
    Stream,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Имя устройства ввода; `null` — устройство по умолчанию.
    pub device: Option<String>,
    pub listening: bool,
    pub reset_mode: ResetMode,
    /// Час, с которого начинаются новые сутки в режиме `daily`.
    pub day_start_hour: u8,
    /// Файл для OBS; `null` — `%APPDATA%\Solonka\count.txt`.
    pub output_file: Option<PathBuf>,
    /// Текст в файле, `{count}` заменяется числом.
    pub template: String,
    pub obs_url: String,
    pub obs_password: String,
    /// Минимальный уровень сигнала (RMS, 0..1), ниже которого звук считается тишиной.
    pub vad_min_level: f32,
    /// EcoQoS: процесс работает на энергоэффективных ядрах.
    pub eco_mode: bool,
    /// Папка модели GigaAM (`model.int8.onnx`, `tokens.txt`); `null` — `models\gigaam-v2-ru` рядом с exe.
    pub model_path: Option<PathBuf>,
    /// Дополнительные слова для счёта; `слово*` — по началу слова.
    pub extra_words: Vec<String>,
    /// Слова, которые не считать никогда.
    pub ignore_words: Vec<String>,
    /// Ставить обновления с GitHub автоматически (проверка при запуске и раз в 6 часов).
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: None,
            listening: true,
            reset_mode: ResetMode::Manual,
            day_start_hour: 5,
            output_file: None,
            template: "Маты: {count}".into(),
            obs_url: DEFAULT_OBS_URL.into(),
            obs_password: String::new(),
            vad_min_level: 0.004,
            eco_mode: true,
            model_path: None,
            extra_words: Vec::new(),
            ignore_words: Vec::new(),
            auto_update: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        app_dir().join("config.json")
    }

    /// Ошибка разбора не перетирает файл пользователя: возвращаем дефолт и текст ошибки.
    pub fn load() -> (Config, Option<String>) {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(s.trim_start_matches('\u{feff}')) {
                Ok(c) => (c, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("Ошибка в {}:\n{e}", path.display())),
                ),
            },
            Err(_) => {
                let c = Config::default();
                c.save();
                (c, None)
            }
        }
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&Self::path(), s.as_bytes());
        }
    }

    pub fn output_path(&self) -> PathBuf {
        self.output_file
            .clone()
            .unwrap_or_else(|| app_dir().join("count.txt"))
    }

    pub fn model_dir(&self) -> PathBuf {
        self.model_path
            .clone()
            .unwrap_or_else(|| exe_dir().join("models").join("gigaam-v2-ru"))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct State {
    pub count: u64,
    /// Номер дня последнего сброса/проверки для режима `daily`.
    pub day: i64,
}

impl State {
    pub fn path() -> PathBuf {
        app_dir().join("state.json")
    }

    pub fn load() -> State {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string(self) {
            let _ = write_atomic(&Self::path(), s.as_bytes());
        }
    }
}

pub fn app_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(exe_dir);
        let dir = base.join("Solonka");
        // До версии 1.0 программа называлась ShitCounter — переносим её данные.
        let legacy = base.join("ShitCounter");
        if !dir.exists() && legacy.is_dir() {
            let _ = std::fs::rename(&legacy, &dir);
        }
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
    .clone()
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Запись через временный файл + rename, чтобы OBS никогда не прочитал полупустой файл.
/// Если файл в этот момент открыт на чтение (OBS как раз его читает) — пара повторов,
/// в крайнем случае обычная запись.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    for _ in 0..3 {
        if std::fs::rename(&tmp, path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(path, data)
}
