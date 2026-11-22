//! Обновление списков доменов и IP из GitHub.
//!
//! winws сам перечитывает hostlist/ipset файлы, когда меняются их время
//! изменения и размер, поэтому обновление применяется на лету — без
//! перезапуска программы и без перезапуска обхода.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::logging::Logger;
use crate::netcheck;
use crate::paths::Paths;

/// GitHub-зеркала. Первым идёт raw.githubusercontent.com, затем jsDelivr —
/// он помогает там, где raw GitHub недоступен.
const BASES: &[&str] = &[
    "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main",
    "https://cdn.jsdelivr.net/gh/Flowseal/zapret-discord-youtube@main",
    "https://fastly.jsdelivr.net/gh/Flowseal/zapret-discord-youtube@main",
];

const MAX_BODY: usize = 48 * 1024 * 1024;
const MIN_IPSET_LINES: usize = 1000;

#[derive(Clone, Copy)]
enum Kind {
    Ipset,
    Hostlist,
}

struct Source {
    name: &'static str,
    path: &'static str,
    kind: Kind,
    min_lines: usize,
}

const SOURCES: &[Source] = &[
    Source {
        name: "ipset-all.txt",
        path: ".service/ipset-service.txt",
        kind: Kind::Ipset,
        min_lines: MIN_IPSET_LINES,
    },
    Source {
        name: "list-general.txt",
        path: "lists/list-general.txt",
        kind: Kind::Hostlist,
        min_lines: 10,
    },
    Source {
        name: "list-google.txt",
        path: "lists/list-google.txt",
        kind: Kind::Hostlist,
        min_lines: 10,
    },
    Source {
        name: "list-exclude.txt",
        path: "lists/list-exclude.txt",
        kind: Kind::Hostlist,
        min_lines: 10,
    },
    Source {
        name: "ipset-exclude.txt",
        path: "lists/ipset-exclude.txt",
        kind: Kind::Ipset,
        min_lines: 5,
    },
];

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub updated: Vec<(String, usize)>,
    pub unchanged: Vec<(String, usize)>,
    pub failures: Vec<(String, String)>,
}

impl UpdateReport {
    /// Короткая строка для всплывающего уведомления.
    pub fn toast(&self) -> String {
        let mut parts = Vec::new();
        if !self.updated.is_empty() {
            let lines: usize = self.updated.iter().map(|(_, n)| *n).sum();
            parts.push(format!(
                "обновлено {} файлов, {} строк",
                self.updated.len(),
                lines
            ));
        }
        if !self.unchanged.is_empty() {
            parts.push(format!("актуально {}", self.unchanged.len()));
        }
        if !self.failures.is_empty() {
            parts.push(format!("ошибок {}", self.failures.len()));
        }
        parts.join(" · ")
    }

    /// Подробности для консольного режима `--update-lists`.
    pub fn detail(&self) -> String {
        let mut out = String::new();
        for (name, count) in &self.updated {
            out.push_str(&format!("  {name}: обновлено, {count} строк\n"));
        }
        for (name, count) in &self.unchanged {
            out.push_str(&format!("  {name}: без изменений, {count} строк\n"));
        }
        for (name, err) in &self.failures {
            out.push_str(&format!("  {name}: ошибка — {err}\n"));
        }
        out
    }
}

