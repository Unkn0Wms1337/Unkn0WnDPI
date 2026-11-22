//! Иконка в системном трее (Shell_NotifyIcon) и меню.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegEnumKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    KEY_WRITE, REG_DWORD,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, LoadIconW,
    PeekMessageW, PostMessageW, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, WNDCLASSW,
};

/// Иконки трея: замок с красной точкой (выключено) и с зелёной (работает).
const TRAY_OFF_ICO: &[u8] = include_bytes!("../assets/tray-off.ico");
const TRAY_WORK_ICO: &[u8] = include_bytes!("../assets/tray-work.ico");
const ICON_RES_VER: u32 = 0x0003_0000;

fn load_tray_icon(bytes: &[u8]) -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    let img = pick_icon_image(bytes, 32).unwrap_or(bytes);
    unsafe {
        CreateIconFromResourceEx(
            img.as_ptr(),
            img.len() as u32,
            1,
            ICON_RES_VER,
            0,
            0,
            0,
        )
    }
}

/// Извлекает из файла .ico картинку нужного размера (или первую попавшуюся).
fn pick_icon_image(bytes: &[u8], want: u8) -> Option<&[u8]> {
    if bytes.len() < 6 || bytes[0] != 0 || bytes[1] != 0 || bytes[2] != 1 || bytes[3] != 0 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut fallback: Option<&[u8]> = None;
    for i in 0..count {
        let e = 6 + 16 * i;
        if e + 16 > bytes.len() {
            break;
        }
        let w = bytes[e];
        let len = u32::from_le_bytes([bytes[e + 8], bytes[e + 9], bytes[e + 10], bytes[e + 11]]) as usize;
        let off = u32::from_le_bytes([bytes[e + 12], bytes[e + 13], bytes[e + 14], bytes[e + 15]]) as usize;
        if off + len > bytes.len() || len == 0 {
            continue;
        }
        if w == want {
            return Some(&bytes[off..off + len]);
        }
        if fallback.is_none() {
            fallback = Some(&bytes[off..off + len]);
        }
    }
    fallback
}

const WM_APP: u32 = 0x8000;
const CALLBACK: u32 = WM_APP + 1;

/// Постоянный GUID иконки: по нему Windows хранит настройку «показывать в трее».
const ICON_GUID: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x4c7a_9e11,
    data2: 0x8d23,
    data3: 0x4f6b,
    data4: [0x9a, 0x05, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f],
};
const WM_COMMAND: u32 = 0x0111;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_NULL: u32 = 0x0000;
const MF_STRING: u32 = 0x0000;
const MF_SEPARATOR: u32 = 0x0800;
const TPM_LEFTALIGN: u32 = 0x0000;
const TPM_BOTTOMALIGN: u32 = 0x0020;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;
const TPM_NONOTIFY: u32 = 0x0080;
const PM_REMOVE: u32 = 0x0001;

const ACT_OPEN: u8 = 1;
const ACT_TOGGLE: u8 = 2;
const ACT_QUIT: u8 = 3;

static ACTION: AtomicU8 = AtomicU8::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
static PROMOTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Open,
    Toggle,
    Quit,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const NOTIFY_SETTINGS: &str = "Control Panel\\NotifyIconSettings";

/// Список подобластей HKCU\Control Panel\NotifyIconSettings (настройки иконок трея).
fn notify_subkeys() -> Vec<String> {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let path = wide(NOTIFY_SETTINGS);
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null_mut(),
            0,
            KEY_READ,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut idx = 0u32;
        while idx < 4096 {
            let mut name = [0u16; 96];
            let mut len = name.len() as u32;
            let rc = RegEnumKeyExW(
                hkey,
                idx,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if rc != 0 {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            idx += 1;
        }
        RegCloseKey(hkey);
        out
    }
}

/// Помечает иконку как «всегда показывать в области уведомлений».
fn set_promoted(key: &str) {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let path = wide(&format!("{NOTIFY_SETTINGS}\\{key}"));
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null_mut(),
            0,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return;
        }
        let val = wide("IsPromoted");
        let data: u32 = 1;
        let _ = RegSetValueExW(
            hkey,
            val.as_ptr(),
            0,
            REG_DWORD,
            &data as *const u32 as *const u8,
            4,
        );
        RegCloseKey(hkey);
    }
}

/// После добавления иконки explorer создаёт её ключ в NotifyIconSettings.
/// Находим новый ключ и закрепляем иконку в видимой области трея.
fn spawn_promoter(state_file: PathBuf, before: Vec<String>) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(4));
        let saved = std::fs::read_to_string(&state_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        let after = notify_subkeys();
        for k in &after {
            if *k != saved && !before.contains(k) {
                set_promoted(k);
                let _ = std::fs::write(&state_file, k);
                PROMOTED.store(true, Ordering::SeqCst);
                break;
            }
        }
        if !saved.is_empty() {
            set_promoted(&saved);
        }
    });
}

/// Выводит окно на передний план, даже если Windows считает его фоновым.
pub fn force_foreground(hwnd: HWND) {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    unsafe {
        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let cur = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != cur && AttachThreadInput(cur, fg_thread, 1) != 0;
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        // Кратковременный topmost — надёжный способ вытащить окно из фона.
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        SetForegroundWindow(hwnd);
        if attached {
            AttachThreadInput(cur, fg_thread, 0);
        }
    }
}

