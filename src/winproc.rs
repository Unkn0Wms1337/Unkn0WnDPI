use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, MessageBoxW, ShowWindow,
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, SW_HIDE, SW_SHOWNORMAL,
};

use crate::logging::Logger;
use crate::paths::Paths;
use crate::presets;
use crate::settings::Settings;

pub const SERVICE_NAME: &str = "Unkn0WnDPIZapret";
pub const TASK_NAME: &str = "Unkn0WnDPIAutostart";
pub const INSTANCE_MUTEX: &str = "Local\\Unkn0WnDPISingleInstance";

/// Имя мьютекса экземпляра. В debug-сборке можно задать отдельное пространство
/// имён через `UNKN0WN_DPI_TEST_NS`, чтобы тестировать рядом с рабочим экземпляром.
pub fn instance_mutex_name() -> String {
    #[cfg(debug_assertions)]
    if let Ok(ns) = std::env::var("UNKN0WN_DPI_TEST_NS") {
        if !ns.is_empty() {
            return format!("{INSTANCE_MUTEX}{ns}");
        }
    }
    INSTANCE_MUTEX.to_string()
}
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

pub fn show_message(title: &str, text: &str, error: bool) {
    let t = wide(title);
    let m = wide(text);
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), MB_OK | icon);
    }
}

pub fn open_path(path: &std::path::Path) {
    let op = wide("open");
    let p = wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            p.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn open_url(url: &str) {
    let op = wide("open");
    let u = wide(url);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            u.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".into();
    }
    if arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        let mut s = String::from("\"");
        for c in arg.chars() {
            if c == '"' {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
        s
    } else {
        arg.to_string()
    }
}

/// Перезапуск себя с правами администратора (UAC).
pub fn elevate(args: &[String]) -> bool {
    let exe = std::env::current_exe().unwrap_or_default();
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let op = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params_w = wide(&params);
    let ret = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            params_w.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ret as isize > 32
}

pub struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

const CONSOLE_TITLE: &str = "Unkn0WnDPI - zapret DPI";
const TRAY_CLASS: &str = "Unkn0WnDPITrayWnd";

/// Есть ли живое консольное окно (наше или уже запущенного экземпляра).
pub fn console_window_exists() -> bool {
    unsafe {
        let title = wide(CONSOLE_TITLE);
        !FindWindowW(std::ptr::null(), title.as_ptr()).is_null()
    }
}

/// Перехватчик закрытия консоли больше не используется: крестик закрывает окно,
/// а фон (иконка в трее и встроенный MTProto-прокси) держит отдельный процесс-страж.
pub fn install_close_to_tray_handler() {}

/// Видимо ли наше консольное окно сейчас.
pub fn console_visible() -> bool {
    unsafe {
        let hwnd = GetConsoleWindow();
        !hwnd.is_null() && IsWindowVisible(hwnd) != 0
    }
}

/// Сворачивание окна прячет его в трей (процесс и прокси продолжают работать).
pub fn hide_if_minimized_to_tray() {
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() && IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Завершает осиротевшие копии Unkn0WnDPI, у которых нет окна
/// (например, консоль, унаследовавшая скрытую консоль стража).
/// Возвращает число завершённых процессов.
pub fn kill_stale_consoles() -> usize {
    let me = std::process::id();
    let tray = unsafe {
        let class = wide(TRAY_CLASS);
        let name = wide("Unkn0WnDPI");
        FindWindowW(class.as_ptr(), name.as_ptr())
    };
    let mut tray_pid = 0u32;
    if !tray.is_null() {
        unsafe { GetWindowThreadProcessId(tray, &mut tray_pid) };
    }
    let mut killed = 0;
    for p in processes() {
        if !p.name.eq_ignore_ascii_case("Unkn0WnDPI.exe") || p.pid == me || p.pid == tray_pid {
            continue;
        }
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, p.pid);
            if h.is_null() {
                continue;
            }
            if TerminateProcess(h, 1) != 0 {
                killed += 1;
            }
            CloseHandle(h);
        }
    }
    killed
}

/// Some(guard) — мы единственный экземпляр, None — уже запущен.
pub fn acquire_instance() -> Option<InstanceGuard> {
    let name = wide(&instance_mutex_name());
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return Some(InstanceGuard(std::ptr::null_mut()));
        }
        let err = GetLastError();
        if err == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            None
        } else {
            Some(InstanceGuard(handle))
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcEntry {
    pub pid: u32,
    pub name: String,
    pub path: Option<PathBuf>,
}

fn process_image_name(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

pub fn processes() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return out;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let end = pe
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(pe.szExeFile.len());
                let name = String::from_utf16_lossy(&pe.szExeFile[..end]);
                out.push(ProcEntry {
                    pid: pe.th32ProcessID,
                    name,
                    path: None,
                });
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    for p in &mut out {
        if p.name.eq_ignore_ascii_case("winws.exe") {
            p.path = process_image_name(p.pid);
        }
    }
    out
}

pub fn winws_processes(paths: &Paths) -> Vec<ProcEntry> {
    let mut v: Vec<ProcEntry> = processes()
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case("winws.exe"))
        .collect();
    for p in &mut v {
        if p.path.is_none() {
            p.path = process_image_name(p.pid);
        }
    }
    let ours = |p: &ProcEntry| {
        p.path
            .as_ref()
            .map(|x| path_eq(x, &paths.winws))
            .unwrap_or(false)
    };
    v.sort_by_key(|p| !ours(p));
    v
}

