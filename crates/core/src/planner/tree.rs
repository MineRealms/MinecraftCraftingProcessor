//! 确定性展开（tree mode）：工作队列 + 环内线性缩放。
//!
//! 算法：
//! 1. 需求队列按材料展开，每种材料用启发式最优配方；
//! 2. 环内材料复用已选配方，按需求量线性放大（迭代收敛）；
//! 3. 其他输出按"抵原料 → 抵需求 → 记副产物"顺序处理；
//! 4. 输入槽取单位成本最便宜的候选。

use std::collections::VecDeque;
use std::time::Instant;

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeId};
use crate::plan::{Plan, PlanEntry, PlanTotals, PlannedRecipe};
use crate::util::{cheapest_alt, mat_norm_qty, output_qty_of};

const EPS: f64 = 1e-9;

/// 规划请求。
#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub target: MaterialId,
    /// 目标速率（物品=个/分，流体=mB/分）。
    pub rate_per_min: f64,
    /// 展开操作上限（防循环失控）。
    pub max_ops: usize,
}

impl PlanRequest {
    pub fn new(target: MaterialId, rate_per_min: f64) -> Self {
        Self {
            target,
            rate_per_min,
            max_ops: 200_000,
        }
    }
}

fn add_demand(
    m: MaterialId,
    qty: f64,
    demand: &mut [f64],
    queued: &mut [bool],
    queue: &mut VecDeque<MaterialId>,
) {
    if !qty.is_finite() || qty <= EPS {
        return;
    }
    demand[m as usize] += qty;
    if !queued[m as usize] {
        queued[m as usize] = true;
        queue.push_back(m);
    }
}

/// 速率归一（流体 mB → 桶）。
fn norm_rate(kind: MaterialKind, rate: f64) -> f64 {
    match kind {
        MaterialKind::Fluid => rate / 1000.0,
        _ => rate,
    }
}

/// 记录备注（去重）。
fn note(notes: &mut Vec<String>, msg: String) {
    if !notes.contains(&msg) {
        notes.push(msg);
    }
}

