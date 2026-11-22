use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::autoselect::{self, AutoEvent, AutoResult, AutoRow};
use crate::engine::{APP_VERSION, ENGINE_VERSION};
use crate::logging::{Level, Logger};
use crate::netcheck::{self, TestReport};
use crate::paths::Paths;
use crate::presets::{self, Preset};
use crate::settings::Settings;
use crate::tgws::TgStatus;
use crate::tray;
use crate::winproc::{self, BypassStatus, Mode, SERVICE_NAME};

// ------------------------------------------------------------------ палитра CMD

const BLACK: Color = Color::Rgb(8, 8, 8);
const GREEN: Color = Color::Rgb(22, 198, 12);
const BRIGHT: Color = Color::Rgb(240, 240, 240);
const TEXT: Color = Color::Rgb(196, 196, 196);
const DIM: Color = Color::Rgb(120, 120, 120);
const RED: Color = Color::Rgb(231, 72, 86);
const CYAN: Color = Color::Rgb(97, 214, 214);
const YELLOW: Color = Color::Rgb(249, 241, 165);
const BLUE: Color = Color::Rgb(90, 150, 255);
const BORDER: Color = Color::Rgb(70, 70, 70);

// ------------------------------------------------------------------ команды

#[derive(Clone, Debug)]
enum Cmd {
    Toggle,
    Auto,
    Test,
    Update,
    Diag,
    Preset(usize),
    Settings(usize),
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Strategy,
    TgWs,
    Log,
    Diag,
    Settings,
}

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "ОБЗОР",
            Tab::Strategy => "СТРАТЕГИЯ",
            Tab::TgWs => "TG WS",
            Tab::Log => "ЖУРНАЛ",
            Tab::Diag => "ДИАГНОСТИКА",
            Tab::Settings => "НАСТРОЙКИ",
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Overview => 0,
            Tab::Strategy => 1,
            Tab::TgWs => 2,
            Tab::Log => 3,
            Tab::Diag => 4,
            Tab::Settings => 5,
        }
    }

    fn from_index(i: usize) -> Tab {
        match i {
            1 => Tab::Strategy,
            2 => Tab::TgWs,
            3 => Tab::Log,
            4 => Tab::Diag,
            5 => Tab::Settings,
            _ => Tab::Overview,
        }
    }
}

enum Msg {
    Done {
        title: String,
        result: Result<String, String>,
    },
    Test(TestReport),
    Diag(Result<String, String>),
    Update(Result<String, String>),
    Auto(AutoEvent),
}

#[derive(Clone, Copy)]
enum ToastKind {
    Ok,
    Err,
    Info,
}

struct Toast {
    text: String,
    kind: ToastKind,
    until: Instant,
}

struct AutoProgress {
    stage: &'static str,
    index: usize,
    total: usize,
    preset: String,
}

// ------------------------------------------------------------------ приложение

struct App {
    paths: Paths,
    logger: Logger,
    settings: Settings,
    presets: Vec<Preset>,
    selected_preset: String,
    tab: Tab,
    main_sel: usize,
    strategy_sel: usize,
    tg_sel: usize,
    settings_sel: usize,
    status: BypassStatus,
    tg_status: TgStatus,
    autostart: bool,
    service: bool,
    last_status: Instant,
    last_slow_refresh: Instant,
    last_tg_refresh: Instant,
    job: Option<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    test: Option<TestReport>,
    diag_text: String,
    diag_scroll: u16,
    log_scroll: usize,
    auto_progress: Option<AutoProgress>,
    auto_rows: Vec<AutoRow>,
    auto_result: Option<AutoResult>,
    toast: Option<Toast>,
    quitting: bool,
    boot_at: Option<Instant>,
    started_at: Instant,
}

pub fn init_console() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleProcessList, GetConsoleScreenBufferInfo, GetConsoleWindow,
        GetStdHandle, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, SetConsoleScreenBufferSize,
        SetConsoleTitleW, SetConsoleWindowInfo, CONSOLE_SCREEN_BUFFER_INFO, COORD, SMALL_RECT,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
        let title: Vec<u16> = "Unkn0WnDPI - zapret DPI\0".encode_utf16().collect();
        SetConsoleTitleW(title.as_ptr());

        // Иконка окна из ресурсов exe + вывод окна на передний план
        // (актуально при открытии из трея).
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SendMessageW, ShowWindow, SW_SHOW, WM_SETICON,
            };
            let hwnd = GetConsoleWindow();
            let icon = tray::app_icon();
            if !hwnd.is_null() {
                if !icon.is_null() {
                    SendMessageW(hwnd, WM_SETICON, 0, icon as isize);
                    SendMessageW(hwnd, WM_SETICON, 1, icon as isize);
                }
                ShowWindow(hwnd, SW_SHOW);
                tray::force_foreground(hwnd);
            }
        }

        // Своё окно: убираем прокрутку буфера, иначе шапка уезжает за край.
        let mut pids = [0u32; 4];
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) != 1 {
            return;
        }
        if GetConsoleWindow().is_null() {
            return;
        }
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(out, &mut info) == 0 {
            return;
        }
        let w = info.srWindow.Right - info.srWindow.Left + 1;
        let h = info.srWindow.Bottom - info.srWindow.Top + 1;
        if w <= 0 || h <= 0 {
            return;
        }
        let _ = SetConsoleWindowInfo(
            out,
            1,
            &SMALL_RECT {
                Left: 0,
                Top: 0,
                Right: w - 1,
                Bottom: h - 1,
            },
        );
        if info.dwSize.X != w || info.dwSize.Y != h {
            let _ = SetConsoleScreenBufferSize(out, COORD { X: w, Y: h });
        }
        // Без переноса строки: иначе запись в последнюю клетку прокручивает экран.
        let mut mode = 0u32;
        if GetConsoleMode(out, &mut mode) != 0 {
            let _ = SetConsoleMode(out, mode & !0x0002);
        }
    }
}

