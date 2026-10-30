use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Имя входного MIDI-устройства (для записи).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Имя выходного MIDI-устройства (для воспроизведения).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Папка для записей.
    pub dir: PathBuf,
    /// Базовое имя файла: name1.mid, name2.mid, ...
    pub name: String,
    /// Начинать запись по первой ноте.
    pub auto_start: bool,
    /// Делить запись по паузе.
    pub pause_split: bool,
    /// Порог паузы в секундах.
    pub pause_secs: f64,
    /// Остановить запись через N секунд после начала первой записи.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer_secs: Option<u64>,
    /// Писать последние значения контроллеров в начало каждого нового файла.
    pub keep_controllers: bool,
    /// Сколько мс контроллеров до первой ноты захватывать при автозаписи.
    pub preroll_ms: u64,
    /// Темп в заголовке файла (на запись не влияет, только на шкалу тиков).
    pub bpm: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let dir = dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("midifon");
        Settings {
            input: None,
            output: None,
            dir,
            name: "rec".into(),
            auto_start: true,
            pause_split: false,
            pause_secs: 5.0,
            timer_secs: None,
            keep_controllers: true,
            preroll_ms: 500,
            bpm: 120.0,
        }
    }
}

impl Settings {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("midifon").join("config.toml"))
    }

    pub fn load(path: &Path) -> Result<Settings> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}
