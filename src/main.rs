#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Солонка — счётчик мата для стрима: трей + микрофон → GigaAM → счётчик → текстовый файл для OBS.

mod audio;
mod config;
mod counter;
mod icon;
mod matcher;
mod obs;
mod recognizer;
mod status;
mod update;
mod win;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

use config::{Config, ResetMode};
use counter::{Counter, SharedCounter};
use matcher::Matcher;
use obs::ObsClient;
use recognizer::{Ctrl, Recognizer};
use status::{Engine, Obs, SharedStatus, Status};
use update::Updater;

const MODES: [(ResetMode, &str); 3] = [
    (ResetMode::Manual, "Только вручную"),
    (ResetMode::Daily, "Каждый день"),
    (ResetMode::Stream, "При старте стрима (OBS)"),
];

struct Menus {
    menu: Menu,
    obs_shown: bool,
    count: MenuItem,
    status: MenuItem,
    obs: MenuItem,
    listen: CheckMenuItem,
    reset: MenuItem,
    mic: Submenu,
    mic_items: Vec<(CheckMenuItem, Option<String>)>,
    mic_refresh: MenuItem,
    modes: Vec<(CheckMenuItem, ResetMode)>,
    open_config: MenuItem,
    reload_config: MenuItem,
    open_folder: MenuItem,
    autostart: CheckMenuItem,
    update: MenuItem,
    quit: MenuItem,
}

struct App {
    cfg: Config,
    counter: SharedCounter,
    status: SharedStatus,
    rec: Recognizer,
    obs: ObsClient,
    updater: Updater,
    capture: Option<audio::Capture>,
    capture_note: Option<String>,
    /// Захват упал повторно — ждём действия пользователя, чтобы не крутиться в цикле ошибок.
    capture_suspended: bool,
    last_auto_restart: Option<Instant>,
    engine_error_shown: bool,
    tray: TrayIcon,
    menus: Menus,
    shown_icon: Option<icon::Kind>,
    shown_tooltip: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--wav" {
        let (cfg, _) = Config::load();
        let matcher = Matcher::new(&cfg.extra_words, &cfg.ignore_words);
        let model = cfg.model_dir();
        if let Err(e) = recognizer::run_wav(args[2].as_ref(), &model, &matcher, cfg.vad_min_level) {
            eprintln!("ошибка: {e}");
        }
        return;
    }
    if !win::acquire_single_instance() {
        win::info_box("Солонка уже запущена — её иконка в трее.");
        return;
    }
    win::register_main_thread();

    let (cfg, cfg_error) = Config::load();
    if let Some(e) = cfg_error {
        win::error_box(&format!("{e}\n\nИспользуются настройки по умолчанию."));
    }
    win::make_process_humble(cfg.eco_mode);

    let status: SharedStatus = Arc::new(Mutex::new(Status::default()));
    let counter: SharedCounter = Arc::new(Mutex::new(Counter::new(&cfg)));
    let rec = Recognizer::spawn(
        cfg.model_dir(),
        Matcher::new(&cfg.extra_words, &cfg.ignore_words),
        cfg.vad_min_level,
        counter.clone(),
        status.clone(),
    );
    let obs = ObsClient::spawn(&cfg, counter.clone(), status.clone());
    let updater = Updater::spawn(cfg.auto_update, status.clone());

    // События мыши по иконке не нужны; без обработчика они копились бы в канале.
    TrayIconEvent::set_event_handler(Some(|_| {}));

    let menus = build_menu(&cfg);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menus.menu.clone()))
        .with_tooltip(win::APP_NAME)
        .with_icon(icon::make(icon::Kind::Busy))
        .build()
        .expect("tray icon");

    let mut app = App {
        cfg,
        counter,
        status,
        rec,
        obs,
        updater,
        capture: None,
        capture_note: None,
        capture_suspended: false,
        last_auto_restart: None,
        engine_error_shown: false,
        tray,
        menus,
        shown_icon: Some(icon::Kind::Busy),
        shown_tooltip: String::new(),
    };
    app.update();

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        if unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } <= 0 {
            break;
        }
        if !msg.hwnd.is_null() {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let mut changed = false;
        let pending = win::take_pending();
        if pending & win::EV_QUIT != 0 {
            return;
        }
        if pending & win::EV_STREAM_ERROR != 0 {
            app.on_stream_error();
            changed = true;
        }
        changed |= pending & win::EV_REFRESH != 0;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *app.menus.quit.id() {
                return;
            }
            app.on_menu(&ev);
            changed = true;
        }
        if changed {
            app.update();
        }
    }
}