pub fn run(    paths: Paths,
    logger: Logger,
    settings: Settings,
    from_autostart: bool,
) -> Result<(), String> {
    let mut stdout = io::stdout();
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    // Повторно: raw mode может сбросить режим вывода консоли.
    init_console();
    execute!(stdout, Hide).map_err(|e| format!("screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;
    let _ = terminal.clear();

    let mut app = App::new(paths, logger, settings, from_autostart);
    if !from_autostart {
        raise_window_soon();
    }
    let result = app.event_loop(&mut terminal);

    let _ = disable_raw_mode();
    let _ = terminal.clear();
    let _ = execute!(terminal.backend_mut(), Show);
    let _ = terminal.show_cursor();
    result
}

/// Повторно выводит окно на передний план в первые мгновения после старта:
/// при запуске из трея окно иногда успевает создаться позже первого вызова.
fn raise_window_soon() {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    std::thread::spawn(|| {
        for ms in [120u64, 400] {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            unsafe {
                let hwnd = GetConsoleWindow();
                if hwnd.is_null() {
                    return;
                }
                if GetForegroundWindow() != hwnd {
                    tray::force_foreground(hwnd);
                }
            }
        }
    });
}

impl App {
    fn new(paths: Paths, logger: Logger, settings: Settings, from_autostart: bool) -> App {
        let presets = presets::list();
        let selected = presets::resolve(&settings.preset);
        let auto_result = autoselect::load_last(&paths);
        let (tx, rx) = channel();
        let mut app = App {
            paths,
            logger,
            settings,
            presets,
            selected_preset: selected,
            tab: Tab::Overview,
            main_sel: 0,
            strategy_sel: 0,
            tg_sel: 0,
            settings_sel: 0,
            status: BypassStatus {
                running: false,
                mode: Mode::Off,
                pid: 0,
                foreign: false,
            },
            tg_status: TgStatus {
                running: false,
                listening: false,
                external: false,
                pid: 0,
            },
            autostart: false,
            service: false,
            last_status: Instant::now() - Duration::from_secs(60),
            last_slow_refresh: Instant::now() - Duration::from_secs(60),
            last_tg_refresh: Instant::now() - Duration::from_secs(60),
            job: None,
            tx,
            rx,
            test: None,
            diag_text: String::new(),
            diag_scroll: 0,
            log_scroll: 0,
            auto_progress: None,
            auto_rows: auto_result
                .as_ref()
                .map(|r| r.presets.clone())
                .unwrap_or_default(),
            auto_result,
            toast: None,
            quitting: false,
            boot_at: if from_autostart {
                Some(Instant::now() + Duration::from_secs(3))
            } else {
                None
            },
            started_at: Instant::now(),
        };
        app.refresh_status();
        app.refresh_tg();
        app.autostart = winproc::autostart_installed();
        app.service = winproc::service_installed(SERVICE_NAME);
        if let Some(r) = &app.auto_result {
            if let Some(pos) = app
                .presets
                .iter()
                .position(|p| p.file == r.best)
            {
                app.strategy_sel = pos;
            }
        }
        app
    }

    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), String> {
        loop {
            self.poll_messages();
            self.tick();
            // Сворачивание («_») убирает окно в трей без закрытия консоли.
            winproc::hide_if_minimized_to_tray();
            let visible = winproc::console_visible();
            if visible {
                terminal
                    .draw(|f| self.draw(f))
                    .map_err(|e| format!("draw: {e}"))?;
            }

            let poll_ms = if visible { 200 } else { 500 };
            if event::poll(Duration::from_millis(poll_ms)).map_err(|e| e.to_string())? {
                if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                    if key.kind == KeyEventKind::Press && visible {
                        self.on_key(key.code, key.modifiers);
                    }
                }
            }
            if self.quitting {
                return Ok(());
            }
        }
    }

    fn tick(&mut self) {
        if self.last_status.elapsed() > Duration::from_secs(1) {
            self.refresh_status();
        }
        if self.last_tg_refresh.elapsed() > Duration::from_secs(2) {
            self.refresh_tg();
        }
        if self.last_slow_refresh.elapsed() > Duration::from_secs(6) {
            self.autostart = winproc::autostart_installed();
            self.service = winproc::service_installed(SERVICE_NAME);
            self.last_slow_refresh = Instant::now();
        }
        if let Some(toast) = &self.toast {
            if Instant::now() >= toast.until {
                self.toast = None;
            }
        }
        if self.started_at.elapsed() > Duration::from_secs(3) {
            if let Some(at) = self.boot_at {
                if Instant::now() >= at {
                    self.boot_at = None;
                    if self.settings.auto_start && !self.status.running && self.job.is_none() {
                        self.logger.info("Автозапуск: включаю обход…");
                        self.start_bypass();
                    }
                }
            }
        }
    }

    fn refresh_status(&mut self) {
        self.status = winproc::status(&self.paths);
        self.last_status = Instant::now();
    }

    fn refresh_tg(&mut self) {
        self.tg_status = crate::tgws::status(&self.paths, &self.settings);
        self.last_tg_refresh = Instant::now();
    }

    fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            until: Instant::now() + Duration::from_secs(4),
        });
    }

    fn reload_settings(&mut self) {
        self.settings = Settings::load(&self.paths.settings_file);
        self.selected_preset = presets::resolve(&self.settings.preset);
    }

    fn poll_messages(&mut self) {
        let mut got_done = false;
        let mut done_title = String::new();
        let mut done_result: Result<String, String> = Ok(String::new());
        let mut auto_done = false;

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Done { title, result } => {
                    got_done = true;
                    done_title = title;
                    done_result = result;
                }
                Msg::Test(report) => {
                    self.job = None;
                    autoselect::save_report(&self.paths, &report);
                    netcheck::report_lines(&report, &self.logger);
                    let score = format!(
                        "Проверка связи: {}/{} за {} мс",
                        report.score, report.max, report.elapsed_ms
                    );
                    self.toast(
                        score,
                        if report.score >= 5 {
                            ToastKind::Ok
                        } else {
                            ToastKind::Info
                        },
                    );
                    self.test = Some(report);
                }
                Msg::Diag(result) => {
                    self.job = None;
                    match result {
                        Ok(text) => {
                            self.diag_text = text;
                            self.diag_scroll = 0;
                            self.tab = Tab::Diag;
                            self.toast(
                                "Диагностика сохранена в logs/diag-latest.txt",
                                ToastKind::Ok,
                            );
                        }
                        Err(e) => {
                            self.logger.error(format!("Диагностика: {e}"));
                            self.toast(format!("Диагностика: {e}"), ToastKind::Err);
                        }
                    }
                }
                Msg::Update(result) => {
                    self.job = None;
                    match result {
                        Ok(n) => self.toast(format!("Список IP обновлён: {n} строк"), ToastKind::Ok),
                        Err(e) => {
                            self.logger.error(format!("Обновление списка IP: {e}"));
                            self.toast(format!("Список IP: {e}"), ToastKind::Err);
                        }
                    }
                }
                Msg::Auto(ev) => match ev {
                    AutoEvent::Progress {
                        stage,
                        index,
                        total,
                        preset,
                    } => {
                        self.auto_progress = Some(AutoProgress {
                            stage,
                            index,
                            total,
                            preset,
                        });
                    }
                    AutoEvent::Row(row) => {
                        if let Some(existing) =
                            self.auto_rows.iter_mut().find(|r| r.preset == row.preset)
                        {
                            *existing = row;
                        } else {
                            self.auto_rows.push(row);
                        }
                    }
                    AutoEvent::Done(result) => {
                        auto_done = true;
                        match result {
                            Ok(r) => {
                                self.toast(
                                    if r.best.is_empty() {
                                        "Авто-подбор: рабочая стратегия не найдена".to_string()
                                    } else {
                                        format!(
                                            "Авто-подбор: {} ({}/{})",
                                            r.best_label, r.best_score, r.max_score
                                        )
                                    },
                                    if r.selected {
                                        ToastKind::Ok
                                    } else {
                                        ToastKind::Info
                                    },
                                );
                                self.auto_result = Some(r);
                            }
                            Err(e) => {
                                self.logger.error(format!("Авто-подбор: {e}"));
                                self.toast(format!("Авто-подбор: {e}"), ToastKind::Err);
                            }
                        }
                    }
                },
            }
        }

        if auto_done {
            self.job = None;
            self.auto_progress = None;
            self.reload_settings();
            self.refresh_status();
        }
        if got_done {
            let bypass_ok = done_title == "Обход" && done_result.is_ok();
            self.job = None;
            match done_result {
                Ok(text) => self.toast(format!("{done_title}: {text}"), ToastKind::Ok),
                Err(e) => {
                    self.logger.error(format!("{done_title}: {e}"));
                    self.toast(format!("{done_title}: {e}"), ToastKind::Err);
                }
            }
            self.reload_settings();
            self.refresh_status();
            if bypass_ok && self.settings.tg_auto_start && !self.tg_status.running {
                // Прокси живёт в процессе-страже (чтобы переживать закрытие окна).
                let _ = crate::tgws::send_command(&self.paths, "start");
                self.refresh_tg();
            }
        }
    }

    // -------------------------------------------------------------- ввод

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.quitting = true;
            return;
        }
        match code {
            KeyCode::F(4) => self.quitting = true,
            KeyCode::Esc => {
                if self.tab == Tab::Overview {
                    self.quitting = true;
                } else {
                    self.tab = Tab::Overview;
                }
            }
            KeyCode::Left => {
                self.tab = Tab::from_index((self.tab.index() + 5) % 6);
                self.log_scroll = 0;
            }
            KeyCode::Right => {
                self.tab = Tab::from_index((self.tab.index() + 1) % 6);
                self.log_scroll = 0;
            }
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Enter => self.activate(),
            _ => {}
        }
    }

    fn move_up(&mut self) {
        match self.tab {
            Tab::Overview => {
                let n = self.main_items().len();
                if n > 0 {
                    self.main_sel = (self.main_sel + n - 1) % n;
                }
            }
            Tab::Strategy => {
                if !self.presets.is_empty() {
                    self.strategy_sel =
                        (self.strategy_sel + self.presets.len() - 1) % self.presets.len();
                }
            }
            Tab::Settings => {
                let n = self.settings_items().len();
                if n > 0 {
                    self.settings_sel = (self.settings_sel + n - 1) % n;
                }
            }
            Tab::TgWs => {
                let n = self.tg_items().len();
                if n > 0 {
                    self.tg_sel = (self.tg_sel + n - 1) % n;
                }
            }
            Tab::Log => self.log_scroll = self.log_scroll.saturating_add(1),
            Tab::Diag => self.diag_scroll = self.diag_scroll.saturating_sub(1),
        }
    }

    fn move_down(&mut self) {
        match self.tab {
            Tab::Overview => {
                let n = self.main_items().len();
                if n > 0 {
                    self.main_sel = (self.main_sel + 1) % n;
                }
            }
            Tab::Strategy => {
                if !self.presets.is_empty() {
                    self.strategy_sel = (self.strategy_sel + 1) % self.presets.len();
                }
            }
            Tab::Settings => {
                let n = self.settings_items().len();
                if n > 0 {
                    self.settings_sel = (self.settings_sel + 1) % n;
                }
            }
            Tab::TgWs => {
                let n = self.tg_items().len();
                if n > 0 {
                    self.tg_sel = (self.tg_sel + 1) % n;
                }
            }
            Tab::Log => self.log_scroll = self.log_scroll.saturating_sub(1),
            Tab::Diag => {
                let total = self.diag_text.lines().count().max(1);
                self.diag_scroll = self.diag_scroll.saturating_add(1).min(total as u16);
            }
        }
    }

    fn activate(&mut self) {
        match self.tab {
            Tab::Overview => {
                if let Some(cmd) = self.main_items().get(self.main_sel).map(|x| x.1.clone()) {
                    self.run_cmd(cmd);
                }
            }
            Tab::Strategy => {
                if let Some(p) = self.presets.get(self.strategy_sel) {
                    self.apply_preset(p.file.clone());
                }
            }
            Tab::Settings => {
                if let Some(cmd) = self
                    .settings_items()
                    .get(self.settings_sel)
                    .map(|x| x.1.clone())
                {
                    self.run_cmd(cmd);
                }
            }
            Tab::Diag => self.build_diag(),
            Tab::Log => self.log_scroll = 0,
            Tab::TgWs => self.tgws_action(self.tg_sel),
        }
    }

    // -------------------------------------------------------------- tg-ws

    fn tg_items(&self) -> Vec<(String, usize)> {
        let toggle = if self.tg_status.running {
            "Остановить MTProto-прокси"
        } else {
            "Запустить MTProto-прокси"
        };
        vec![
            (toggle.to_string(), 0),
            ("Перезапустить прокси".to_string(), 1),
            ("Открыть в Telegram".to_string(), 2),
            ("Скопировать ссылку".to_string(), 3),
            ("Новый secret".to_string(), 4),
            (
                format!(
                    "Запуск вместе с обходом: {}",
                    if self.settings.tg_auto_start {
                        "включён"
                    } else {
                        "выключен"
                    }
                ),
                5,
            ),
        ]
    }

    fn tgws_action(&mut self, index: usize) {
        match index {
            0 => {
                // Прокси хостится в процессе-страже, чтобы переживать закрытие окна.
                if self.tg_status.running {
                    if let Err(e) = crate::tgws::send_command(&self.paths, "stop") {
                        self.toast(e, ToastKind::Err);
                    } else {
                        self.toast("MTProto-прокси остановлен", ToastKind::Ok);
                    }
                } else if let Err(e) = crate::tgws::send_command(&self.paths, "start") {
                    self.toast(e, ToastKind::Err);
                } else {
                    self.toast(
                        format!("Прокси запускается: 127.0.0.1:{}", self.settings.tg_port),
                        ToastKind::Ok,
                    );
                }
                self.refresh_tg();
            }
            1 => {
                if let Err(e) = crate::tgws::send_command(&self.paths, "restart") {
                    self.toast(e, ToastKind::Err);
                } else {
                    self.toast("Прокси перезапускается", ToastKind::Ok);
                }
                self.refresh_tg();
            }
            2 => {
                let url = crate::tgws::link(&self.settings);
                if crate::tgws::open_in_telegram(&url).is_err() {
                    self.toast("Не удалось открыть Telegram", ToastKind::Err);
                } else {
                    self.toast("Ссылка открыта в Telegram", ToastKind::Ok);
                }
            }
            3 => {
                let url = crate::tgws::link(&self.settings);
                match crate::tgws::copy_link(&url) {
                    Ok(()) => self.toast("Ссылка скопирована", ToastKind::Ok),
                    Err(e) => self.toast(e, ToastKind::Err),
                }
            }
            4 => {
                let was_running = self.tg_status.running;
                if was_running {
                    let _ = crate::tgws::send_command(&self.paths, "stop");
                    std::thread::sleep(Duration::from_millis(350));
                }
                let mut settings = self.settings.clone();
                settings.tg_secret = String::new();
                match crate::tgws::ensure(&self.paths, &mut settings, &self.logger) {
                    Ok(()) => {
                        self.settings = settings;
                        if was_running {
                            let _ = crate::tgws::send_command(&self.paths, "start");
                        }
                        self.toast("Secret обновлён", ToastKind::Ok);
                    }
                    Err(e) => self.toast(e, ToastKind::Err),
                }
                self.refresh_tg();
            }
            5 => {
                let mut settings = self.settings.clone();
                settings.tg_auto_start = !settings.tg_auto_start;
                if let Err(e) = settings.save(&self.paths.settings_file) {
                    self.toast(e, ToastKind::Err);
                    return;
                }
                self.settings = settings;
                self.toast(
                    if self.settings.tg_auto_start {
                        "Прокси будет запускаться вместе с обходом"
                    } else {
                        "Автозапуск прокси выключен"
                    },
                    ToastKind::Ok,
                );
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------- действия

    fn main_items(&self) -> Vec<(String, Cmd)> {
        let toggle = if self.status.running {
            "Остановить обход".to_string()
        } else {
            "Запустить обход".to_string()
        };
        vec![
            (toggle, Cmd::Toggle),
            ("Авто-подбор стратегии".to_string(), Cmd::Auto),
            ("Проверить связь".to_string(), Cmd::Test),
            ("Выход".to_string(), Cmd::Quit),
        ]
    }

    fn settings_items(&self) -> Vec<(String, Cmd)> {
        let game = match self.settings.game_mode.as_str() {
            "udp" => "UDP",
            "tcp" => "TCP",
            "all" => "TCP + UDP",
            _ => "выключен",
        };
        vec![
            (format!("Игровой режим: {game}"), Cmd::Settings(0)),
            (
                format!(
                    "Автозапуск: {}",
                    if self.autostart { "включён" } else { "выключен" }
                ),
                Cmd::Settings(1),
            ),
            (
                format!(
                    "Служба Windows: {}",
                    if self.service {
                        "переустановить"
                    } else {
                        "установить"
                    }
                ),
                Cmd::Settings(2),
            ),
            ("Удалить службу Windows".to_string(), Cmd::Settings(3)),
            ("Обновить списки IP".to_string(), Cmd::Settings(4)),
            (
                "Удалить службу и автозапуск".to_string(),
                Cmd::Settings(5),
            ),
            ("Открыть папку программы".to_string(), Cmd::Settings(6)),
        ]
    }

    fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Toggle => {
                if self.status.running {
                    self.stop_bypass();
                } else {
                    self.start_bypass();
                }
            }
            Cmd::Auto => self.start_auto(),
            Cmd::Test => self.start_test(),
            Cmd::Update => self.start_update(),
            Cmd::Diag => self.build_diag(),
            Cmd::Preset(_) => self.tab = Tab::Strategy,
            Cmd::Settings(i) => self.settings_action(i),
            Cmd::Quit => self.quitting = true,
        }
    }

    fn settings_action(&mut self, index: usize) {
        match index {
            0 => self.cycle_game_mode(),
            1 => self.toggle_autostart(),
            2 => self.install_service(),
            3 => self.remove_service(),
            4 => self.start_update(),
            5 => self.uninstall_all(),
            6 => winproc::open_path(&self.paths.root),
            _ => {}
        }
    }

    fn guard_job(&mut self) -> bool {
        if self.job.is_some() {
            self.toast("Дождитесь завершения текущей операции", ToastKind::Info);
            true
        } else {
            false
        }
    }

    fn start_bypass(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Нужны права администратора", ToastKind::Err);
            return;
        }
        self.job = Some("запускаю обход…".into());
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let res = winproc::start_bypass(&paths, &settings, &logger);
            let _ = tx.send(Msg::Done {
                title: "Обход".into(),
                result: res.map(|_| "включён".to_string()),
            });
        });
    }

    fn stop_bypass(&mut self) {
        if self.guard_job() {
            return;
        }
        self.job = Some("останавливаю…".into());
        let tx = self.tx_clone();
        let logger = self.logger.clone();
        std::thread::spawn(move || {
            winproc::stop_bypass(&logger);
            logger.ok("Обход выключен");
            let _ = tx.send(Msg::Done {
                title: "Обход".into(),
                result: Ok("выключен".into()),
            });
        });
    }

    fn apply_preset(&mut self, file: String) {
        if self.guard_job() {
            return;
        }
        let label = presets::label_for(&file);
        let mut s = self.settings.clone();
        s.preset = presets::resolve(&file);
        if let Err(e) = s.save(&self.paths.settings_file) {
            self.toast(format!("Не удалось сохранить настройки: {e}"), ToastKind::Err);
            return;
        }
        self.settings = s.clone();
        self.selected_preset = s.preset.clone();
        self.logger.info(format!("Стратегия: {label}"));
        self.job = Some(format!("применяю стратегию {label}…"));
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        std::thread::spawn(move || {
            let running = winproc::status(&paths).running;
            let res = if winproc::service_installed(SERVICE_NAME) {
                winproc::install_service(&paths, &s, &logger)
            } else if running {
                winproc::start_bypass(&paths, &s, &logger)
            } else {
                Ok(())
            };
            let _ = tx.send(Msg::Done {
                title: format!("Стратегия {label}"),
                result: res.map(|_| "применена".to_string()),
            });
        });
    }

    fn cycle_game_mode(&mut self) {
        if self.guard_job() {
            return;
        }
        let next = match self.settings.game_mode.as_str() {
            "off" => "udp",
            "udp" => "tcp",
            "tcp" => "all",
            _ => "off",
        };
        let mut s = self.settings.clone();
        s.game_mode = next.to_string();
        if let Err(e) = s.save(&self.paths.settings_file) {
            self.toast(format!("Настройки: {e}"), ToastKind::Err);
            return;
        }
        winproc::sync_game_filter(&self.paths, &s);
        self.settings = s.clone();
        let label = s.game_label().to_string();
        self.logger.ok(format!("Игровой режим: {next}"));
        if winproc::status(&self.paths).running {
            self.job = Some("перезапускаю с новым игровым режимом…".into());
            let tx = self.tx_clone();
            let paths = self.paths.clone();
            let logger = self.logger.clone();
            std::thread::spawn(move || {
                let res = if winproc::service_installed(SERVICE_NAME) {
                    winproc::install_service(&paths, &s, &logger)
                } else {
                    winproc::start_bypass(&paths, &s, &logger)
                };
                let _ = tx.send(Msg::Done {
                    title: "Игровой режим".into(),
                    result: res.map(|_| label),
                });
            });
        } else {
            self.toast(format!("Игровой режим: {label}"), ToastKind::Ok);
        }
    }

    fn toggle_autostart(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Нужны права администратора", ToastKind::Err);
            return;
        }
        let on = !self.autostart;
        self.job = Some(if on {
            "включаю автозапуск…".into()
        } else {
            "выключаю автозапуск…".into()
        });
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let mut s = self.settings.clone();
        std::thread::spawn(move || {
            let res = if on {
                winproc::install_autostart(&paths, &logger)
            } else {
                winproc::remove_autostart(&logger)
            };
            if res.is_ok() {
                s.auto_start = on;
                let _ = s.save(&paths.settings_file);
            }
            let _ = tx.send(Msg::Done {
                title: "Автозапуск".into(),
                result: res.map(|_| {
                    if on {
                        "включён".to_string()
                    } else {
                        "выключен".to_string()
                    }
                }),
            });
        });
    }

    fn install_service(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Нужны права администратора", ToastKind::Err);
            return;
        }
        self.job = Some("устанавливаю службу…".into());
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let res = winproc::install_service(&paths, &settings, &logger);
            let _ = tx.send(Msg::Done {
                title: "Служба Windows".into(),
                result: res.map(|_| "установлена".to_string()),
            });
        });
    }

    fn remove_service(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Нужны права администратора", ToastKind::Err);
            return;
        }
        self.job = Some("удаляю службу…".into());
        let tx = self.tx_clone();
        let logger = self.logger.clone();
        std::thread::spawn(move || {
            let res = winproc::remove_service(&logger);
            let _ = tx.send(Msg::Done {
                title: "Служба Windows".into(),
                result: res.map(|_| "удалена".to_string()),
            });
        });
    }

    fn uninstall_all(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Нужны права администратора", ToastKind::Err);
            return;
        }
        self.job = Some("удаляю службу и автозапуск…".into());
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let mut s = self.settings.clone();
        std::thread::spawn(move || {
            let res = winproc::uninstall_all(&paths, &logger);
            if res.is_ok() {
                s.auto_start = false;
                let _ = s.save(&paths.settings_file);
            }
            let _ = tx.send(Msg::Done {
                title: "Удаление".into(),
                result: res.map(|_| "служба и автозапуск удалены".to_string()),
            });
        });
    }

    fn start_test(&mut self) {
        if self.guard_job() {
            return;
        }
        self.job = Some("проверяю связь…".into());
        self.logger.info("Проверяю доступность сервисов…");
        let tx = self.tx_clone();
        std::thread::spawn(move || {
            let report = netcheck::run_targets(netcheck::TARGETS);
            let _ = tx.send(Msg::Test(report));
        });
    }

    fn build_diag(&mut self) {
        if self.guard_job() {
            return;
        }
        self.job = Some("собираю диагностику…".into());
        self.logger.info("Собираю диагностику…");
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let text = crate::diagnostics::build(&paths, &settings, &logger);
            let res = crate::diagnostics::save(&paths, &text, &logger).map(|_| text);
            let _ = tx.send(Msg::Diag(res));
        });
    }

    fn start_update(&mut self) {
        if self.guard_job() {
            return;
        }
        self.job = Some("обновляю списки IP…".into());
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        std::thread::spawn(move || {
            let res = netcheck::update_ipset(&paths, &logger, None);
            let _ = tx.send(Msg::Update(res));
        });
    }

    fn start_auto(&mut self) {
        if self.guard_job() {
            return;
        }
        if !winproc::is_admin() {
            self.toast("Для авто-подбора нужны права администратора", ToastKind::Err);
            return;
        }
        self.job = Some("идёт авто-подбор…".into());
        self.auto_rows.clear();
        self.auto_result = None;
        self.auto_progress = Some(AutoProgress {
            stage: "baseline",
            index: 0,
            total: self.presets.len(),
            preset: "подготовка".into(),
        });
        self.logger
            .info("Запущен автоподбор стратегии. Это займёт около 2–3 минут.");
        self.tab = Tab::Strategy;
        let tx = self.tx_clone();
        let paths = self.paths.clone();
        let logger = self.logger.clone();
        let settings = self.settings.clone();
        let had_service = self.service;
        std::thread::spawn(move || {
            let (etx, erx) = channel::<AutoEvent>();
            let forward_tx = tx.clone();
            let forward = std::thread::spawn(move || {
                for ev in erx {
                    let _ = forward_tx.send(Msg::Auto(ev));
                }
            });
            autoselect::run(&paths, settings, logger, etx, had_service);
            let _ = forward.join();
        });
    }

    fn tx_clone(&self) -> Sender<Msg> {
        // Канал один: клонируем Sender через сохранённый клон.
        self.tx.clone()
    }

    // -------------------------------------------------------------- отрисовка

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        // Полная перерисовка кадра: убирает «призраков» от прошлых кадров.
        f.render_widget(Clear, area);
        if area.width < 78 || area.height < 22 {
            let msg = Paragraph::new(vec![
                Line::from(Span::styled(
                    "Unkn0WnDPI — окно слишком маленькое",
                    Style::new().fg(RED).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Разверните окно (минимум 78x22).",
                    Style::new().fg(TEXT),
                )),
            ])
            .alignment(ratatui::layout::Alignment::Center);
            f.render_widget(msg, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(8),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_header(f, chunks[0]);
        self.draw_status(f, chunks[1]);
        self.draw_tabs(f, chunks[2]);
        match self.tab {
            Tab::Overview => self.draw_overview(f, chunks[3]),
            Tab::Strategy => self.draw_strategy(f, chunks[3]),
            Tab::Log => self.draw_log(f, chunks[3]),
            Tab::Diag => self.draw_diag(f, chunks[3]),
            Tab::Settings => self.draw_settings(f, chunks[3]),
            Tab::TgWs => self.draw_tgws(f, chunks[3]),
        }
        self.draw_footer(f, chunks[4]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let art = [
            "██  ██ ██  ██ ██  ██ ██  ██ ██████ ██    ██ ██  ██",
            "██  ██ ███ ██ ██ ██  ███ ██ ██  ██ ██    ██ ███ ██",
            "██  ██ ██ ███ ████   ██ ███ ██  ██ ██ ██ ██ ██ ███",
            "██  ██ ██  ██ ██ ██  ██  ██ ██  ██ ███  ███ ██  ██",
            "██████ ██  ██ ██  ██ ██  ██ ██████ ██    ██ ██  ██",
        ];
        let mut lines: Vec<Line> = vec![Line::from("")];
        lines.extend(
            art.iter()
                .map(|l| Line::from(Span::styled(*l, Style::new().fg(GREEN)))),
        );
        lines.push(Line::from(vec![
            Span::styled(
                format!("DPI CONTROL  ·  v{APP_VERSION}"),
                Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  winws {ENGINE_VERSION}  ·  Discord / YouTube / Games"),
                Style::new().fg(DIM),
            ),
        ]));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let (label, color) = if self.status.running {
            (" RUNNING ", GREEN)
        } else if self.status.foreign {
            (" OLD ZAPRET ", YELLOW)
        } else if self.job.is_some() {
            (" WORKING ", YELLOW)
        } else {
            (" STOPPED ", RED)
        };
        let mut spans = vec![
            Span::styled(
                label,
                Style::new()
                    .fg(BLACK)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", presets::label_for(&self.settings.preset)),
                Style::new().fg(BRIGHT),
            ),
            Span::styled(
                format!("  ·  {}", self.settings.game_label()),
                Style::new().fg(DIM),
            ),
        ];
        if self.status.pid > 0 {
            spans.push(Span::styled(
                format!("  ·  PID {}", self.status.pid),
                Style::new().fg(DIM),
            ));
        }

        let second = if let Some(job) = &self.job {
            Line::from(Span::styled(
                format!("  {job}"),
                Style::new().fg(YELLOW),
            ))
        } else if let Some(toast) = &self.toast {
            let c = match toast.kind {
                ToastKind::Ok => GREEN,
                ToastKind::Err => RED,
                ToastKind::Info => YELLOW,
            };
            Line::from(Span::styled(
                format!("  {}", toast.text),
                Style::new().fg(c),
            ))
        } else {
            Line::from("")
        };

        f.render_widget(Paragraph::new(vec![Line::from(spans), second]), area);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        for i in 0..6 {
            let tab = Tab::from_index(i);
            let selected = tab == self.tab;
            let text = format!("[ {} ]", tab.title());
            if selected {
                spans.push(Span::styled(
                    text,
                    Style::new()
                        .fg(BLACK)
                        .bg(GREEN)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(text, Style::new().fg(DIM)));
            }
            if i < 5 {
                spans.push(Span::raw("  "));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_overview(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);

        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(5)])
            .split(chunks[0]);

        let items: Vec<String> = self.main_items().into_iter().map(|x| x.0).collect();
        draw_menu(f, left[0], "ДЕЙСТВИЯ", &items, self.main_sel);

        let tip_block = frame_block("ПОДСКАЗКА");
        let tip_inner = tip_block.inner(left[1]);
        f.render_widget(tip_block, left[1]);
        let tip = if let Some(r) = &self.auto_result {
            vec![
                Line::from(vec![
                    Span::styled("Авто-подбор: ", Style::new().fg(DIM)),
                    Span::styled(
                        format!("{}  {}/{}", r.best_label, r.best_score, r.max_score),
                        Style::new().fg(GREEN),
                    ),
                ]),
                Line::from(Span::styled(
                    "Enter на стратегии — применить её.",
                    Style::new().fg(DIM),
                )),
            ]
        } else {
            vec![
                Line::from(Span::styled(
                    "Сначала выполните авто-подбор.",
                    Style::new().fg(YELLOW),
                )),
                Line::from(Span::styled(
                    "22 стратегии проверяются за 2–3 минуты.",
                    Style::new().fg(DIM),
                )),
            ]
        };
        f.render_widget(Paragraph::new(tip), tip_inner);

        let block = frame_block("ЖУРНАЛ · последние события");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);
        let visible = inner.height as usize;
        if visible > 0 {
            let lines = self.log_lines(visible, self.log_scroll);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
    }

    fn draw_strategy(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(5)])
            .split(area);

        let info_block = frame_block("АВТО-ПОДБОР");
        let info_inner = info_block.inner(chunks[0]);
        f.render_widget(info_block, chunks[0]);

        if let Some(p) = &self.auto_progress {
            let ratio = if p.total == 0 {
                0.0
            } else {
                (p.index as f64 / p.total as f64).clamp(0.0, 1.0)
            };
            let stage = match p.stage {
                "baseline" => "проверяю без обхода".to_string(),
                "testing" => format!("{}  {}/{}", p.preset, p.index, p.total),
                "full" => format!("финал: {}  {}/{}", p.preset, p.index, p.total),
                _ => "завершаю".to_string(),
            };
            let g = Gauge::default()
                .gauge_style(Style::new().fg(GREEN).bg(Color::Rgb(28, 28, 28)))
                .ratio(ratio)
                .label(Span::styled(
                    format!("{:>3}%   {stage}", (ratio * 100.0).round() as i32),
                    Style::new().fg(BRIGHT),
                ));
            let gauge_area = Rect {
                x: info_inner.x,
                y: info_inner.y,
                width: info_inner.width,
                height: 1,
            };
            f.render_widget(g, gauge_area);
        } else {
            let line = if let Some(r) = &self.auto_result {
                let when = chrono::NaiveDateTime::parse_from_str(&r.date, "%Y-%m-%dT%H:%M:%S")
                    .map(|d| d.format("%d.%m.%Y %H:%M").to_string())
                    .unwrap_or_else(|_| r.date.clone());
                Line::from(vec![
                    Span::styled("Лучший: ", Style::new().fg(DIM)),
                    Span::styled(
                        format!("{}  {}/{}", r.best_label, r.best_score, r.max_score),
                        Style::new().fg(GREEN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("   без обхода {}/{}   {when}", r.baseline_score, r.max_score),
                        Style::new().fg(DIM),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    "Авто-подбор ещё не выполнялся — запустите его на вкладке ОБЗОР",
                    Style::new().fg(YELLOW),
                ))
            };
            f.render_widget(Paragraph::new(line), info_inner);
        }

        let block = frame_block("СТРАТЕГИИ ZAPRET");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);
        let visible = inner.height as usize;
        if visible == 0 {
            return;
        }

        let list: Vec<String> = self
            .presets
            .iter()
            .map(|p| {
                let mut score = String::new();
                if let Some(r) = self.auto_rows.iter().find(|r| r.preset == p.file) {
                    if r.full_score > 0 {
                        score = format!("  {:>2}/{}", r.full_score, r.full_max);
                    } else {
                        score = format!("  {:>2}/7", r.core_score);
                    }
                }
                let mark = if p.file == self.settings.preset {
                    "* "
                } else {
                    "  "
                };
                format!("{mark}{}{}", p.label, score)
            })
            .collect();
        draw_menu_inner(f, inner, &list, self.strategy_sel);
    }

    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        let block = frame_block("ЖУРНАЛ · logs/Unkn0WnDPI.log");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let visible = inner.height as usize;
        if visible == 0 {
            return;
        }
        let lines = self.log_lines(visible, self.log_scroll);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn draw_diag(&mut self, f: &mut Frame, area: Rect) {
        let block = frame_block("ДИАГНОСТИКА · Enter — собрать отчёт");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let text = if self.diag_text.is_empty() {
            "Отчёт ещё не собран.\n\nНажмите Enter — программа проверит службы, конфликты,\nVPN, прокси, DNS, hosts и доступность сервисов.\n\nГотовый отчёт сохраняется в logs/diag-latest.txt".to_string()
        } else {
            self.diag_text.clone()
        };
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(TEXT))
            .wrap(Wrap { trim: false })
            .scroll((self.diag_scroll, 0));
        f.render_widget(paragraph, inner);
    }

    fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(9)])
            .split(area);

        let items: Vec<String> = self.settings_items().into_iter().map(|x| x.0).collect();
        draw_menu(f, chunks[0], "НАСТРОЙКИ И СЛУЖБЫ", &items, self.settings_sel);

        let block = frame_block("О ПРОГРАММЕ");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);
        let ipset = std::fs::read_to_string(self.paths.lists.join("ipset-all.txt"))
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        let lines = vec![
            Line::from(vec![
                Span::styled("Версия  ", Style::new().fg(DIM)),
                Span::styled(
                    format!("{APP_VERSION}   zapret {ENGINE_VERSION}"),
                    Style::new().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("Папка   ", Style::new().fg(DIM)),
                Span::styled(self.paths.root.display().to_string(), Style::new().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("Списки  ", Style::new().fg(DIM)),
                Span::styled(format!("{ipset} строк (Flowseal)"), Style::new().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("Права   ", Style::new().fg(DIM)),
                Span::styled(
                    if winproc::is_admin() {
                        "администратор"
                    } else {
                        "обычный пользователь"
                    },
                    Style::new().fg(if winproc::is_admin() { GREEN } else { YELLOW }),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Не VPN: не меняет IP и не обходит серверные гео-блокировки.",
                Style::new().fg(DIM),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_tgws(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(9)])
            .split(area);

        let items: Vec<String> = self.tg_items().into_iter().map(|x| x.0).collect();
        draw_menu(f, chunks[0], "TELEGRAM · MTPROTO-ПРОКСИ", &items, self.tg_sel);

        let block = frame_block("СОСТОЯНИЕ ПРОКСИ");
        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);

        let (state_text, state_color) = if self.tg_status.running && self.tg_status.listening {
            ("РАБОТАЕТ", GREEN)
        } else if self.tg_status.running {
            ("ЗАПУСКАЕТСЯ…", YELLOW)
        } else if self.tg_status.external {
            ("СТОРОННИЙ TGWSPROXY", YELLOW)
        } else {
            ("ОСТАНОВЛЕН", RED)
        };
        let link = crate::tgws::link(&self.settings);
        let lines = vec![
            Line::from(vec![
                Span::styled("Статус  ", Style::new().fg(DIM)),
                Span::styled(
                    format!(" {state_text} "),
                    Style::new().fg(BLACK).bg(state_color).add_modifier(Modifier::BOLD),
                ),
                if self.tg_status.running {
                    Span::styled("   внутри программы", Style::new().fg(DIM))
                } else {
                    Span::raw("")
                },
            ]),
            Line::from(vec![
                Span::styled("Адрес   ", Style::new().fg(DIM)),
                Span::styled(
                    format!("127.0.0.1:{}", self.settings.tg_port),
                    Style::new().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("Ссылка  ", Style::new().fg(DIM)),
                Span::styled(link, Style::new().fg(GREEN)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Встроенный MTProto-прокси (протокол tg-ws-proxy, Flowseal, MIT).",
                Style::new().fg(DIM),
            )),
            Line::from(Span::styled(
                "Без внешних exe: работает в этом процессе, Telegram → WS → DC.",
                Style::new().fg(DIM),
            )),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let hints = match self.tab {
            Tab::Overview => "↑↓ выбор    ←→ раздел    Enter выполнить    Esc выход",
            Tab::Strategy => "↑↓ стратегия    Enter применить    ←→ раздел    Esc назад",
            Tab::Log => "↑↓ прокрутка    Enter к последним строкам    ←→ раздел    Esc назад",
            Tab::Diag => "↑↓ прокрутка    Enter собрать отчёт    ←→ раздел    Esc назад",
            Tab::Settings => "↑↓ выбор    Enter изменить    ←→ раздел    Esc назад",
            Tab::TgWs => "↑↓ выбор    Enter выполнить    ←→ раздел    Esc назад",
        };
        let line = Line::from(vec![
            Span::styled(hints, Style::new().fg(DIM)),
            Span::styled("    F4 выход", Style::new().fg(DIM)),
            Span::styled(
                if winproc::is_admin() {
                    "    [ ADMIN ]"
                } else {
                    "    [ USER ]"
                },
                Style::new().fg(if winproc::is_admin() { GREEN } else { YELLOW }),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn log_lines(&self, visible: usize, extra: usize) -> Vec<Line<'static>> {
        let all = self.logger.snapshot();
        let want = visible + extra;
        let start = all.len().saturating_sub(want);
        let mut lines: Vec<Line<'static>> = all[start..]
            .iter()
            .map(|l| {
                let color = match l.level {
                    Level::Error => RED,
                    Level::Warn => YELLOW,
                    Level::Ok => GREEN,
                    Level::Info => TEXT,
                };
                Line::from(Span::styled(
                    format!(
                        "{} [{}] {}",
                        l.time,
                        l.level.tag(),
                        l.text
                    ),
                    Style::new().fg(color),
                ))
            })
            .collect();
        if extra > 0 {
            let keep = lines.len().saturating_sub(extra);
            lines.truncate(keep);
        }
        lines
    }
}

// ------------------------------------------------------------------ виджеты

fn frame_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(GREEN).add_modifier(Modifier::BOLD),
        ))
}

fn draw_menu(f: &mut Frame, area: Rect, title: &str, items: &[String], selected: usize) {
    let block = frame_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_menu_inner(f, inner, items, selected);
}

fn draw_menu_inner(f: &mut Frame, inner: Rect, items: &[String], selected: usize) {
    let visible = inner.height as usize;
    if visible == 0 || items.is_empty() {
        return;
    }
    let offset = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, text)| {
            if i == selected {
                Line::from("")
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(text.clone(), Style::new().fg(TEXT)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    let row = inner.y + (selected - offset) as u16;
    if row < inner.y + inner.height {
        let rect = Rect {
            x: inner.x,
            y: row,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Block::default().style(Style::new().bg(GREEN)),
            rect,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("> {}", items[selected]),
                Style::new().fg(BLACK).add_modifier(Modifier::BOLD),
            ))),
            rect,
        );
    }
}