/// Иконка приложения из ресурсов exe (ID 1), иначе стандартная.
pub fn app_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let icon = LoadIconW(hinst, 1 as *const u16);
        if !icon.is_null() {
            return icon;
        }
        LoadIconW(std::ptr::null_mut(), 32512 as *const u16)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        CALLBACK => {
            let ev = (lp as u32) & 0xffff;
            if ev == WM_LBUTTONUP || ev == WM_LBUTTONDBLCLK {
                ACTION.store(ACT_OPEN, Ordering::SeqCst);
            } else if ev == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as u8;
            if (ACT_OPEN..=ACT_QUIT).contains(&id) {
                ACTION.store(id, Ordering::SeqCst);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let open = wide("Открыть окно");
    let toggle = if RUNNING.load(Ordering::SeqCst) {
        wide("Остановить обход")
    } else {
        wide("Запустить обход")
    };
    let exit = wide("Выход");

    AppendMenuW(menu, MF_STRING, ACT_OPEN as usize, open.as_ptr());
    AppendMenuW(menu, MF_STRING, ACT_TOGGLE as usize, toggle.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, ACT_QUIT as usize, exit.as_ptr());

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    DestroyMenu(menu);
    PostMessageW(hwnd, WM_NULL, 0, 0);
    if cmd != 0 {
        ACTION.store(cmd as u8, Ordering::SeqCst);
    }
}

pub struct Tray {
    hwnd: HWND,
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    icon_off: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    icon_work: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    running: AtomicBool,
}

impl Tray {
    pub fn new(state_file: PathBuf, initial_running: bool) -> Option<Tray> {
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            if hinst.is_null() {
                return None;
            }
            // Закрепляем иконку в видимой области трея до её добавления.
            let before = notify_subkeys();
            let saved = std::fs::read_to_string(&state_file)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !saved.is_empty() {
                set_promoted(&saved);
            }
            let class = wide("Unkn0WnDPITrayWnd");
            let wc = WNDCLASSW {
                // CS_DBLCLKS: нужны сообщения о двойном клике по иконке.
                style: 0x0001,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);

            let title = wide("Unkn0WnDPI");
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }

            let icon = app_icon();
            if icon.is_null() {
                DestroyWindow(hwnd);
                return None;
            }
            let icon_off = load_tray_icon(TRAY_OFF_ICO);
            let icon_work = load_tray_icon(TRAY_WORK_ICO);
            let shown = if initial_running && !icon_work.is_null() {
                icon_work
            } else if !icon_off.is_null() {
                icon_off
            } else {
                icon
            };

            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID;
            nid.uCallbackMessage = CALLBACK;
            nid.hIcon = shown;
            nid.guidItem = ICON_GUID;
            let tip: Vec<u16> = "Unkn0WnDPI — обход DPI"
                .encode_utf16()
                .take(nid.szTip.len() - 1)
                .collect();
            nid.szTip[..tip.len()].copy_from_slice(&tip);

            if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
                DestroyWindow(hwnd);
                return None;
            }
            spawn_promoter(state_file, before);
            Some(Tray {
                hwnd,
                icon,
                icon_off,
                icon_work,
                running: AtomicBool::new(initial_running),
            })
        }
    }

    /// Прокачивает очередь сообщений окна трея и возвращает действие.
    pub fn poll(&self) -> Option<Event> {
        unsafe {
            let mut msg: windows_sys::Win32::UI::WindowsAndMessaging::MSG =
                std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Иконку только что закрепили в трее: перечитываем настройку пересозданием.
        if PROMOTED.swap(false, Ordering::SeqCst) {
            unsafe {
                let nid = self.nid();
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }
        }
        match ACTION.swap(0, Ordering::SeqCst) {
            ACT_OPEN => Some(Event::Open),
            ACT_TOGGLE => Some(Event::Toggle),
            ACT_QUIT => Some(Event::Quit),
            _ => None,
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID;
        nid.uCallbackMessage = CALLBACK;
        nid.hIcon = self.icon;
        nid.guidItem = ICON_GUID;
        let tip: Vec<u16> = "Unkn0WnDPI — обход DPI"
            .encode_utf16().take(nid.szTip.len() - 1).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        nid
    }

    pub fn set_running(&self, running: bool) {
        RUNNING.store(running, Ordering::SeqCst);
        if self.running.swap(running, Ordering::SeqCst) == running {
            return;
        }
        let icon = if running { self.icon_work } else { self.icon_off };
        if icon.is_null() {
            return;
        }
        let mut nid = self.nid();
        nid.uFlags = NIF_ICON | NIF_GUID;
        nid.hIcon = icon;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            nid.hIcon = self.icon;
            nid.guidItem = ICON_GUID;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            DestroyWindow(self.hwnd);
            if !self.icon_off.is_null() {
                DestroyIcon(self.icon_off);
            }
            if !self.icon_work.is_null() {
                DestroyIcon(self.icon_work);
            }
        }
    }
}
