use std::fs;
use std::path::Path;

use crate::paths::Paths;

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

pub const ENGINE_VERSION: &str = "1.10.4";
pub const APP_VERSION: &str = "2.1.0";

/// Распаковывает встроенный движок. Бинарники обновляются при смене сборки,
/// пользовательские списки никогда не перезаписываются.
pub fn ensure_engine(paths: &Paths) -> Result<usize, String> {
    let stamp_file = paths.engine.join(".build-stamp");
    let current = fs::read_to_string(&stamp_file).unwrap_or_default();
    let fresh = current.trim() == ENGINE_STAMP && paths.winws.exists();

    let mut written = 0usize;
    for (rel, bytes) in ENGINE_FILES {
        let dest = paths.engine.join(rel);
        let preserve = rel.starts_with("lists/") || rel.starts_with("utils/");
        let exists = dest.exists();
        if exists && (fresh || preserve) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(&dest, bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        written += 1;
    }

    if !fresh {
        fs::write(&stamp_file, ENGINE_STAMP).map_err(|e| e.to_string())?;
    }
    Ok(written)
}

/// Необязательные пользовательские списки создаются пустыми, если их нет.
pub fn ensure_user_lists(paths: &Paths) {
    let files = [
        ("list-general-user.txt", ""),
        (
            "list-exclude-user.txt",
            "# Домены-исключения (по одному в строке)\n",
        ),
        (
            "ipset-exclude-user.txt",
            "# Подсети-исключения (CIDR, по одной в строке)\n",
        ),
    ];
    for (name, content) in files {
        let path = paths.lists.join(name);
        if !path.exists() {
            let _ = fs::write(&path, content);
        }
    }
}

pub fn read_external_preset(name: &str) -> Option<Vec<u8>> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return None;
    }
    let dir = std::env::var("UNKN0WN_DPI_PRESETS").ok()?;
    fs::read(Path::new(&dir).join(name)).ok()
}
