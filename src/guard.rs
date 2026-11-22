//! Фоновый «страж» трея: отдельный скрытый процесс, который держит иконку в трее,
//! перезапускает консольное окно и хостит встроенный MTProto-прокси.
//! Консоль (UI) можно закрывать крестиком — страж и прокси продолжают работать.

use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenMutexW, OpenProcess, SetEvent, TerminateProcess,
    WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, IsIconic, PostMessageW, ShowWindow, SW_RESTORE, SW_SHOW,
};

use crate::logging::Logger;
use crate::paths::Paths;
use crate::settings::Settings;
use crate::tgws;
use crate::tray;
use crate::winproc;

pub const GUARD_MUTEX: &str = "Local\\Unkn0WnDPIGuardian";
pub const QUIT_EVENT: &str = "Local\\Unkn0WnDPIQuitAll";

/// Те же debug-пространства имён, что и у мьютекса экземпляра.
fn ns_name(base: &str) -> String {
    #[cfg(debug_assertions)]
    if let Ok(ns) = std::env::var("UNKN0WN_DPI_TEST_NS") {
        if !ns.is_empty() {
            return format!("{base}{ns}");
        }
    }
    base.to_string()
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Новое видимое консольное окно: без этого дочерний процесс унаследует
/// скрытую консоль стража и окно не появится.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const ERROR_ALREADY_EXISTS: u32 = 183;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const VK_F4: usize = 0x73;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct MutexGuard(HANDLE);
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Захват мьютекса стража; None — если страж уже работает.
fn acquire_guard() -> Option<MutexGuard> {
    unsafe {
        let name = wide(&ns_name(GUARD_MUTEX));
        let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if h.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(h);
            return None;
        }
        Some(MutexGuard(h))
    }
}

