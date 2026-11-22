use crate::engine::PRESET_FILES;
use crate::paths::Paths;

#[derive(Clone, Debug)]
pub struct Preset {
    pub file: String,
    pub label: String,
}

pub fn label_for(file: &str) -> String {
    let stem = file
        .strip_suffix(".bat")
        .or_else(|| file.strip_suffix(".BAT"))
        .unwrap_or(file);
    let mut name = stem;
    if let Some(rest) = name.strip_prefix("general") {
        name = rest;
    }
    let name = name.trim().trim_matches(|c| c == '(' || c == ')').trim();
    if name.is_empty() {
        "general — базовый".to_string()
    } else {
        name.to_string()
    }
}

fn preset_rank(file: &str) -> u8 {
    if file.eq_ignore_ascii_case("general.bat") {
        0
    } else if file.eq_ignore_ascii_case("general (UNIVERSAL 1).bat") {
        1
    } else if file.eq_ignore_ascii_case("general (UNIVERSAL 2).bat") {
        2
    } else {
        3
    }
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ia = a.chars().peekable();
    let mut ib = b.chars().peekable();
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na: u64 = 0;
                    while let Some(&c) = ia.peek() {
                        if c.is_ascii_digit() {
                            na = na.saturating_mul(10).saturating_add((c as u64) - ('0' as u64));
                            ia.next();
                        } else {
                            break;
                        }
                    }
                    let mut nb: u64 = 0;
                    while let Some(&c) = ib.peek() {
                        if c.is_ascii_digit() {
                            nb = nb.saturating_mul(10).saturating_add((c as u64) - ('0' as u64));
                            ib.next();
                        } else {
                            break;
                        }
                    }
                    match na.cmp(&nb) {
                        std::cmp::Ordering::Equal => {}
                        other => return other,
                    }
                } else {
                    let la = ca.to_ascii_lowercase();
                    let lb = cb.to_ascii_lowercase();
                    match la.cmp(&lb) {
                        std::cmp::Ordering::Equal => {
                            ia.next();
                            ib.next();
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

/// general.bat первым, затем универсальные, далее остальные — по имени
/// (натуральная сортировка, чтобы ALT2 шёл раньше ALT10).
pub fn list() -> Vec<Preset> {
    let mut out: Vec<Preset> = PRESET_FILES
        .iter()
        .map(|(name, _)| Preset {
            file: name.to_string(),
            label: label_for(name),
        })
        .collect();
    out.sort_by(|a, b| {
        preset_rank(&a.file)
            .cmp(&preset_rank(&b.file))
            .then_with(|| natural_cmp(&a.file, &b.file))
    });
    out
}

fn preset_bytes(name: &str) -> Option<Vec<u8>> {
    if let Some((_, data)) = PRESET_FILES
        .iter()
        .find(|(file, _)| file.eq_ignore_ascii_case(name))
    {
        return Some(data.to_vec());
    }
    crate::engine::read_external_preset(name)
}

pub fn resolve(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "general.bat".into();
    }
    if PRESET_FILES
        .iter()
        .any(|(file, _)| file.eq_ignore_ascii_case(name))
    {
        if let Some((file, _)) = PRESET_FILES
            .iter()
            .find(|(file, _)| file.eq_ignore_ascii_case(name))
        {
            return file.to_string();
        }
    }
    for p in list() {
        if p.label.eq_ignore_ascii_case(name) || p.file.eq_ignore_ascii_case(name) {
            return p.file;
        }
    }
    "general.bat".into()
}

fn collect_command(text: &str) -> Option<String> {
    let mut joined: Vec<String> = Vec::new();
    let mut buf = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("::") {
            continue;
        }
        if t.is_empty() {
            if !buf.is_empty() {
                joined.push(std::mem::take(&mut buf));
            }
            continue;
        }
        if t.ends_with('^') {
            buf.push_str(&t[..t.len() - 1]);
            buf.push(' ');
            continue;
        }
        buf.push_str(line);
        joined.push(std::mem::take(&mut buf));
    }
    if !buf.is_empty() {
        joined.push(buf);
    }
    joined
        .into_iter()
        .find(|l| l.to_ascii_lowercase().contains("winws.exe"))
}

/// Разбор командной строки как в Windows: кавычки группируют, `^` экранирует.
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut in_quotes = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if !in_quotes && ch == '^' && i + 1 < chars.len() {
            i += 1;
            cur.push(chars[i]);
            has_token = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            has_token = true;
            i += 1;
            continue;
        }
        if !in_quotes && ch.is_whitespace() {
            if has_token {
                out.push(std::mem::take(&mut cur));
                has_token = false;
            }
            i += 1;
            continue;
        }
        cur.push(ch);
        has_token = true;
        i += 1;
    }
    if has_token {
        out.push(cur);
    }
    out
}

fn leftover_var(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '%' {
            if let Some(end) = bytes[i + 1..].iter().position(|c| *c == '%') {
                let inner: String = bytes[i + 1..i + 1 + end].iter().collect();
                if !inner.is_empty()
                    && inner
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return Some(format!("%{inner}%"));
                }
            }
        }
        i += 1;
    }
    None
}

/// Аргументы winws для пресета (без имени exe).
pub fn args(paths: &Paths, name: &str, game_mode: &str, tcp_range: &str, udp_range: &str) -> Result<Vec<String>, String> {
    let data = preset_bytes(name).ok_or_else(|| format!("Пресет не найден: {name}"))?;
    let text = String::from_utf8_lossy(&data);
    let cmd = collect_command(&text).ok_or_else(|| format!("В пресете {name} не найдена команда winws.exe"))?;
    let lower = cmd.to_ascii_lowercase();
    let idx = lower.find("winws.exe").map(|i| i + "winws.exe".len()).unwrap_or(0);
    let mut raw = cmd[idx..].to_string();
    if let Some(dash) = raw.find("--") {
        raw = raw[dash..].to_string();
    }

    let (mut tcp, mut udp) = ("12".to_string(), "12".to_string());
    match game_mode {
        "tcp" => tcp = tcp_range.to_string(),
        "udp" => udp = udp_range.to_string(),
        "all" => {
            tcp = tcp_range.to_string();
            udp = udp_range.to_string();
        }
        _ => {}
    }

    let bin = format!("{}\\", paths.bin.display());
    let lists = format!("{}\\", paths.lists.display());
    let root = format!("{}\\", paths.engine.display());
    raw = raw.replace("%~dp0", &root);
    raw = raw.replace("%BIN%", &bin);
    raw = raw.replace("%LISTS%", &lists);
    raw = raw.replace("%GameFilterTCP%", &tcp);
    raw = raw.replace("%GameFilterUDP%", &udp);

    if let Some(var) = leftover_var(&raw) {
        return Err(format!("В пресете {name} остались неразобранные переменные: {var}"));
    }
    Ok(split_args(&raw))
}

pub fn join_args(args: &[String]) -> String {
    let mut parts = Vec::new();
    for a in args {
        if a.chars().any(|c| c.is_whitespace() || c == '"') {
            parts.push(format!("\"{}\"", a.replace('"', "\\\"")));
        } else {
            parts.push(a.clone());
        }
    }
    parts.join(" ")
}