fn path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

pub fn decode_console(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    unsafe {
        let len = MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if len <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut buf = vec![0u16; len as usize];
        let n = MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            buf.as_mut_ptr(),
            len,
        );
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

pub fn run_hidden<I, S>(program: &str, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
}

pub fn service_installed(name: &str) -> bool {
    service_state(name).is_some()
}

pub fn service_state(name: &str) -> Option<String> {
    let out = run_hidden("sc.exe", ["query", name]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = decode_console(&out.stdout);
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once(':') {
            let mut it = rest.split_whitespace();
            if let (Some(num), Some(word)) = (it.next(), it.next()) {
                if num.parse::<u32>().is_ok()
                    && !word.is_empty()
                    && word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                {
                    return Some(word.to_string());
                }
            }
        }
    }
    Some("UNKNOWN".into())
}

pub fn service_running(name: &str) -> bool {
    service_state(name).map(|s| s == "RUNNING").unwrap_or(false)
}

pub fn stop_service(name: &str) {
    let _ = run_hidden("sc.exe", ["stop", name]);
    for _ in 0..10 {
        if !service_running(name) {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

pub fn start_service(name: &str) -> Result<(), String> {
    let out = run_hidden("sc.exe", ["start", name]).map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let text = decode_console(&out.stdout) + &decode_console(&out.stderr);
    if text.contains("1056") || text.to_lowercase().contains("already") {
        return Ok(());
    }
    Err(format!("Не удалось запустить службу {name}: {}", text.trim()))
}

pub fn sync_game_filter(paths: &Paths, settings: &Settings) {
    let path = paths.game_filter_file();
    if settings.game_mode == "off" {
        let _ = std::fs::remove_file(path);
    } else {
        let text = format!(
            "mode={}\r\ntcp={}\r\nudp={}\r\n",
            settings.game_mode, settings.game_tcp_range, settings.game_udp_range
        );
        let _ = std::fs::write(path, text);
    }
}

pub fn stop_bypass(log: &Logger) {
    if service_running(SERVICE_NAME) {
        stop_service(SERVICE_NAME);
    }
    if service_running("zapret") {
        log.warn("Останавливаю старый zapret (служба zapret)...");
    }
    for name in ["zapret", "GoodbyeDPI", "discordfix_zapret", "winws1", "winws2"] {
        if service_running(name) {
            stop_service(name);
        }
    }
    let _ = run_hidden("taskkill.exe", ["/F", "/IM", "winws.exe"]);
    std::thread::sleep(Duration::from_millis(500));
}

pub fn start_bypass(paths: &Paths, settings: &Settings, log: &Logger) -> Result<(), String> {
    let preset = presets::resolve(&settings.preset);

    if service_installed(SERVICE_NAME) {
        start_service(SERVICE_NAME)?;
        log.ok(format!("Обход запущен службой Windows ({preset})"));
        return Ok(());
    }

    stop_bypass(log);
    sync_game_filter(paths, settings);

    // BFE нужен WinDivert.
    if !service_running("BFE") {
        let _ = run_hidden("sc.exe", ["start", "BFE"]);
    }

    if !paths.winws.exists() {
        return Err(format!("Не найден движок: {}", paths.winws.display()));
    }

    let args = presets::args(
        paths,
        &preset,
        &settings.game_mode,
        &settings.game_tcp_range,
        &settings.game_udp_range,
    )?;

    log.info(format!(
        "Запуск: {preset} (режим игры: {})",
        settings.game_mode
    ));
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let mut cmd = Command::new(&paths.winws);
        cmd.args(&args)
            .current_dir(&paths.bin)
            .creation_flags(CREATE_NO_WINDOW);
        match cmd.spawn() {
            Ok(mut child) => {
                std::thread::sleep(Duration::from_millis(1500));
                match child.try_wait() {
                    Ok(Some(status)) => {
                        last_err = format!(
                            "winws завершился с кодом {}. Проверьте права администратора, VPN/антивирус и журнал.",
                            status.code().unwrap_or(-1)
                        );
                        log.warn(format!(
                            "winws не запустился (попытка {attempt} из 3): {last_err}"
                        ));
                    }
                    Ok(None) => {
                        let pid = child.id();
                        log.ok(format!(
                            "Обход включён: {} (PID {pid})",
                            presets::label_for(&preset)
                        ));
                        return Ok(());
                    }
                    Err(e) => {
                        last_err = e.to_string();
                    }
                }
            }
            Err(e) => {
                last_err = e.to_string();
                log.warn(format!(
                    "winws не запустился (попытка {attempt} из 3): {last_err}"
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(1200));
    }
    Err(last_err)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    App,
    Service,
    Foreign,
}

impl Mode {
    pub fn text(self) -> &'static str {
        match self {
            Mode::Off => "выключено",
            Mode::App => "приложение",
            Mode::Service => "служба Windows",
            Mode::Foreign => "старый zapret",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BypassStatus {
    pub running: bool,
    pub mode: Mode,
    pub pid: u32,
    pub foreign: bool,
}

pub fn status(paths: &Paths) -> BypassStatus {
    let svc = service_running(SERVICE_NAME);
    let procs = winws_processes(paths);
    let own = procs
        .iter()
        .find(|p| {
            p.path
                .as_ref()
                .map(|x| path_eq(x, &paths.winws))
                .unwrap_or(false)
        })
        .cloned();
    let any = procs.first().cloned();
    let old_svc = service_running("zapret");
    let mode = if svc {
        Mode::Service
    } else if own.is_some() {
        Mode::App
    } else if any.is_some() || old_svc {
        Mode::Foreign
    } else {
        Mode::Off
    };
    let pid = own
        .as_ref()
        .or(any.as_ref())
        .map(|p| p.pid)
        .unwrap_or(0);
    BypassStatus {
        running: matches!(mode, Mode::App | Mode::Service),
        mode,
        pid,
        foreign: mode == Mode::Foreign,
    }
}

// ---------------------------------------------------------------- автозапуск

pub fn autostart_installed() -> bool {
    run_hidden("schtasks.exe", ["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn install_autostart(paths: &Paths, log: &Logger) -> Result<(), String> {
    if !is_admin() {
        return Err("Нужны права администратора.".into());
    }
    let tr = format!("\"{}\" --from-autostart", paths.exe.display());
    let out = run_hidden(
        "schtasks.exe",
        [
            "/Create",
            "/TN",
            TASK_NAME,
            "/TR",
            tr.as_str(),
            "/SC",
            "ONLOGON",
            "/RL",
            "HIGHEST",
            "/F",
        ],
    )
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "Не удалось создать задачу автозапуска: {}",
            decode_console(&out.stdout).trim()
        ));
    }
    log.ok("Автозапуск при входе включён");
    Ok(())
}

pub fn remove_autostart(log: &Logger) -> Result<(), String> {
    if !is_admin() {
        return Err("Нужны права администратора.".into());
    }
    let _ = run_hidden("schtasks.exe", ["/Delete", "/TN", TASK_NAME, "/F"]);
    log.ok("Автозапуск при входе выключен");
    Ok(())
}

// ---------------------------------------------------------------- служба

pub fn remove_service(log: &Logger) -> Result<(), String> {
    if !is_admin() {
        return Err("Нужны права администратора.".into());
    }
    if !service_installed(SERVICE_NAME) {
        return Ok(());
    }
    stop_service(SERVICE_NAME);
    std::thread::sleep(Duration::from_millis(500));
    let out = run_hidden("sc.exe", ["delete", SERVICE_NAME]).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(500));
    if !out.status.success() && service_installed(SERVICE_NAME) {
        return Err(format!(
            "Не удалось удалить службу: {}",
            decode_console(&out.stdout).trim()
        ));
    }
    log.info("Служба Windows удалена");
    Ok(())
}

pub fn install_service(paths: &Paths, settings: &Settings, log: &Logger) -> Result<(), String> {
    if !is_admin() {
        return Err("Нужны права администратора.".into());
    }
    let preset = presets::resolve(&settings.preset);
    sync_game_filter(paths, settings);
    let _ = remove_service(log);
    stop_bypass(log);

    let args = presets::args(
        paths,
        &preset,
        &settings.game_mode,
        &settings.game_tcp_range,
        &settings.game_udp_range,
    )?;
    let bin_path = format!(
        "\"{}\" {}",
        paths.winws.display(),
        presets::join_args(&args)
    );
    let out = run_hidden(
        "sc.exe",
        [
            "create",
            SERVICE_NAME,
            "binPath=",
            bin_path.as_str(),
            "DisplayName=",
            "Unkn0WnDPI (zapret DPI)",
            "start=",
            "auto",
        ],
    )
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "Служба не создана: {}",
            decode_console(&out.stdout).trim()
        ));
    }
    let _ = run_hidden(
        "sc.exe",
        [
            "description",
            SERVICE_NAME,
            "Обход DPI: Discord, YouTube, игры (zapret/winws)",
        ],
    );
    std::thread::sleep(Duration::from_millis(700));
    start_service(SERVICE_NAME)?;
    let _ = run_hidden(
        "sc.exe",
        [
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/5000/restart/5000",
        ],
    );
    log.ok(format!("Служба Windows установлена и запущена: {preset}"));
    Ok(())
}

pub fn uninstall_all(paths: &Paths, log: &Logger) -> Result<(), String> {
    stop_bypass(log);
    let mut errors = Vec::new();
    if let Err(e) = remove_service(log) {
        errors.push(format!("служба: {e}"));
    }
    if autostart_installed() {
        if let Err(e) = remove_autostart(log) {
            errors.push(format!("автозапуск: {e}"));
        }
    }
    let _ = paths;
    if errors.is_empty() {
        log.ok("Служба и автозапуск удалены. Папку можно удалить вручную.");
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
