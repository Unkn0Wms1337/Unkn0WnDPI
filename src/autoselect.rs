use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

use crate::logging::Logger;
use crate::netcheck::{self, TestReport, MAX_SCORE, TARGETS};
use crate::paths::Paths;
use crate::presets;
use crate::settings::Settings;
use crate::winproc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoRow {
    pub preset: String,
    pub label: String,
    pub core_score: u32,
    pub core_max: u32,
    pub core_ms: u128,
    pub full_score: u32,
    pub full_max: u32,
    pub full_ms: u128,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoResult {
    pub date: String,
    pub best: String,
    pub best_label: String,
    pub best_score: u32,
    pub max_score: u32,
    pub baseline_score: u32,
    pub selected: bool,
    pub presets: Vec<AutoRow>,
}

#[derive(Clone, Debug)]
pub enum AutoEvent {
    Progress {
        stage: &'static str,
        index: usize,
        total: usize,
        preset: String,
    },
    Row(AutoRow),
    Done(Result<AutoResult, String>),
}

fn send(tx: &Sender<AutoEvent>, ev: AutoEvent) {
    let _ = tx.send(ev);
}

pub fn run(
    paths: &Paths,
    mut settings: Settings,
    log: Logger,
    tx: Sender<AutoEvent>,
    had_service: bool,
) {
    let result = run_inner(paths, &mut settings, &log, &tx, had_service);
    let _ = &settings;
    send(&tx, AutoEvent::Done(result));
}

fn run_inner(
    paths: &Paths,
    settings: &mut Settings,
    log: &Logger,
    tx: &Sender<AutoEvent>,
    had_service: bool,
) -> Result<AutoResult, String> {
    if !winproc::is_admin() {
        return Err("Для авто-подбора нужны права администратора.".into());
    }
    if had_service {
        winproc::remove_service(log)?;
    }
    let list = presets::list();
    if list.is_empty() {
        return Err("Не найдены встроенные пресеты.".into());
    }
    winproc::stop_bypass(log);

    log.info("Авто-подбор: проверяю текущее состояние без обхода...");
    send(
        tx,
        AutoEvent::Progress {
            stage: "baseline",
            index: 0,
            total: list.len(),
            preset: "без обхода".into(),
        },
    );
    let baseline = netcheck::run_targets(TARGETS);
    log.info(format!("Без обхода: {}/{}", baseline.score, baseline.max));
    let mut rows: Vec<AutoRow> = Vec::new();

    for (i, preset) in list.iter().enumerate() {
        send(
            tx,
            AutoEvent::Progress {
                stage: "testing",
                index: i + 1,
                total: list.len(),
                preset: preset.label.clone(),
            },
        );
        let mut row = AutoRow {
            preset: preset.file.clone(),
            label: preset.label.clone(),
            core_score: 0,
            core_max: 7,
            core_ms: 0,
            full_score: 0,
            full_max: MAX_SCORE,
            full_ms: 0,
            error: String::new(),
        };
        match winproc::start_bypass(paths, settings, log) {
            Ok(_) => {
                let r = netcheck::run_targets(&netcheck::core_targets());
                row.core_score = r.score;
                row.core_ms = r.elapsed_ms;
                if !r.results.iter().any(|x| x.ok) {
                    row.error = "нет связи".into();
                }
            }
            Err(e) => row.error = e,
        }
        winproc::stop_bypass(log);
        if row.error.is_empty() {
            log.info(format!(
                "  {:<26} {}/7  {} мс",
                row.label, row.core_score, row.core_ms
            ));
        } else {
            log.warn(format!("  {} — ошибка: {}", row.label, row.error));
        }
        rows.push(row.clone());
        send(tx, AutoEvent::Row(row));
    }

    let mut candidates: Vec<AutoRow> = rows
        .iter()
        .filter(|r| r.core_score >= 5 && r.error.is_empty())
        .cloned()
        .collect();
    candidates.sort_by(|a, b| {
        b.core_score
            .cmp(&a.core_score)
            .then(a.core_ms.cmp(&b.core_ms))
    });
    candidates.truncate(4);
    if candidates.is_empty() {
        let mut all = rows.clone();
        all.sort_by(|a, b| {
            b.core_score
                .cmp(&a.core_score)
                .then(a.core_ms.cmp(&b.core_ms))
        });
        all.truncate(3);
        candidates = all;
        log.warn("Ни один пресет не прошёл базовую проверку полностью — проверяю лучшие частично.");
    }

    let cand_total = candidates.len();
    for (i, cand) in candidates.iter_mut().enumerate() {
        send(
            tx,
            AutoEvent::Progress {
                stage: "full",
                index: i + 1,
                total: cand_total,
                preset: cand.label.clone(),
            },
        );
        match winproc::start_bypass(paths, settings, log) {
            Ok(_) => {
                let r = netcheck::run_targets(TARGETS);
                cand.full_score = r.score;
                cand.full_ms = r.elapsed_ms;
            }
            Err(e) => cand.error = e,
        }
        winproc::stop_bypass(log);
        log.info(format!(
            "  финал {:<22} {}/{}  {} мс",
            cand.label, cand.full_score, MAX_SCORE, cand.full_ms
        ));
    }

    candidates.sort_by(|a, b| {
        b.full_score
            .cmp(&a.full_score)
            .then(b.core_score.cmp(&a.core_score))
            .then(a.full_ms.cmp(&b.full_ms))
    });
    let best = candidates.first().cloned();

    let mut result = AutoResult {
        date: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        best: best.as_ref().map(|b| b.preset.clone()).unwrap_or_default(),
        best_label: best.as_ref().map(|b| b.label.clone()).unwrap_or_default(),
        best_score: best.as_ref().map(|b| b.full_score).unwrap_or(0),
        max_score: MAX_SCORE,
        baseline_score: baseline.score,
        selected: false,
        presets: rows,
    };

    if let Ok(json) = serde_json::to_string_pretty(&result) {
        let _ = std::fs::write(&paths.auto_result, json);
    }

    if let Some(best) = &best {
        if best.full_score >= 5 {
            settings.preset = best.preset.clone();
            settings.last_auto_preset = best.preset.clone();
            settings.last_auto_score = best.full_score as i32;
            settings.last_auto_date = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            settings.save(&paths.settings_file)?;
            if had_service {
                winproc::install_service(paths, settings, log)?;
            } else {
                winproc::start_bypass(paths, settings, log)?;
            }
            result.selected = true;
            log.ok(format!(
                "Авто-подбор завершён: {} ({}/{}). Без обхода было {}/{}.",
                best.label, best.full_score, MAX_SCORE, baseline.score, MAX_SCORE
            ));
        } else {
            log.error("Авто-подбор не нашёл рабочую стратегию. Проверьте интернет, VPN/антивирус и диагностику.");
        }
    } else {
        log.error("Авто-подбор не нашёл рабочую стратегию.");
    }

    send(
        tx,
        AutoEvent::Progress {
            stage: "done",
            index: list.len(),
            total: list.len(),
            preset: result.best_label.clone(),
        },
    );
    let _ = &baseline;
    Ok(result)
}

pub fn load_last(paths: &Paths) -> Option<AutoResult> {
    let text = std::fs::read_to_string(&paths.auto_result).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_report(paths: &Paths, report: &TestReport) {
    if let Ok(json) = serde_json::to_string_pretty(report) {
        let _ = std::fs::write(&paths.test_result, json);
    }
}