/// Жива ли консольная часть (держит ли кто-то мьютекс экземпляра).
fn console_running() -> bool {
    unsafe {
        let name = wide(&winproc::instance_mutex_name());
        let h = OpenMutexW(PROCESS_SYNCHRONIZE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

fn quit_event() -> HANDLE {
    unsafe {
        let name = wide(&ns_name(QUIT_EVENT));
        CreateEventW(std::ptr::null(), 1, 0, name.as_ptr())
    }
}

/// Консоль выходит штатно (Esc/F4/меню) — говорим стражу выйти тоже.
pub fn signal_quit_all() {
    unsafe {
        let h = quit_event();
        if !h.is_null() {
            SetEvent(h);
            CloseHandle(h);
        }
    }
}

fn quit_signaled(h: HANDLE) -> bool {
    unsafe { WaitForSingleObject(h, 0) == WAIT_OBJECT_0 }
}

/// Консольный экземпляр запускает стража, если того ещё нет.
pub fn spawn_guard(args: &[String]) {
    if console_guard_running() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--tray-guard");
    for a in args {
        if a != "--from-autostart" {
            cmd.arg(a);
        }
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.spawn();
}

fn console_guard_running() -> bool {
    unsafe {
        let name = wide(&ns_name(GUARD_MUTEX));
        let h = OpenMutexW(PROCESS_SYNCHRONIZE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

const CONSOLE_TITLE: &str = "Unkn0WnDPI - zapret DPI";

fn show_console_window() {
    unsafe {
        let title = wide(CONSOLE_TITLE);
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        tray::force_foreground(hwnd);
    }
}

fn launch_console(args: &[String]) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    unsafe {
        // Страж получил клик пользователя — разрешаем новому окну выйти на передний план.
        AllowSetForegroundWindow(0xFFFF_FFFF);
    }
    let mut cmd = std::process::Command::new(exe);
    for a in args {
        if a != "--from-autostart" {
            cmd.arg(a);
        }
    }
    cmd.creation_flags(CREATE_NEW_CONSOLE);
    let _ = cmd.spawn();
    nudge_console_foreground();
}

/// Дождаться окна нового процесса и поднять его.
fn nudge_console_foreground() {
    std::thread::spawn(|| {
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            let hwnd = console_hwnd();
            if !hwnd.is_null() {
                tray::force_foreground(hwnd);
                return;
            }
        }
    });
}

fn console_hwnd() -> windows_sys::Win32::Foundation::HWND {
    unsafe {
        let title = wide(CONSOLE_TITLE);
        FindWindowW(std::ptr::null(), title.as_ptr())
    }
}

/// PID консольного процесса, сохранённый при его старте.
fn saved_console_pid(paths: &Paths) -> Option<u32> {
    let text = std::fs::read_to_string(paths.config.join("console.pid")).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return None;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        CloseHandle(h);
        if alive {
            Some(pid)
        } else {
            None
        }
    }
}

/// Завершает консольный процесс, у которого нет окна.
fn kill_console_process(pid: u32, logger: &Logger) {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            logger.warn(format!("Не удалось открыть консоль PID {pid} для перезапуска"));
            return;
        }
        if TerminateProcess(h, 1) != 0 {
            logger.warn(format!("Завершён консольный процесс без окна (PID {pid})"));
        }
        WaitForSingleObject(h, 3000);
        CloseHandle(h);
    }
}

/// Ждёт освобождения мьютекса экземпляра.
fn wait_instance_free(timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !console_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Просит консоль штатно завершиться (F4), страж выйдет следом по событию.
fn ask_console_to_quit() {
    unsafe {
        let title = wide(CONSOLE_TITLE);
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return;
        }
        PostMessageW(hwnd, WM_KEYDOWN, VK_F4, 0);
        PostMessageW(hwnd, WM_KEYUP, VK_F4, 0);
    }
}

pub fn run_guard(args: Vec<String>) {
    let _guard = match acquire_guard() {
        Some(g) => g,
        None => return,
    };

    let paths = Paths::init();
    let logger = Logger::new(&paths.log_file, true);
    let settings = Settings::load(&paths.settings_file);
    logger.info("Страж трея запущен");

    // Подготовить secret, чтобы прокси можно было поднять по команде/автозапуску.
    let mut s = settings.clone();
    let _ = tgws::ensure(&paths, &mut s, &logger);

    let mut status = winproc::status(&paths);
    let mut last_status = Instant::now();
    let mut last_launch = Instant::now() - Duration::from_secs(10);
    let tray = tray::Tray::new(paths.config.join("tray-key.txt"), status.running);
    let quit = quit_event();
    if quit.is_null() {
        return;
    }

    // Если обход уже работает (служба/автозапуск) — поднять прокси по настройке.
    tgws::autostart_if_needed(&paths, &logger, status.running);

    loop {
        // Команды от консоли (запуск/остановка/перезапуск прокси).
        if let Some(cmd) = tgws::take_command(&paths) {
            tgws::apply_command(&paths, &logger, &cmd);
        }

        if let Some(t) = &tray {
            if let Some(ev) = t.poll() {
                match ev {
                    tray::Event::Open => {
                        if !console_hwnd().is_null() {
                            show_console_window();
                        } else {
                            // Мьютекс занят, а окна нет — «зомби» (консоль без окна).
                            if console_running() {
                                if let Some(pid) = saved_console_pid(&paths) {
                                    kill_console_process(pid, &logger);
                                    wait_instance_free(Duration::from_secs(2));
                                }
                            }
                            if last_launch.elapsed() > Duration::from_secs(1) {
                                launch_console(&args);
                                last_launch = Instant::now();
                            }
                        }
                    }
                    tray::Event::Toggle => {
                        if status.running {
                            winproc::stop_bypass(&logger);
                        } else {
                            let current = Settings::load(&paths.settings_file);
                            let _ = winproc::start_bypass(&paths, &current, &logger);
                        }
                        status = winproc::status(&paths);
                        last_status = Instant::now();
                        tgws::autostart_if_needed(&paths, &logger, status.running);
                    }
                    tray::Event::Quit => {
                        signal_quit_all();
                        if !console_hwnd().is_null() {
                            ask_console_to_quit();
                        } else {
                            if console_running() {
                                if let Some(pid) = saved_console_pid(&paths) {
                                    kill_console_process(pid, &logger);
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        if last_status.elapsed() > Duration::from_secs(1) {
            status = winproc::status(&paths);
            last_status = Instant::now();
            tgws::autostart_if_needed(&paths, &logger, status.running);
        }

        let shown_running = match std::env::var("UNKN0WN_DPI_TRAY_STATE").as_deref() {
            Ok("work") => true,
            Ok("off") => false,
            _ => status.running,
        };
        if let Some(t) = &tray {
            t.set_running(shown_running);
        }

        if quit_signaled(quit) && !console_running() {
            break;
        }
        // Штатный выход консоли не дошёл — завершаем осиротевший процесс.
        if quit_signaled(quit) && console_running() && console_hwnd().is_null() {
            if let Some(pid) = saved_console_pid(&paths) {
                kill_console_process(pid, &logger);
            }
        }
        std::thread::sleep(Duration::from_millis(80));
    }

    // Полный выход: останавливаем встроенный прокси, если он работает в страже.
    if tgws::running_here() {
        tgws::stop(&paths, &logger);
        std::thread::sleep(Duration::from_millis(150));
    }
    unsafe { CloseHandle(quit) };
    logger.info("Страж трея завершается");
}
