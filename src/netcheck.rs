use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use native_tls::TlsConnector;
use serde::{Deserialize, Serialize};

use crate::engine::APP_VERSION;
use crate::logging::{Level, Logger};

#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub name: &'static str,
    pub url: &'static str,
    pub weight: u32,
    pub core: bool,
}

pub const TARGETS: &[Target] = &[
    Target { name: "Discord API", url: "https://discord.com/api/v10/gateway", weight: 3, core: true },
    Target { name: "YouTube", url: "https://www.youtube.com/", weight: 3, core: true },
    Target { name: "Google", url: "https://www.google.com/", weight: 1, core: true },
    Target { name: "Discord CDN", url: "https://cdn.discordapp.com/", weight: 2, core: false },
    Target { name: "YouTube IMG", url: "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg", weight: 2, core: false },
    Target { name: "Cloudflare", url: "https://www.cloudflare.com/", weight: 1, core: false },
    Target { name: "Steam", url: "https://store.steampowered.com/", weight: 1, core: false },
    Target { name: "EA / FIFA", url: "https://www.ea.com/", weight: 1, core: false },
    Target { name: "Call of Duty", url: "https://www.callofduty.com/", weight: 1, core: false },
];

pub const MAX_SCORE: u32 = 15;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestItem {
    pub name: String,
    pub url: String,
    pub weight: u32,
    pub ok: bool,
    pub code: u16,
    pub error: String,
    pub ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestReport {
    pub results: Vec<TestItem>,
    pub elapsed_ms: u128,
    pub score: u32,
    pub max: u32,
}

fn parse_url(url: &str) -> Option<(String, String, u16)> {
    let rest = url.strip_prefix("https://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (hostport.to_string(), 443),
    };
    Some((host, path.to_string(), port))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn connect_any(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let mut addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err("DNS: нет адреса".into());
    }
    // IPv4 первым: у IPv6 часто нет маршрута, и соединение просто висит.
    addrs.sort_by_key(|a| !a.is_ipv4());
    let start = Instant::now();
    let mut last = String::new();
    for addr in addrs {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        let step = remaining.min(Duration::from_millis(1800));
        match TcpStream::connect_timeout(&addr, step) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    if last.is_empty() {
        Err("не удалось подключиться (таймаут)".into())
    } else {
        Err(last)
    }
}

/// Простой HTTPS-запрос. Возвращает статус и тело (для обновления списков).
pub(crate) fn https_request(
    url: &str,
    timeout: Duration,
    max_body: usize,
) -> Result<(u16, Vec<u8>, Option<String>), String> {
    let (host, path, port) = parse_url(url).ok_or_else(|| "некорректный URL".to_string())?;
    let stream = connect_any(&host, port, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let connector = TlsConnector::new().map_err(|e| e.to_string())?;
    let mut tls = connector
        .connect(&host, stream)
        .map_err(|e| e.to_string())?;

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Unkn0WnDPI/{APP_VERSION} (Windows)\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    tls.flush().ok();

    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let header_end;
    loop {
        let n = tls.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("сервер закрыл соединение".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err("слишком большой ответ".into());
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("некорректный ответ: {status_line}"))?;

    let mut location = None;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "location" => location = Some(v.to_string()),
                "transfer-encoding" if v.to_ascii_lowercase().contains("chunked") => chunked = true,
                "content-length" => content_length = v.parse().ok(),
                _ => {}
            }
        }
    }

    let mut body = buf[header_end..].to_vec();
    if max_body > 0 {
        if let Some(len) = content_length {
            let target = len.min(max_body);
            while body.len() < target {
                let n = tls.read(&mut chunk).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
                if body.len() >= max_body {
                    break;
                }
            }
        } else {
            while body.len() < max_body {
                match tls.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        }
        if chunked {
            body = decode_chunked(&body);
        }
        body.truncate(max_body);
    } else {
        body.clear();
    }

    Ok((code, body, location))
}

fn decode_chunked(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let line_end = match data[pos..].windows(2).position(|w| w == b"\r\n") {
            Some(i) => pos + i,
            None => break,
        };
        let line = String::from_utf8_lossy(&data[pos..line_end]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_str, 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(data.len());
        out.extend_from_slice(&data[start..end]);
        pos = end + 2;
    }
    out
}

fn check_target(t: &Target, timeout: Duration) -> TestItem {
    let start = Instant::now();
    let (ok, code, error) = match https_request(t.url, timeout, 0) {
        Ok((code, _, _)) => {
            let ok = code >= 200 && code < 500 && code != 451;
            (ok, code, String::new())
        }
        Err(e) => (false, 0, e),
    };
    TestItem {
        name: t.name.into(),
        url: t.url.into(),
        weight: t.weight,
        ok,
        code,
        error,
        ms: start.elapsed().as_millis(),
    }
}

pub fn run_targets(targets: &[Target]) -> TestReport {
    let start = Instant::now();
    let mut handles = Vec::new();
    for t in targets {
        let t = *t;
        handles.push(std::thread::spawn(move || check_target(&t, Duration::from_secs(4))));
    }
    let mut results = Vec::new();
    for h in handles {
        if let Ok(r) = h.join() {
            results.push(r);
        }
    }
    let score: u32 = results.iter().filter(|r| r.ok).map(|r| r.weight).sum();
    TestReport {
        results,
        elapsed_ms: start.elapsed().as_millis(),
        score,
        max: MAX_SCORE,
    }
}

pub fn core_targets() -> Vec<Target> {
    TARGETS.iter().copied().filter(|t| t.core).collect()
}

pub fn report_lines(report: &TestReport, log: &Logger) {
    for r in &report.results {
        let state = if r.ok { "OK" } else { "FAIL" };
        let detail = if r.code > 0 {
            format!("HTTP {}", r.code)
        } else {
            r.error.clone()
        };
        log.log(
            if r.ok { Level::Ok } else { Level::Warn },
            format!("{:<14} {:<5} {}", r.name, state, detail),
        );
    }
    log.info(format!(
        "Итог проверки: {}/{} за {} мс",
        report.score, report.max, report.elapsed_ms
    ));
}


