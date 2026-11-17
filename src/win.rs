//! Тонкие обёртки над WinAPI: приоритет процесса, локальное время, сообщения главному потоку.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CreateMutexW, GetCurrentProcess, GetCurrentThread,
    GetCurrentThreadId, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetPriorityClass, SetProcessInformation, SetThreadPriority,
    THREAD_PRIORITY_LOWEST,
};
use windows_sys::Win32::UI::Shell::{
    ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_EXECUTABLE, AssocQueryStringW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW,
    PostThreadMessageW, SW_SHOWNORMAL, WM_APP,
};

pub const APP_NAME: &str = "Солонка";

/// Сообщение-«будильник» для главного потока. Сами события лежат в `PENDING`:
/// пока открыто меню трея, модальный цикл Windows выбрасывает thread-сообщения,
/// а флаг не теряется и обработается на следующем сообщении.
const WM_WAKE: u32 = WM_APP + 1;

/// Состояние поменялось — обновить меню/иконку/захват.
pub const EV_REFRESH: u32 = 1;
/// Аудиопоток упал (устройство пропало).
pub const EV_STREAM_ERROR: u32 = 2;
/// Запущен установщик обновления — пора выходить.
pub const EV_QUIT: u32 = 4;

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static PENDING: AtomicU32 = AtomicU32::new(0);

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn register_main_thread() {
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
}

/// Отметить событие и разбудить главный поток (вызывается из любых потоков).
pub fn notify_main(ev: u32) {
    PENDING.fetch_or(ev, Ordering::AcqRel);
    let tid = MAIN_THREAD.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe { PostThreadMessageW(tid, WM_WAKE, 0, 0) };
    }
}

pub fn take_pending() -> u32 {
    PENDING.swap(0, Ordering::AcqRel)
}

/// Пониженный приоритет + EcoQoS (Windows 11 отправляет процесс на энергоэффективные ядра).
pub fn make_process_humble(eco: bool) {
    unsafe {
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        if eco {
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            };
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

pub fn lower_current_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
}

/// `false`, если приложение уже запущено.
pub fn acquire_single_instance() -> bool {
    let name = wide("Local\\SolonkaSingleInstance");
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // Хэндл намеренно не закрываем: мьютекс живёт до конца процесса.
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Номер «игрового» дня: сутки начинаются в `day_start_hour` по локальному времени,
/// чтобы стрим, перешедший за полночь, оставался в одном дне.
pub fn local_day_key(day_start_hour: u8) -> i64 {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let day = days_from_civil(t.wYear as i64, t.wMonth as i64, t.wDay as i64);
    if (t.wHour as u8) < day_start_hour {
        day - 1
    } else {
        day
    }
}

/// Дни от 1970-01-01 (алгоритм Хиннанта).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Короткий 8.3-путь: sherpa-onnx открывает файлы модели через `char*`, и кириллица в пути
/// (например, в имени пользователя) иначе ломает загрузку.
pub fn short_path(p: &Path) -> String {
    let w = wide(p);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return p.to_string_lossy().into_owned();
    }
    String::from_utf16_lossy(&buf[..n])
}

/// Открыть файл, папку или ссылку программой по умолчанию; `args` — параметры для exe.
pub fn shell_open(p: impl AsRef<OsStr>, args: Option<&str>) -> bool {
    let op = wide("open");
    let file = wide(p);
    let args = args.map(wide);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // По документации успех — значение больше 32.
    r as usize > 32
}

/// Текстовый файл: программой для его типа, а если её нет (у .json часто нет) — Блокнотом.
/// Без проверки Windows показала бы окно «Выберите приложение», которое из трея теряется.
pub fn open_text_file(p: &Path) {
    let has_app = p
        .extension()
        .is_some_and(|e| has_association(&format!(".{}", e.to_string_lossy())));
    if !(has_app && shell_open(p, None)) {
        shell_open("notepad.exe", Some(&format!("\"{}\"", p.display())));
    }
}

/// Есть ли у расширения программа для открытия (без запасного «Выберите приложение»).
fn has_association(ext: &str) -> bool {
    let e = wide(ext);
    let mut len = 0u32;
    unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_EXECUTABLE,
            e.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut len,
        ) >= 0
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Solonka";

/// Автозапуск: значение в HKCU\…\Run (его же ставит галочка в установщике).
pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let mut size = 0u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    r == 0
}

pub fn set_autostart(on: bool) -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let r = if on {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let data = wide(format!("\"{}\"", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) }
    };
    r == 0
}

pub fn error_box(text: &str) {
    message_box(text, MB_OK | MB_ICONERROR);
}

pub fn info_box(text: &str) {
    message_box(text, MB_OK | MB_ICONINFORMATION);
}

pub fn ask_yes_no(text: &str) -> bool {
    message_box(text, MB_ICONQUESTION | MB_YESNO) == IDYES
}

fn message_box(text: &str, flags: u32) -> i32 {
    let t = wide(text);
    let c = wide(APP_NAME);
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), flags) }
}

#[cfg(test)]
mod tests {
    use super::days_from_civil;

    #[test]
    #[ignore = "меняет реестр пользователя; запускать вручную"]
    fn autostart_toggle() {
        let was = super::autostart_enabled();
        assert!(super::set_autostart(true));
        assert!(super::autostart_enabled());
        assert!(super::set_autostart(false));
        assert!(!super::autostart_enabled());
        super::set_autostart(was);
    }

    #[test]
    fn file_associations() {
        assert!(super::has_association(".txt"));
        assert!(!super::has_association(".solonka-unknown"));
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(
            days_from_civil(2026, 1, 1) - days_from_civil(2025, 12, 31),
            1
        );
    }
}
