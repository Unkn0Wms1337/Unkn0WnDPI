use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
    Ok,
}

impl Level {
    pub fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
            Level::Ok => "OK",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub time: String,
    pub level: Level,
    pub text: String,
}

#[derive(Clone)]
pub struct Logger {
    lines: Arc<Mutex<Vec<LogLine>>>,
    file: Option<Arc<std::path::PathBuf>>,
    quiet: bool,
}

impl Logger {
    pub fn new(file: &Path, quiet: bool) -> Logger {
        // Ротация: не даём журналу расти бесконечно.
        if let Ok(meta) = std::fs::metadata(file) {
            if meta.len() > 2 * 1024 * 1024 {
                let old = file.with_extension("log.old");
                let _ = std::fs::rename(file, old);
            }
        }
        Logger {
            lines: Arc::new(Mutex::new(Vec::new())),
            file: Some(Arc::new(file.to_path_buf())),
            quiet,
        }
    }

    pub fn log(&self, level: Level, text: impl AsRef<str>) {
        let text = text.as_ref().to_string();
        let line = LogLine {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            text: text.clone(),
        };
        if let Ok(mut guard) = self.lines.lock() {
            guard.push(line.clone());
            if guard.len() > 1200 {
                let drain = guard.len() - 1200;
                guard.drain(0..drain);
            }
        }
        if !self.quiet {
            println!("{} [{}] {}", line.time, level.tag(), text);
        }
        if let Some(path) = &self.file {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path.as_ref()) {
                let _ = writeln!(f, "{} [{}] {}", line.time, level.tag(), text);
            }
        }
    }

    pub fn info(&self, text: impl AsRef<str>) {
        self.log(Level::Info, text);
    }
    pub fn ok(&self, text: impl AsRef<str>) {
        self.log(Level::Ok, text);
    }
    pub fn warn(&self, text: impl AsRef<str>) {
        self.log(Level::Warn, text);
    }
    pub fn error(&self, text: impl AsRef<str>) {
        self.log(Level::Error, text);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lines.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let guard = self.lines.lock().unwrap_or_else(|p| p.into_inner());
        let start = guard.len().saturating_sub(n);
        guard[start..].to_vec()
    }

    pub fn clear_view(&self) {
        if let Ok(mut guard) = self.lines.lock() {
            guard.clear();
        }
    }
}