/// 确定性展开规划。
pub fn plan_tree(g: &KnowledgeGraph, an: &Analysis, req: &PlanRequest) -> Plan {
    let t0 = Instant::now();
    let n = g.materials.len();
    let mut notes: Vec<String> = Vec::new();

    let mut demand: Vec<f64> = vec![0.0; n];
    let mut raw: Vec<f64> = vec![0.0; n];
    let mut byproduct: Vec<f64> = vec![0.0; n];
    let mut chosen: Vec<Option<RecipeId>> = vec![None; n];
    let mut ops: Vec<f64> = vec![0.0; g.recipes.len()];
    let mut queued = vec![false; n];
    let mut queue: VecDeque<MaterialId> = VecDeque::new();
    // 每材料展开次数（循环保险：超限按外部输入处理）
    let mut visits: Vec<u16> = vec![0; n];
    let mut visit_limit_hit = false;

    let mut expansions = 0usize;
    let mut truncated = false;

    add_demand(
        req.target,
        req.rate_per_min,
        &mut demand,
        &mut queued,
        &mut queue,
    );

    while let Some(m) = queue.pop_front() {
        queued[m as usize] = false;
        let qty = demand[m as usize];
        demand[m as usize] = 0.0;
        if qty <= EPS {
            continue;
        }
        // 发散保护：需求失控时中止
        if !qty.is_finite() || qty > 1e15 {
            truncated = true;
            note(&mut notes, format!(
                "需求发散（{} 达到 {:e}），已中止展开",
                g.material_id_str(m),
                qty
            ));
            break;
        }
        // 循环保险：同一材料展开次数超限后按外部输入处理
        visits[m as usize] += 1;
        if visits[m as usize] > 16 {
            visit_limit_hit = true;
            raw[m as usize] += qty;
            continue;
        }

        // 原料（无生产者）：直接记账
        if g.is_raw(m) {
            raw[m as usize] += qty;
            continue;
        }

        // 选定配方（首次决定后固定，环内复用）。
        // 无可达生产路线（可采集资源 / 纯循环不可达）→ 按外部输入处理。
        let rid = match chosen[m as usize] {
            Some(r) => r,
            None => match an.cost.best_recipe[m as usize] {
                Some(r) => {
                    chosen[m as usize] = Some(r);
                    r
                }
                None => {
                    if !g.harvestable[m as usize] {
                        note(&mut notes, format!(
                            "{} 无可达生产路线，按外部输入处理",
                            g.material_id_str(m)
                        ));
                    }
                    raw[m as usize] += qty;
                    continue;
                }
            },
        };
        let recipe = g.recipe(rid);
        let Some(out_q) = output_qty_of(recipe, m) else {
            note(&mut notes, format!(
                "配方 {} 不产出目标材料，已按原料处理",
                g.recipe_full_id(rid)
            ));
            raw[m as usize] += qty;
            continue;
        };

        expansions += 1;
        if expansions > req.max_ops {
            truncated = true;
            note(&mut notes, format!("展开次数达到上限 {}，计划可能不完整", req.max_ops));
            break;
        }

        let ops_add = qty / mat_norm_qty(g, m, out_q);
        ops[rid as usize] += ops_add;

        // 其他输出：抵原料 → 抵需求 → 记副产物
        for slot in &recipe.outputs {
            let Some((om, oq)) = slot.primary() else {
                continue;
            };
            if om == m {
                continue;
            }
            let mut left = ops_add * mat_norm_qty(g, om, oq);
            if left <= EPS {
                continue;
            }
            if raw[om as usize] > EPS {
                let used = left.min(raw[om as usize]);
                raw[om as usize] -= used;
                left -= used;
            }
            if left > EPS && demand[om as usize] > EPS {
                let used = left.min(demand[om as usize]);
                demand[om as usize] -= used;
                left -= used;
            }
            if left > EPS {
                byproduct[om as usize] += left;
            }
        }

        // 输入需求
        for slot in &recipe.inputs {
            let Some((am, aq)) = cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot)
            else {
                continue;
            };
            let need = ops_add * mat_norm_qty(g, am, aq);
            add_demand(am, need, &mut demand, &mut queued, &mut queue);
        }
    }

    if visit_limit_hit {
        note(&mut notes, "部分材料循环展开次数超限，已按外部输入处理".to_string());
    }

    if !an.cost.converged {
        note(&mut notes, "成本迭代未完全收敛（存在复杂循环），估算值可能偏差".to_string());
    }
    if truncated {
        note(&mut notes, "结果不完整：存在未满足需求或循环放大".to_string());
    }
    note(&mut notes, "JEI 数据不含配方时长与耗电：机器数量与 EU 消耗未计算".to_string());

    // ---------------- 组装 Plan IR ----------------

    let mut planned: Vec<PlannedRecipe> = Vec::new();
    for (rid, &op) in ops.iter().enumerate() {
        if op <= EPS {
            continue;
        }
        let r = g.recipe(rid as RecipeId);
        let cat = g.category(r.category);
        let mut inputs = Vec::new();
        for slot in &r.inputs {
            let Some((am, aq)) = cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot)
            else {
                continue;
            };
            inputs.push(PlanEntry {
                material: g.material_dto(am),
                rate_per_min: op * mat_norm_qty(g, am, aq),
            });
        }
        let mut outputs = Vec::new();
        for slot in &r.outputs {
            let Some((om, oq)) = slot.primary() else {
                continue;
            };
            outputs.push(PlanEntry {
                material: g.material_dto(om),
                rate_per_min: op * mat_norm_qty(g, om, oq),
            });
        }
        planned.push(PlannedRecipe {
            recipe: g.recipe_full_id(rid as RecipeId),
            category: cat.ty.clone(),
            category_title: cat.title.clone(),
            ops_per_min: op,
            machine_count: None,
            inputs,
            outputs,
        });
    }
    planned.sort_by(|a, b| {
        b.ops_per_min
            .total_cmp(&a.ops_per_min)
            .then_with(|| a.recipe.cmp(&b.recipe))
    });

    let mut raw_entries: Vec<PlanEntry> = Vec::new();
    let mut byproduct_entries: Vec<PlanEntry> = Vec::new();
    let mut estimated_cost = 0.0f64;
    let mut raw_items = 0.0f64;
    let mut raw_fluids = 0.0f64;
    for m in 0..n {
        if raw[m] > EPS {
            let info = g.material(m as MaterialId);
            estimated_cost += norm_rate(info.key.kind, raw[m]);
            if info.key.kind.is_fluid() {
                raw_fluids += raw[m];
            } else {
                raw_items += raw[m];
            }
            raw_entries.push(PlanEntry {
                material: g.material_dto(m as MaterialId),
                rate_per_min: raw[m],
            });
        }
        if byproduct[m] > EPS {
            byproduct_entries.push(PlanEntry {
                material: g.material_dto(m as MaterialId),
                rate_per_min: byproduct[m],
            });
        }
    }
    raw_entries.sort_by(|a, b| {
        b.rate_per_min
            .total_cmp(&a.rate_per_min)
            .then_with(|| a.material.id.cmp(&b.material.id))
    });
    byproduct_entries.sort_by(|a, b| {
        b.rate_per_min
            .total_cmp(&a.rate_per_min)
            .then_with(|| a.material.id.cmp(&b.material.id))
    });

    let totals = PlanTotals {
        distinct_recipes: planned.len(),
        recipe_ops_per_min: planned.iter().map(|p| p.ops_per_min).sum(),
        raw_items_per_min: raw_items,
        raw_fluids_mb_per_min: raw_fluids,
        estimated_cost,
    };

    Plan {
        target: g.material_dto(req.target),
        rate_per_min: req.rate_per_min,
        mode: "tree".to_string(),
        recipes: planned,
        raw_materials: raw_entries,
        byproducts: byproduct_entries,
        totals,
        notes,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
    }
}