fn build_menu(cfg: &Config) -> Menus {
    let m = Menus {
        menu: Menu::new(),
        obs_shown: true,
        count: MenuItem::new("Маты: 0", false, None),
        status: MenuItem::new("Загрузка модели…", false, None),
        obs: MenuItem::new("", false, None),
        listen: CheckMenuItem::new("Слушать микрофон", true, cfg.listening, None),
        reset: MenuItem::new("Обнулить счётчик", true, None),
        mic: Submenu::new("Микрофон", true),
        mic_items: Vec::new(),
        mic_refresh: MenuItem::new("Обновить список", true, None),
        modes: MODES
            .iter()
            .map(|&(mode, title)| {
                (
                    CheckMenuItem::new(title, true, cfg.reset_mode == mode, None),
                    mode,
                )
            })
            .collect(),
        open_config: MenuItem::new("Открыть настройки (config.json)", true, None),
        reload_config: MenuItem::new("Перечитать настройки", true, None),
        open_folder: MenuItem::new("Папка с файлом для OBS", true, None),
        autostart: CheckMenuItem::new(
            "Запускать вместе с Windows",
            true,
            win::autostart_enabled(),
            None,
        ),
        update: MenuItem::new(
            format!("Проверить обновления (v{})", update::VERSION),
            true,
            None,
        ),
        quit: MenuItem::new("Выход", true, None),
    };
    let reset_sub = Submenu::new("Автосброс", true);
    for (item, _) in &m.modes {
        let _ = reset_sub.append(item);
    }
    let sep = PredefinedMenuItem::separator;
    let _ = m.menu.append_items(&[
        &m.count,
        &m.status,
        &m.obs,
        &sep(),
        &m.listen,
        &m.reset,
        &sep(),
        &m.mic,
        &reset_sub,
        &sep(),
        &m.open_config,
        &m.reload_config,
        &m.open_folder,
        &sep(),
        &m.autostart,
        &m.update,
        &m.quit,
    ]);
    let mut m = m;
    fill_mic_menu(&mut m, cfg.device.as_deref());
    m
}

fn fill_mic_menu(m: &mut Menus, selected: Option<&str>) {
    for (item, _) in m.mic_items.drain(..) {
        let _ = m.mic.remove(&item);
    }
    let _ = m.mic.remove(&m.mic_refresh);
    let mut names: Vec<Option<String>> = vec![None];
    names.extend(audio::input_device_names().into_iter().map(Some));
    for name in names {
        let title = name.as_deref().unwrap_or("По умолчанию (Windows)");
        let item = CheckMenuItem::new(title, true, name.as_deref() == selected, None);
        let _ = m.mic.append(&item);
        m.mic_items.push((item, name));
    }
    let _ = m.mic.append(&m.mic_refresh);
}

impl App {
    fn on_menu(&mut self, ev: &MenuEvent) {
        let id = &ev.id;
        let m = &self.menus;
        if id == m.listen.id() {
            self.cfg.listening = !self.cfg.listening;
            self.capture_suspended = false;
            self.cfg.save();
        } else if id == m.reset.id() {
            self.counter.lock().unwrap().reset();
        } else if id == m.mic_refresh.id() {
            let selected = self.cfg.device.clone();
            fill_mic_menu(&mut self.menus, selected.as_deref());
            self.capture_suspended = false;
        } else if let Some((_, name)) = m.mic_items.iter().find(|(i, _)| i.id() == id) {
            self.cfg.device = name.clone();
            self.cfg.save();
            self.restart_capture();
        } else if let Some(&(_, mode)) = m.modes.iter().find(|(i, _)| i.id() == id) {
            self.cfg.reset_mode = mode;
            self.cfg.save();
            self.apply_config();
        } else if id == m.open_config.id() {
            let path = Config::path();
            if !path.exists() {
                self.cfg.save();
            }
            win::open_text_file(&path);
        } else if id == m.reload_config.id() {
            self.reload_config();
        } else if id == m.open_folder.id() {
            let out = self.cfg.output_path();
            if let Some(dir) = out.parent() {
                win::shell_open(dir, None);
            }
        } else if id == m.autostart.id() {
            let on = !win::autostart_enabled();
            if !win::set_autostart(on) {
                win::error_box("Не удалось изменить автозапуск.");
            }
        } else if id == m.update.id() {
            if self.status.lock().unwrap().update.is_some() {
                self.updater.install_found();
            } else {
                self.updater.check_now();
            }
        }
    }

    fn reload_config(&mut self) {
        let (cfg, err) = Config::load();
        if let Some(e) = err {
            win::error_box(&format!("{e}\n\nНастройки не применены."));
            return;
        }
        let device_changed = cfg.device != self.cfg.device;
        self.cfg = cfg;
        self.apply_config();
        self.rec.send(Ctrl::SetMatcher(Matcher::new(
            &self.cfg.extra_words,
            &self.cfg.ignore_words,
        )));
        self.rec.send(Ctrl::SetVadLevel(self.cfg.vad_min_level));
        let selected = self.cfg.device.clone();
        fill_mic_menu(&mut self.menus, selected.as_deref());
        if device_changed {
            self.restart_capture();
        }
    }