/// Скачивает и применяет все списки. Ошибка возвращается только если не удалось
/// получить ни один файл (частичный успех отдаётся в отчёте).
pub fn update_all(paths: &Paths, log: &Logger) -> Result<UpdateReport, String> {
    log.info("Обновляю списки и IP из GitHub…");
    let fetched = fetch_all();
    let backup_dir = paths.lists.join("backup");
    let mut report = UpdateReport::default();

    for (src, result) in fetched {
        match result {
            Err(err) => {
                log.warn(format!("{}: {err}", src.name));
                report.failures.push((src.name.to_string(), err));
            }
            Ok(raw) => {
                let text = normalize(&raw);
                match validate(&text, src.kind, src.min_lines) {
                    Err(err) => {
                        log.warn(format!("{}: {err}", src.name));
                        report.failures.push((src.name.to_string(), err));
                    }
                    Ok(count) => {
                        let dest = paths.lists.join(src.name);
                        let old = fs::read(&dest).ok();
                        if old.as_deref() == Some(text.as_bytes()) {
                            log.info(format!("{}: без изменений ({count} строк)", src.name));
                            report.unchanged.push((src.name.to_string(), count));
                            continue;
                        }
                        if let Err(err) =
                            backup_and_replace(paths, src, &dest, text.as_bytes(), &backup_dir)
                        {
                            log.error(format!("{}: не удалось записать: {err}", src.name));
                            report.failures.push((src.name.to_string(), err));
                        } else {
                            log.ok(format!("{}: {count} строк — обновлено", src.name));
                            report.updated.push((src.name.to_string(), count));
                        }
                    }
                }
            }
        }
    }

    if report.updated.is_empty() && report.unchanged.is_empty() {
        let msg = report
            .failures
            .iter()
            .map(|(name, err)| format!("{name}: {err}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if msg.is_empty() {
            "не удалось получить списки".into()
        } else {
            msg
        });
    }

    if report.failures.is_empty() {
        if report.updated.is_empty() {
            log.ok("Списки и IP уже актуальны — обновление не требуется.");
        } else {
            log.ok("Списки обновлены. winws применит их автоматически, без перезапуска.");
        }
    } else if report.updated.is_empty() {
        log.warn("Не удалось обновить часть списков, остальные актуальны.");
    } else {
        log.warn(format!(
            "Обновление частичное: ошибок {}. Обновлённые файлы уже применяются без перезапуска.",
            report.failures.len()
        ));
    }
    Ok(report)
}

fn fetch_all() -> Vec<(&'static Source, Result<String, String>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = SOURCES
            .iter()
            .map(|src| scope.spawn(move || (src, fetch_source(src))))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    })
}

fn fetch_source(src: &Source) -> Result<String, String> {
    let mut errors = Vec::new();
    for base in BASES {
        let url = format!("{base}/{}", src.path);
        let mut last = String::new();
        for _ in 0..2 {
            match fetch_follow(&url) {
                Ok(text) => return Ok(text),
                Err(err) => last = err,
            }
        }
        let host = base.split("://").nth(1).unwrap_or(base);
        errors.push(format!("{host}: {last}"));
    }
    Err(errors.join(" | "))
}

fn fetch_follow(start: &str) -> Result<String, String> {
    let mut url = start.to_string();
    for _ in 0..6 {
        let (code, body, location) =
            netcheck::https_request(&url, Duration::from_secs(25), MAX_BODY)?;
        if (300..400).contains(&code) {
            match location {
                Some(location) => {
                    url = resolve_url(&url, &location);
                    continue;
                }
                None => return Err(format!("HTTP {code} без Location")),
            }
        }
        if code != 200 {
            return Err(format!("HTTP {code}"));
        }
        if body.is_empty() {
            return Err("пустой ответ".into());
        }
        return Ok(String::from_utf8_lossy(&body).into_owned());
    }
    Err("слишком много перенаправлений".into())
}

fn resolve_url(base: &str, location: &str) -> String {
    if location.starts_with("https://") || location.starts_with("http://") {
        return location.to_string();
    }
    if location.starts_with("//") {
        return format!("https:{location}");
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    if location.starts_with('/') {
        return format!("{}{}", &base[..host_end], location);
    }
    let dir_end = base.rfind('/').unwrap_or(base.len());
    format!("{}/{}", &base[..dir_end], location)
}

/// Приводит файл к CRLF и гарантирует перевод строки в конце.
fn normalize(raw: &str) -> String {
    let raw = raw.trim_start_matches('\u{feff}');
    let mut out = String::with_capacity(raw.len() + raw.len() / 16 + 2);
    for line in raw.lines() {
        out.push_str(line.trim_end_matches('\r'));
        out.push_str("\r\n");
    }
    out
}

fn validate(text: &str, kind: Kind, min_lines: usize) -> Result<usize, String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<html") {
        return Err("сервер вернул HTML вместо списка".into());
    }
    let mut total = 0usize;
    let mut bad = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        total += 1;
        let ok = match kind {
            Kind::Ipset => is_ipset_line(line),
            Kind::Hostlist => is_host_line(line),
        };
        if !ok {
            bad += 1;
        }
    }
    if total < min_lines {
        return Err(format!("слишком мало строк: {total} (ожидалось ≥{min_lines})"));
    }
    if bad > 0 && bad * 20 > total {
        return Err(format!("некорректных строк: {bad} из {total}"));
    }
    Ok(total)
}

