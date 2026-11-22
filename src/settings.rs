use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(rename = "Preset", default = "default_preset")]
    pub preset: String,
    #[serde(rename = "GameMode", default = "default_game_mode")]
    pub game_mode: String,
    #[serde(rename = "GameTcpRange", default = "default_range")]
    pub game_tcp_range: String,
    #[serde(rename = "GameUdpRange", default = "default_range")]
    pub game_udp_range: String,
    #[serde(rename = "AutoStart", default = "default_true")]
    pub auto_start: bool,
    #[serde(rename = "LastAutoPreset", default)]
    pub last_auto_preset: String,
    #[serde(rename = "LastAutoScore", default)]
    pub last_auto_score: i32,
    #[serde(rename = "LastAutoDate", default)]
    pub last_auto_date: String,
    #[serde(rename = "TgPort", default = "default_tg_port")]
    pub tg_port: u16,
    #[serde(rename = "TgSecret", default)]
    pub tg_secret: String,
    #[serde(rename = "TgAutoStart", default)]
    pub tg_auto_start: bool,
}

fn default_preset() -> String {
    "general.bat".into()
}
fn default_game_mode() -> String {
    "off".into()
}
fn default_range() -> String {
    "1024-65535".into()
}
fn default_true() -> bool {
    true
}
fn default_tg_port() -> u16 {
    1443
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            preset: default_preset(),
            game_mode: default_game_mode(),
            game_tcp_range: default_range(),
            game_udp_range: default_range(),
            auto_start: true,
            last_auto_preset: String::new(),
            last_auto_score: 0,
            last_auto_date: String::new(),
            tg_port: default_tg_port(),
            tg_secret: String::new(),
            tg_auto_start: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        let mut s = Settings::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            match serde_json::from_str::<Settings>(&text) {
                Ok(parsed) => s = parsed,
                Err(_) => {
                    // Мягко переносим настройки исходной PowerShell-версии.
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(x) = v.get("Preset").and_then(|x| x.as_str()) {
                            s.preset = x.to_string();
                        }
                        if let Some(x) = v.get("GameMode").and_then(|x| x.as_str()) {
                            s.game_mode = x.to_string();
                        }
                        if let Some(x) = v.get("GameTcpRange").and_then(|x| x.as_str()) {
                            s.game_tcp_range = x.to_string();
                        }
                        if let Some(x) = v.get("GameUdpRange").and_then(|x| x.as_str()) {
                            s.game_udp_range = x.to_string();
                        }
                        if let Some(x) = v.get("AutoStart").and_then(|x| x.as_bool()) {
                            s.auto_start = x;
                        }
                        if let Some(x) = v.get("LastAutoPreset").and_then(|x| x.as_str()) {
                            s.last_auto_preset = x.to_string();
                        }
                        if let Some(x) = v.get("LastAutoScore").and_then(|x| x.as_i64()) {
                            s.last_auto_score = x as i32;
                        }
                        if let Some(x) = v.get("LastAutoDate").and_then(|x| x.as_str()) {
                            s.last_auto_date = x.to_string();
                        }
                        if let Some(x) = v.get("TgPort").and_then(|x| x.as_u64()) {
                            s.tg_port = x as u16;
                        }
                        if let Some(x) = v.get("TgSecret").and_then(|x| x.as_str()) {
                            s.tg_secret = x.to_string();
                        }
                        if let Some(x) = v.get("TgAutoStart").and_then(|x| x.as_bool()) {
                            s.tg_auto_start = x;
                        }
                    }
                }
            }
        }
        if !matches!(s.game_mode.as_str(), "off" | "udp" | "tcp" | "all") {
            s.game_mode = "off".into();
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn game_values(&self) -> (String, String) {
        let mut tcp = "12".to_string();
        let mut udp = "12".to_string();
        match self.game_mode.as_str() {
            "tcp" => tcp = self.game_tcp_range.clone(),
            "udp" => udp = self.game_udp_range.clone(),
            "all" => {
                tcp = self.game_tcp_range.clone();
                udp = self.game_udp_range.clone();
            }
            _ => {}
        }
        (tcp, udp)
    }

    pub fn game_label(&self) -> &'static str {
        match self.game_mode.as_str() {
            "udp" => "игры: UDP",
            "tcp" => "игры: TCP",
            "all" => "игры: TCP + UDP",
            _ => "игры: выкл",
        }
    }
}
