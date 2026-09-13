//! 运行记录（JSONL）：每次规划一行，便于复盘与实验统计。
//!
//! - `PlanRecord::from_plan`：从 Plan IR 提取关键指标
//! - `append_jsonl` / `read_jsonl`：追加与读取（保留最后 N 条）
//!
//! 时间戳用自实现 UTC ISO8601（不引入 chrono 依赖）。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::plan::Plan;

/// 一次规划运行的记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRecord {
    /// UTC ISO8601（如 2026-09-14T03:21:00Z）
    pub ts: String,
    pub ts_unix: u64,
    pub target: String,
    pub rate_per_min: f64,
    pub mode: String,
    pub objective: Option<String>,
    pub max_tier: Option<u8>,
    pub elapsed_ms: f64,
    pub recipes: usize,
    pub ops_per_min: f64,
    pub cost: f64,
    pub machines: f64,
    pub machines_int: u64,
    pub net_eu_t: f64,
    pub objective_score: f64,
    pub raw_items_per_min: f64,
    pub raw_fluids_mb_per_min: f64,
    pub notes: Vec<String>,
}

impl PlanRecord {
    /// 从 Plan IR 提取记录。
    pub fn from_plan(plan: &Plan, objective: Option<&str>, max_tier: Option<u8>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        Self {
            ts: iso8601_utc(secs),
            ts_unix: secs,
            target: plan.target.id.clone(),
            rate_per_min: plan.rate_per_min,
            mode: plan.mode.clone(),
            objective: objective.map(str::to_string),
            max_tier,
            elapsed_ms: plan.elapsed_ms,
            recipes: plan.totals.distinct_recipes,
            ops_per_min: plan.totals.recipe_ops_per_min,
            cost: plan.totals.estimated_cost,
            machines: plan.totals.total_machines,
            machines_int: plan.totals.total_machines_int,
            net_eu_t: plan.totals.net_eu_t,
            objective_score: plan.totals.objective_score,
            raw_items_per_min: plan.totals.raw_items_per_min,
            raw_fluids_mb_per_min: plan.totals.raw_fluids_mb_per_min,
            notes: plan.notes.clone(),
        }
    }

    /// 追加一行到 JSONL 文件（自动创建目录）。
    pub fn append_jsonl(path: &Path, rec: &PlanRecord) -> Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{}", serde_json::to_string(rec)?)?;
        Ok(())
    }

    /// 读取最后 `limit` 条记录（文件不存在返回空）。
    pub fn read_jsonl(path: &Path, limit: usize) -> Result<Vec<PlanRecord>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let reader = BufReader::new(File::open(path)?);
        let mut out: Vec<PlanRecord> = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(r) = serde_json::from_str::<PlanRecord>(&line) {
                out.push(r);
            }
        }
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }
}

/// Unix 秒 → UTC ISO8601（civil-from-days 算法，Howard Hinnant）。
pub fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_epoch() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
