use std::path::PathBuf;

use crate::engine::{APP_VERSION, ENGINE_VERSION};
use crate::logging::Logger;
use crate::netcheck;
use crate::paths::Paths;
use crate::presets;
use crate::settings::Settings;
use crate::winproc::{self, SERVICE_NAME};

fn capture(program: &str, args: &[&str]) -> String {
    match winproc::run_hidden(program, args) {
        Ok(out) => {
            let stdout = winproc::decode_console(&out.stdout);
            let stderr = winproc::decode_console(&out.stderr);
            format!("{stdout}{stderr}")
        }
        Err(e) => format!("ошибка запуска {program}: {e}"),
    }
}

fn svc_line(log: &mut String, name: &str) {
    if let Some(state) = winproc::service_state(name) {
        log.push_str(&format!("{name:<18} {state}\n"));
    }
}

pub fn build(paths: &Paths, settings: &Settings, logger: &Logger) -> String {
    let mut s = String::new();
    s.push_str("============================================================\n");
    s.push_str(" Unkn0WnDPI — диагностика\n");
    s.push_str(&format!(
        " Дата: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    s.push_str("============================================================\n\n");

    let ver = capture("cmd.exe", &["/c", "ver"]).trim().to_string();
    s.push_str(&format!("Windows: {ver}\n"));
    s.push_str(&format!(
        "Компьютер: {}\n",
        std::env::var("COMPUTERNAME").unwrap_or_default()
    ));
    s.push_str(&format!(
        "Пользователь: {}\n",
        std::env::var("USERNAME").unwrap_or_default()
    ));
    s.push_str(&format!("Права администратора: {}\n", winproc::is_admin()));
    s.push_str(&format!("Папка: {}\n", paths.root.display()));
    s.push_str(&format!(
        "Версия оболочки: {APP_VERSION} (движок {ENGINE_VERSION})\n\n"
    ));

    let st = winproc::status(paths);
    let preset = presets::resolve(&settings.preset);
    s.push_str("--- Состояние ---\n");
    s.push_str(&format!(
        "Обход: {}\n",
        if st.running { "РАБОТАЕТ" } else { "выключен" }
    ));
    s.push_str(&format!("Режим: {}\n", st.mode.text()));
    s.push_str(&format!("Пресет: {preset}\n"));
    s.push_str(&format!(
        "Игровой режим: {} (TCP {}, UDP {})\n",
        settings.game_mode, settings.game_tcp_range, settings.game_udp_range
    ));
    s.push_str(&format!("Автозапуск: {}\n", winproc::autostart_installed()));
    s.push_str(&format!(
        "Служба {SERVICE_NAME}: {}\n",
        winproc::service_installed(SERVICE_NAME)
    ));
    if st.pid > 0 {
        s.push_str(&format!("PID winws: {}\n", st.pid));
    }
    s.push('\n');

    s.push_str("--- Службы ---\n");
    for name in [
        "BFE",
        "WinDivert",
        "WinDivert14",
        SERVICE_NAME,
        "zapret",
        "GoodbyeDPI",
        "discordfix_zapret",
    ] {
        svc_line(&mut s, name);
    }
    s.push('\n');

    s.push_str("--- Процесс winws ---\n");
    let procs = winproc::winws_processes(paths);
    if procs.is_empty() {
        s.push_str("не запущен\n");
    }
    for p in &procs {
        s.push_str(&format!("PID {}\n", p.pid));
        if let Some(path) = &p.path {
            s.push_str(&format!("{}\n", path.display()));
        }
    }
    s.push('\n');

    s.push_str("--- Возможные конфликты ---\n");
    let conflicts = [
        "adguard",
        "killer",
        "smartbyte",
        "tracsrv",
        "epwd",
        "intelconnectivity",
    ];
    let mut found = false;
    let all = winproc::processes();
    for c in conflicts {
        for p in all.iter().filter(|p| p.name.to_lowercase().contains(c)) {
            s.push_str(&format!("процесс: {} (PID {})\n", p.name, p.pid));
            found = true;
        }
    }
    for name in [
        "AdguardSvc",
        "Adguard",
        "Killer",
        "SmartByte",
        "TracSrvWrapper",
        "EPWD",
    ] {
        if winproc::service_running(name) {
            s.push_str(&format!("служба: {name}\n"));
            found = true;
        }
    }
    if !found {
        s.push_str("не найдены\n");
    }
    s.push('\n');

    s.push_str("--- VPN-адаптеры ---\n");
    let ipconfig = capture("ipconfig.exe", &["/all"]);
    let mut vpn_found = false;
    for line in ipconfig.lines() {
        let low = line.to_lowercase();
        if ["vpn", "wireguard", "openvpn", "tap-", "tun", "wintun", "happ"]
            .iter()
            .any(|k| low.contains(k))
        {
            let t = line.trim();
            if !t.is_empty() {
                s.push_str(&format!("{t}\n"));
                vpn_found = true;
            }
        }
    }
    if !vpn_found {
        s.push_str("не найдены (это хорошо)\n");
    }
    s.push('\n');

    s.push_str("--- Прокси ---\n");
    let reg = capture(
        "reg.exe",
        &[
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
            "/v",
            "ProxyEnable",
        ],
    );
    if reg.contains("0x1") {
        let server = capture(
            "reg.exe",
            &[
                "query",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "ProxyServer",
            ],
        );
        s.push_str(&format!("включён: {}\n", server.trim()));
    } else {
        s.push_str("выключен\n");
    }
    s.push('\n');

    s.push_str("--- DNS ---\n");
    let dns = capture("netsh.exe", &["interface", "ip", "show", "dns"]);
    let mut dns_lines = 0;
    for line in dns.lines().take(60) {
        let t = line.trim();
        if !t.is_empty() {
            s.push_str(t);
            s.push('\n');
            dns_lines += 1;
        }
    }
    if dns_lines == 0 {
        s.push_str("не удалось получить\n");
    }
    s.push('\n');

    s.push_str("--- TCP timestamps ---\n");
    let tcp = capture("netsh.exe", &["interface", "tcp", "show", "global"]);
    let mut shown = false;
    for line in tcp.lines() {
        let low = line.to_lowercase();
        if low.contains("timestamp") || low.contains("метк") || low.contains("1323") {
            s.push_str(line.trim());
            s.push('\n');
            shown = true;
        }
    }
    if !shown {
        s.push_str("не удалось проверить\n");
    }
    s.push('\n');

    s.push_str("--- hosts (youtube/discord) ---\n");
    let hosts = std::env::var("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("C:\\Windows"))
        .join("System32\\drivers\\etc\\hosts");
    let mut hosts_found = false;
    if let Ok(text) = std::fs::read_to_string(&hosts) {
        for line in text.lines() {
            let low = line.to_lowercase();
            if !low.trim_start().starts_with('#')
                && ["youtube", "youtu.be", "discord", "googlevideo"]
                    .iter()
                    .any(|k| low.contains(k))
            {
                s.push_str(line.trim());
                s.push('\n');
                hosts_found = true;
            }
        }
    }
    if !hosts_found {
        s.push_str("записей нет\n");
    }
    s.push('\n');

    s.push_str("--- Последние строки журнала ---\n");
    for line in logger.tail(40) {
        s.push_str(&format!(
            "{} [{}] {}\n",
            line.time,
            line.level.tag(),
            line.text
        ));
    }
    s.push('\n');

    s.push_str("--- Проверка сети (сейчас) ---\n");
    let report = netcheck::run_targets(netcheck::TARGETS);
    for x in &report.results {
        s.push_str(&format!(
            "{:<14} {:<5} HTTP {}\n",
            x.name,
            if x.ok { "OK" } else { "FAIL" },
            x.code
        ));
    }
    s.push_str(&format!(
        "Итого: {}/{}, {} мс\n\n",
        report.score, report.max, report.elapsed_ms
    ));
    s.push_str("Конец отчёта.\n");
    s
}

pub fn save(paths: &Paths, text: &str, logger: &Logger) -> Result<PathBuf, String> {
    std::fs::write(&paths.diag_file, text).map_err(|e| e.to_string())?;
    let copy = paths.logs.join(format!(
        "diag-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let _ = std::fs::copy(&paths.diag_file, &copy);
    logger.ok(format!("Диагностика сохранена: {}", paths.diag_file.display()));
    Ok(paths.diag_file.clone())
}