    fn apply_config(&mut self) {
        self.counter.lock().unwrap().apply_config(&self.cfg);
        self.obs.apply_config(&self.cfg);
        self.updater.set_auto(self.cfg.auto_update);
    }

    fn on_stream_error(&mut self) {
        self.stop_capture();
        let recently = self
            .last_auto_restart
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
        if recently {
            self.capture_suspended = true;
            self.capture_note = Some("Микрофон пропал — выберите его в меню".into());
        } else {
            self.last_auto_restart = Some(Instant::now());
        }
    }

    fn stop_capture(&mut self) {
        if self.capture.take().is_some() {
            self.rec.send(Ctrl::Stop);
        }
    }

    fn restart_capture(&mut self) {
        self.stop_capture();
        self.capture_suspended = false;
        self.capture_note = None;
        self.sync_capture();
    }

    /// Приводит захват к желаемому состоянию: пишем, только если включено и модель готова.
    fn sync_capture(&mut self) {
        let ready = self.status.lock().unwrap().engine == Engine::Ready;
        let want = self.cfg.listening && ready && !self.capture_suspended;
        if want && self.capture.is_none() {
            match audio::start(self.cfg.device.as_deref(), &self.rec) {
                Ok((cap, note)) => {
                    self.capture = Some(cap);
                    self.capture_note = note;
                }
                Err(e) => {
                    self.capture_suspended = true;
                    self.capture_note = Some(e);
                }
            }
        } else if !want {
            self.stop_capture();
        }
    }

    fn update(&mut self) {
        self.sync_capture();
        let count = self.counter.lock().unwrap().count();
        let st = self.status.lock().unwrap().clone();
        let m = &self.menus;

        m.count.set_text(format!("Маты: {count}"));
        m.listen.set_checked(self.cfg.listening);
        // Пункт меню мог переключить автозапуск, а установщик — поставить его заново.
        m.autostart.set_checked(win::autostart_enabled());
        match &st.update {
            Some(v) => m.update.set_text(format!("Установить обновление {v}")),
            None => m
                .update
                .set_text(format!("Проверить обновления (v{})", update::VERSION)),
        }
        for (item, name) in &m.mic_items {
            item.set_checked(*name == self.cfg.device);
        }
        for (item, mode) in &m.modes {
            item.set_checked(*mode == self.cfg.reset_mode);
        }

        let (text, kind) = match (&st.engine, &self.capture, &self.capture_note) {
            (Engine::Loading, ..) => ("Загрузка модели…".to_string(), icon::Kind::Busy),
            (Engine::Error(e), ..) => (e.clone(), icon::Kind::Error),
            _ if !self.cfg.listening => ("На паузе".to_string(), icon::Kind::Paused),
            (_, Some(cap), note) => (
                note.clone()
                    .unwrap_or_else(|| format!("Слушаю: {}", cap.device_name)),
                icon::Kind::Listening,
            ),
            (_, None, note) => (
                note.clone().unwrap_or_else(|| "Микрофон не запущен".into()),
                icon::Kind::Error,
            ),
        };
        m.status.set_text(shorten(&text, 60));
        // Строка OBS нужна только в режиме «при старте стрима».
        let want_obs = self.cfg.reset_mode == ResetMode::Stream;
        if want_obs {
            m.obs.set_text(match &st.obs {
                Obs::Connected => "OBS: подключено".to_string(),
                Obs::Error(e) => shorten(&format!("OBS: {e}"), 60),
                _ => "OBS: подключение…".to_string(),
            });
        }
        if want_obs != m.obs_shown {
            if want_obs {
                let _ = m.menu.insert(&m.obs, 2);
            } else {
                let _ = m.menu.remove(&m.obs);
            }
            self.menus.obs_shown = want_obs;
        }
        if self.shown_icon != Some(kind) {
            let _ = self.tray.set_icon(Some(icon::make(kind)));
            self.shown_icon = Some(kind);
        }
        let tooltip = match &st.update {
            Some(v) => format!(
                "{} — маты: {count}
Доступна версия {v}",
                win::APP_NAME
            ),
            None => format!("{} — маты: {count}", win::APP_NAME),
        };
        if tooltip != self.shown_tooltip {
            let _ = self.tray.set_tooltip(Some(&tooltip));
            self.shown_tooltip = tooltip;
        }

        if let Engine::Error(e) = &st.engine
            && !self.engine_error_shown
        {
            self.engine_error_shown = true;
            win::error_box(&format!(
                "{e}\n\nРядом с exe должна лежать папка models\\gigaam-v2-ru \
                 (model.int8.onnx и tokens.txt). Подробнее — в README."
            ));
        }
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}
