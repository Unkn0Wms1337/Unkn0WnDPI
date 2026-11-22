#![allow(dead_code)]

mod autoselect;
mod diagnostics;
mod engine;
mod guard;
mod logging;
mod netcheck;
mod paths;
mod presets;
mod settings;
mod tgws;
mod tray;
mod tui;
mod updater;
mod winproc;

use logging::Logger;
use paths::Paths;
use settings::Settings;

fn wait_enter() {
    println!("\nНажмите Enter, чтобы закрыть окно…");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // Фоновый режим: только трей, без консоли.
    if raw_args.iter().any(|a| a == "--tray-guard") {
        let args = raw_args
            .iter()
            .filter(|a| *a != "--tray-guard")
            .cloned()
            .collect();
        guard::run_guard(args);
        return;
    }

    tui::init_console();

    let from_autostart = raw_args.iter().any(|a| a == "--from-autostart");
    let no_elevate = raw_args.iter().any(|a| a == "--no-elevate");
    let run_selftest = raw_args.iter().any(|a| a == "--selftest");

    if run_selftest {
        selftest_main();
        return;
    }

    // Обновление списков без запуска интерфейса (удобно для планировщика).
    if raw_args.iter().any(|a| a == "--update-lists") {
        update_lists_main();
        return;
    }

    // Только для отладки: поднять встроенный MTProto-прокси без TUI.
    #[cfg(debug_assertions)]
    if raw_args.iter().any(|a| a == "--tgws-test") {
        let paths = Paths::init();
        let logger = Logger::new(&paths.log_file, false);
        let mut settings = Settings::load(&paths.settings_file);
        if let Err(e) = tgws::ensure(&paths, &mut settings, &logger) {
            logger.error(e);
        }
        match tgws::start(&paths, &settings, &logger) {
            Ok(_) => {
                println!("Встроенный MTProto-прокси работает. Ctrl+C — выход.");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            Err(e) => logger.error(e),
        }
        return;
    }

    // Один экземпляр: второй запуск не плодит копии и не мешает обходу.
    let mut instance = winproc::acquire_instance();
    if instance.is_none() && winproc::console_window_exists() {
        println!("Unkn0WnDPI уже запущен.");
        println!("Переключитесь в его консольное окно.");
        winproc::show_message(
            "Unkn0WnDPI",
            "Unkn0WnDPI уже запущен.\n\nПереключитесь в его консольное окно.",
            false,
        );
        wait_enter();
        return;
    }

    if !winproc::is_admin() && !no_elevate {
        drop(instance);
        if !winproc::elevate(&raw_args) {
            winproc::show_message(
                "Unkn0WnDPI",
                "Для работы нужны права администратора.\nЗапустите Unkn0WnDPI.exe от имени администратора.",
                true,
            );
        }
        return;
    }

    // Мьютекс занят, но окна нет — остался «зомби» от старой сборки
    // (консоль без окна). С правами администратора убираем его и продолжаем.
    if instance.is_none() {
        let killed = winproc::kill_stale_consoles();
        if killed > 0 {
            eprintln!("Завершено зависших копий Unkn0WnDPI: {killed}");
        }
        for _ in 0..40 {
            instance = winproc::acquire_instance();
            if instance.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if instance.is_none() {
            winproc::show_message(
                "Unkn0WnDPI",
                "Unkn0WnDPI уже запущен, но его окно недоступно.\n\nЗавершите процесс Unkn0WnDPI.exe через Диспетчер задач и запустите снова.",
                true,
            );
            wait_enter();
            return;
        }
    }

    let paths = Paths::init();
    // Стражу нужен PID консоли, чтобы уметь перезапускать её, если окно пропало.
    let _ = std::fs::write(
        paths.config.join("console.pid"),
        std::process::id().to_string(),
    );
    let logger = Logger::new(&paths.log_file, true);
    logger.info(format!(
        "Unkn0WnDPI {} запущен ({})",
        engine::APP_VERSION,
        if winproc::is_admin() {
            "администратор"
        } else {
            "без прав администратора"
        }
    ));

    match engine::ensure_engine(&paths) {
        Ok(written) => {
            if written > 0 {
                logger.info(format!(
                    "Движок распакован: {written} файлов → {}",
                    paths.engine.display()
                ));
            }
        }
        Err(e) => {
            logger.error(format!("Не удалось распаковать движок: {e}"));
            winproc::show_message(
                "Unkn0WnDPI",
                &format!(
                    "Не удалось распаковать движок в папку:\n{}\n\n{e}\n\nДобавьте папку в исключения антивируса и запустите снова.",
                    paths.engine.display()
                ),
                true,
            );
            wait_enter();
            return;
        }
    }
    engine::ensure_user_lists(&paths);

    let mut settings = Settings::load(&paths.settings_file);
    settings.preset = presets::resolve(&settings.preset);
    if let Err(e) = settings.save(&paths.settings_file) {
        logger.warn(format!("Не удалось сохранить настройки: {e}"));
    }
    if let Err(e) = tgws::ensure(&paths, &mut settings, &logger) {
        logger.warn(format!("tg-ws-proxy: не удалось подготовить: {e}"));
    }
    logger.info(format!(
        "Стратегия: {} · игровой режим: {}",
        presets::label_for(&settings.preset),
        settings.game_mode
    ));

    guard::spawn_guard(&raw_args);
    let ui = tui::run(paths, logger, settings, from_autostart);
    // Консоль закрывается штатно — страж трея выходит вместе с ней.
    guard::signal_quit_all();
    if let Err(e) = ui {
        winproc::show_message("Unkn0WnDPI", &format!("Ошибка интерфейса: {e}"), true);
    }
}

fn update_lists_main() {
    let paths = Paths::init();
    let logger = Logger::new(&paths.log_file, false);
    engine::ensure_user_lists(&paths);
    match updater::update_all(&paths, &logger) {
        Ok(report) => {
            print!("{}", report.detail());
            if !report.failures.is_empty() {
                eprintln!("Обновление завершено частично: не все файлы удалось получить.");
                std::process::exit(1);
            }
            println!("Готово. Перезапуск не требуется: winws перечитывает списки автоматически.");
        }
        Err(e) => {
            eprintln!("Не удалось обновить списки: {e}");
            std::process::exit(1);
        }
    }
}

fn selftest_main() {
    let paths = Paths::init();
    let logger = Logger::new(&paths.log_file, false);
    if let Err(e) = engine::ensure_engine(&paths) {
        logger.error(format!("Не удалось распаковать движок: {e}"));
    }
    engine::ensure_user_lists(&paths);
    let mut settings = Settings::load(&paths.settings_file);
    settings.preset = presets::resolve(&settings.preset);
    selftest(&paths, &logger, &settings);
}

fn selftest(paths: &Paths, logger: &Logger, settings: &Settings) {
    println!("Unkn0WnDPI self-test");
    println!("root: {}", paths.root.display());
    let list = presets::list();
    println!("presets: {}", list.len());
    let mut bad = 0;
    for p in &list {
        match presets::args(
            paths,
            &p.file,
            &settings.game_mode,
            &settings.game_tcp_range,
            &settings.game_udp_range,
        ) {
            Ok(args) => println!("  {:<34} OK  args={}  [{}]", p.file, args.len(), p.label),
            Err(e) => {
                bad += 1;
                println!("  {:<34} FAIL {e}", p.file);
            }
        }
    }
    let report = netcheck::run_targets(netcheck::TARGETS);
    println!(
        "network: {}/{} за {} мс",
        report.score, report.max, report.elapsed_ms
    );
    for r in &report.results {
        println!(
            "  {:<14} {:<4} {:<4} {}",
            r.name,
            if r.ok { "OK" } else { "FAIL" },
            r.code,
            if r.error.is_empty() { "" } else { &r.error }
        );
    }
    let text = diagnostics::build(paths, settings, logger);
    let _ = diagnostics::save(paths, &text, logger);
    println!("diagnostics: {} bytes", text.len());
    if bad > 0 {
        println!("SELFTEST FAIL: {bad} presets");
        std::process::exit(1);
    }
    println!("SELFTEST OK");
}
