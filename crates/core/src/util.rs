//! 图操作辅助函数（数量归一 / 槽候选选择 / 成本计算）。
//!
//! 所有成本比较都使用**相对容差**：浮点噪声（如循环路线的 `9 * (c/9)`）
//! 必须视为并列，否则规划器会以 1e-17 的优势选中净守恒循环，导致永不收敛。
//! 并列时的裁决规则：优先非循环（跨 SCC）候选。

use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeNode, Slot};

/// 数量归一：物品 = 个，流体 = 桶（1000 mB），其他 = 个。
pub fn norm_qty(kind: MaterialKind, qty: u64) -> f64 {
    match kind {
        MaterialKind::Fluid => qty as f64 / 1000.0,
        _ => qty as f64,
    }
}

pub fn mat_norm_qty(g: &KnowledgeGraph, m: MaterialId, qty: u64) -> f64 {
    norm_qty(g.material(m).key.kind, qty)
}

/// 相对容差：`|a-b| <= tol` 视为并列。
fn tie_tol(a: f64, b: f64) -> f64 {
    1e-9 * (1.0 + a.abs().max(b.abs()))
}

/// 槽内选最便宜候选：
/// - 成本差异超过相对容差时取更便宜者；
/// - 并列时优先非循环材料（`cyclic` 提供时）；
/// - 仍并列取先出现的候选。
pub fn cheapest_alt(
    g: &KnowledgeGraph,
    unit_cost: &[f64],
    cyclic: Option<&[bool]>,
    slot: &Slot,
) -> Option<(MaterialId, u64)> {
    let mut best: Option<(MaterialId, u64, f64)> = None;
    for &(m, q) in &slot.alts {
        let c = unit_cost[m as usize] * mat_norm_qty(g, m, q);
        match best {
            None => best = Some((m, q, c)),
            Some((bm, _, bc)) => {
                let better = if (c - bc).abs() <= tie_tol(c, bc) {
                    match cyclic {
                        Some(cy) => cy[m as usize] && !cy[bm as usize],
                        None => false,
                    }
                } else {
                    c < bc
                };
                if better {
                    best = Some((m, q, c));
                }
            }
        }
    }
    best.map(|(m, q, _)| (m, q))
}

/// 配方总成本：Σ 每个输入槽的最便宜候选（单位成本 × 归一数量）。
pub fn recipe_cost(
    g: &KnowledgeGraph,
    unit_cost: &[f64],
    cyclic: Option<&[bool]>,
    r: &RecipeNode,
) -> f64 {
    let mut total = 0.0;
    for slot in &r.inputs {
        match cheapest_alt(g, unit_cost, cyclic, slot) {
            Some((m, q)) => {
                let c = unit_cost[m as usize];
                if !c.is_finite() {
                    return f64::INFINITY;
                }
                total += c * mat_norm_qty(g, m, q);
            }
            None => return f64::INFINITY,
        }
    }
    total
}

/// 配方产出材料 m 的数量（第一个含 m 的输出槽）。
pub fn output_qty_of(r: &RecipeNode, m: MaterialId) -> Option<u64> {
    for slot in &r.outputs {
        for &(mm, q) in &slot.alts {
            if mm == m {
                return Some(q);
            }
        }
    }
    None
}

/// 配方的产出单位成本列表：[(材料, 每单位成本)]。
///
/// 成本分摊规则（关键设计）：
/// - **主输出**（第一个输出槽）承担全部配方成本 —— 保证成本沿生产链守恒，
///   否则循环（离心 ↔ 化合）会因逐级"除以份额"把成本收缩到接近 0；
/// - 副产物均摊 `recipe_cost / 输出槽数`，只作为上界估计。
pub fn recipe_output_unit_costs(
    g: &KnowledgeGraph,
    r: &RecipeNode,
    recipe_cost: f64,
) -> Vec<(MaterialId, f64)> {
    if !recipe_cost.is_finite() || r.outputs.is_empty() {
        return Vec::new();
    }
    let n_slots = r.outputs.len() as f64;
    let byproduct_cost = recipe_cost / n_slots;
    let mut out = Vec::new();
    for (i, slot) in r.outputs.iter().enumerate() {
        let slot_cost = if i == 0 { recipe_cost } else { byproduct_cost };
        for &(m, q) in &slot.alts {
            let nq = mat_norm_qty(g, m, q);
            if nq > 0.0 {
                out.push((m, slot_cost / nq));
            }
        }
    }
    out
}

/// 配方深度：1 + 各输入槽最小深度的最大值（INF 感知）。
pub fn recipe_depth(depth: &[u32], r: &RecipeNode) -> u32 {
    let mut max_in: u32 = 0;
    for slot in &r.inputs {
        let mut min_d = u32::MAX;
        for &(m, _) in &slot.alts {
            min_d = min_d.min(depth[m as usize]);
        }
        if min_d == u32::MAX {
            return u32::MAX;
        }
        max_in = max_in.max(min_d);
    }
    max_in.saturating_add(1)
}

/// 成本是否可视为"严格更小"（超过相对容差）。
pub fn cost_strictly_less(a: f64, b: f64) -> bool {
    if !a.is_finite() {
        return false;
    }
    if !b.is_finite() {
        // b = INF（或 NaN）时任何有限成本都严格更小
        return true;
    }
    a < b - tie_tol(a, b)
}
