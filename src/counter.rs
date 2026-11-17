//! Значение счётчика, режимы автосброса и вывод в файл для OBS.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::config::{Config, ResetMode, State, write_atomic};
use crate::win;

pub type SharedCounter = Arc<Mutex<Counter>>;

pub struct Counter {
    state: State,
    mode: ResetMode,
    day_start_hour: u8,
    output: PathBuf,
    template: String,
}

impl Counter {
    pub fn new(cfg: &Config) -> Counter {
        let mut c = Counter {
            state: State::load(),
            mode: cfg.reset_mode,
            day_start_hour: cfg.day_start_hour,
            output: cfg.output_path(),
            template: cfg.template.clone(),
        };
        if c.mode == ResetMode::Daily {
            c.roll_day();
        }
        c.write_output();
        c
    }

    pub fn count(&self) -> u64 {
        self.state.count
    }

    pub fn apply_config(&mut self, cfg: &Config) {
        // При переключении в режим «за сегодня» не сбрасываем сразу — отсчёт дня начинается сейчас.
        if cfg.reset_mode == ResetMode::Daily && self.mode != ResetMode::Daily {
            self.state.day = win::local_day_key(cfg.day_start_hour);
            self.state.save();
        }
        self.mode = cfg.reset_mode;
        self.day_start_hour = cfg.day_start_hour;
        self.template = cfg.template.clone();
        self.output = cfg.output_path();
        self.write_output();
    }

    pub fn add(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        self.roll_day();
        self.state.count += u64::from(n);
        self.persist();
    }

    pub fn reset(&mut self) {
        self.state.count = 0;
        self.state.day = win::local_day_key(self.day_start_hour);
        self.persist();
    }

    /// Сброс по старту стрима: срабатывает только в режиме `stream`.
    pub fn on_stream_started(&mut self) {
        if self.mode == ResetMode::Stream {
            self.reset();
        }
    }

    /// В режиме `daily` сбрасывает счётчик, если наступили новые сутки. `true` — был сброс.
    pub fn roll_day(&mut self) -> bool {
        if self.mode != ResetMode::Daily {
            return false;
        }
        let today = win::local_day_key(self.day_start_hour);
        if self.state.day == today {
            return false;
        }
        self.state.day = today;
        self.state.count = 0;
        self.persist();
        true
    }

    fn persist(&self) {
        self.write_output();
        self.state.save();
        win::notify_main(win::EV_REFRESH);
    }

    fn write_output(&self) {
        let text = self
            .template
            .replace("{count}", &self.state.count.to_string());
        let _ = write_atomic(&self.output, text.as_bytes());
    }
}