fn is_ipset_line(line: &str) -> bool {
    let (ip, prefix) = match line.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix)),
        None => (line, None),
    };
    let ip: std::net::IpAddr = match ip.parse() {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    match prefix {
        None => true,
        Some(prefix) => match prefix.parse::<u8>() {
            Ok(bits) => bits <= if ip.is_ipv4() { 32 } else { 128 },
            Err(_) => false,
        },
    }
}

fn is_host_line(line: &str) -> bool {
    let line = line.trim_start_matches('^');
    !line.is_empty()
        && line.len() <= 253
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*'))
}

fn backup_and_replace(
    paths: &Paths,
    src: &Source,
    dest: &Path,
    bytes: &[u8],
    backup_dir: &Path,
) -> Result<(), String> {
    if dest.exists() {
        let _ = fs::create_dir_all(backup_dir);
        let _ = fs::copy(dest, backup_dir.join(src.name));
        if src.name.eq_ignore_ascii_case("ipset-all.txt") {
            let _ = fs::copy(dest, paths.lists.join("ipset-all.txt.backup"));
        }
    }
    replace_file(dest, bytes)
}

fn replace_file(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let old = fs::metadata(dest).ok();
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lists.tmp");
    let tmp = dest.with_file_name(format!(".{file_name}.tmp"));
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;

    let mut last = String::new();
    for _ in 0..15 {
        match fs::rename(&tmp, dest) {
            Ok(()) => {
                bump_if_ambiguous(dest, old.as_ref());
                return Ok(());
            }
            Err(err) => {
                last = err.to_string();
                std::thread::sleep(Duration::from_millis(80));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    fs::write(dest, bytes).map_err(|e| format!("rename: {last}; write: {e}"))?;
    bump_if_ambiguous(dest, old.as_ref());
    Ok(())
}

/// winws сравнивает только секунды изменения и размер. Если файл заменён в ту же
/// секунду и с тем же размером, сдвигаем mtime, чтобы reload точно сработал.
fn bump_if_ambiguous(dest: &Path, old: Option<&fs::Metadata>) {
    let Some(old) = old else { return };
    let old_secs = old
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let Ok(new) = fs::metadata(dest) else { return };
    let new_secs = new
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    if old.len() == new.len() && old_secs == new_secs {
        if let Ok(file) = fs::OpenOptions::new().write(true).open(dest) {
            let when = SystemTime::now() + Duration::from_secs(1);
            let _ = file.set_times(fs::FileTimes::new().set_modified(when));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipset_validation_accepts_ipv4_and_ipv6() {
        let text = "1.1.1.0/24\r\n8.8.8.8\r\n2a00:1450::/32\r\n";
        assert_eq!(validate(text, Kind::Ipset, 3).unwrap(), 3);
    }

    #[test]
    fn ipset_validation_rejects_html() {
        let text = "<!DOCTYPE html><html><body>error</body></html>";
        assert!(validate(text, Kind::Ipset, 1).is_err());
    }

    #[test]
    fn hostlist_validation_accepts_caret_and_wildcard() {
        let text = "^dns.google\r\nyoutube.com\r\n*.example.org\r\n";
        assert_eq!(validate(text, Kind::Hostlist, 3).unwrap(), 3);
    }

    #[test]
    fn normalize_uses_crlf() {
        assert_eq!(normalize("a\nb\r\nc"), "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn replace_file_swaps_contents() {
        let dir = std::env::temp_dir().join(format!(
            "unkn0wndpi-update-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("list.txt");
        fs::write(&dest, b"old").unwrap();
        replace_file(&dest, b"new").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }
}
