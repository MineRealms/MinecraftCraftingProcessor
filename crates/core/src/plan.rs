//! Plan IR：规划结果（可直接序列化给 CLI / Web API / 前端）。

use serde::Serialize;

use crate::model::MaterialDto;

/// 完整生产计划。
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub target: MaterialDto,
    /// 目标速率（物品=个/分，流体=mB/分）。
    pub rate_per_min: f64,
    pub mode: String,
    pub recipes: Vec<PlannedRecipe>,
    pub raw_materials: Vec<PlanEntry>,
    pub byproducts: Vec<PlanEntry>,
    pub totals: PlanTotals,
    pub notes: Vec<String>,
    pub elapsed_ms: f64,
}

/// 计划中的单个配方步骤。
#[derive(Debug, Clone, Serialize)]
pub struct PlannedRecipe {
    /// 完整配方名：`<分类 type>/<配方 id>`。
    pub recipe: String,
    pub category: String,
    pub category_title: String,
    /// 每分钟执行次数。
    pub ops_per_min: f64,
    /// 机器数量 = ops/min × duration_ticks / 1200（需 GT 时长数据）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_count: Option<f64>,
    /// MILP-lite：整数机器数（向上取整）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_count_int: Option<u64>,
    /// 单机功率 EU/t（eut × amperage；发电配方为发电功率）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eut: Option<f64>,
    /// 电压等级（LV/MV/…）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// 该步骤每分钟耗电（EU/min；发电为负向贡献前的正值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eu_per_min: Option<f64>,
    /// 实际选中的输入候选与速率。
    pub inputs: Vec<PlanEntry>,
    /// 全部主输出与速率（含副产物）。
    pub outputs: Vec<PlanEntry>,
}

/// 计划条目：材料 + 速率（个/分 或 mB/分）。
#[derive(Debug, Clone, Serialize)]
pub struct PlanEntry {
    pub material: MaterialDto,
    pub rate_per_min: f64,
}

/// 计划汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanTotals {
    pub distinct_recipes: usize,
    pub recipe_ops_per_min: f64,
    pub raw_items_per_min: f64,
    pub raw_fluids_mb_per_min: f64,
    /// 估算成本（原始物品当量；流体按桶计）。
    pub estimated_cost: f64,
    /// 机器总数（Σ machine_count，含所有步骤）。
    pub total_machines: f64,
    /// MILP-lite：整数机器总数（Σ ceil）。
    pub total_machines_int: u64,
    /// 净功率 EU/t（耗电 − 发电）。
    pub net_eu_t: f64,
    /// 总耗电 EU/t（不含发电）。
    pub consume_eu_t: f64,
    /// 总发电 EU/t。
    pub generate_eu_t: f64,
    /// 每分钟净能量（EU/min，耗电 − 发电）。
    pub net_eu_per_min: f64,
    /// 加权目标分：w_m·原料 + w_eu·EU/min + w_machine·机器数。
    pub objective_score: f64,
}
