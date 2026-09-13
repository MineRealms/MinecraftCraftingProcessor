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
    /// 机器数量（需配方时长覆盖层；缺失时为 null）。
    pub machine_count: Option<f64>,
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
}
