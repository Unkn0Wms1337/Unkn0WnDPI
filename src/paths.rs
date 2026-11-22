use std::path::PathBuf;

/// Каталоги программы. Всё хранится в %LOCALAPPDATA%\Unkn0WnDPI,
/// поэтому exe можно запускать откуда угодно (в т.ч. с флешки).
#[derive(Clone, Debug)]
pub struct Paths {
    pub exe: PathBuf,
    pub root: PathBuf,
    pub engine: PathBuf,
    pub bin: PathBuf,
    pub lists: PathBuf,
    pub utils: PathBuf,
    pub config: PathBuf,
    pub logs: PathBuf,
    pub winws: PathBuf,
    pub settings_file: PathBuf,
    pub log_file: PathBuf,
    pub auto_status: PathBuf,
    pub auto_result: PathBuf,
    pub test_result: PathBuf,
    pub diag_file: PathBuf,
}

pub const APP_NAME: &str = "Unkn0WnDPI";

fn is_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".unkn0wn-write-test");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// %LOCALAPPDATA%\Unkn0WnDPI (AppData\Local\Unkn0WnDPI)
fn default_root() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|_| {
            std::env::var("USERPROFILE")
                .map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());

    base.join(APP_NAME)
}

impl Paths {
    pub fn init() -> Paths {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from(format!("{APP_NAME}.exe")));
        let exe_dir = exe
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        // UNKN0WN_DPI_HOME — только для тестов и переносимых запусков.
        let root = if let Ok(home) = std::env::var("UNKN0WN_DPI_HOME") {
            if !home.trim().is_empty() {
                PathBuf::from(home)
            } else {
                default_root()
            }
        } else {
            let dir = default_root();
            if is_writable(&dir) {
                dir
            } else {
                // Крайний случай: LOCALAPPDATA недоступен — пишем рядом с exe.
                exe_dir
            }
        };

        let _ = std::fs::create_dir_all(&root);
        let engine = root.join("engine");
        let bin = engine.join("bin");
        let lists = engine.join("lists");
        let utils = engine.join("utils");
        let config = root.join("config");
        let logs = root.join("logs");
        for d in [&engine, &bin, &lists, &utils, &config, &logs] {
            let _ = std::fs::create_dir_all(d);
        }

        Paths {
            winws: bin.join("winws.exe"),
            settings_file: config.join("settings.json"),
            log_file: logs.join("Unkn0WnDPI.log"),
            auto_status: logs.join("autoselect-status.json"),
            auto_result: logs.join("autoselect-result.json"),
            test_result: logs.join("test-result.json"),
            diag_file: logs.join("diag-latest.txt"),
            exe,
            root,
            engine,
            bin,
            lists,
            utils,
            config,
            logs,
        }
    }

    pub fn game_filter_file(&self) -> PathBuf {
        self.utils.join("game_filter.enabled")
    }
}
