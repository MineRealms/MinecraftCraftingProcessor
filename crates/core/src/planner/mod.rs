//! 规划器层：确定性展开（tree）与 Beam Search（beam）。
//!
//! 两种模式共享 Plan IR 组装（`assemble_plan`）。

pub mod beam;
pub mod mcts;
pub mod tree;

pub use beam::{
    plan_beam, plan_beam_with_evaluator, recipe_alternatives, BatchEvaluator, BatchStats,
    BeamOptions,
};
pub use mcts::{plan_mcts, MctsOptions};
pub use tree::{greedy_choices, plan_tree, PlanRequest};

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeId};
use crate::plan::{Plan, PlanEntry, PlanMetrics, PlanTotals, PlannedRecipe};
use crate::util::{cheapest_alt, mat_norm_qty};

const EPS: f64 = 1e-9;

/// 速率归一（流体 mB → 桶）。
pub(crate) fn norm_rate(kind: MaterialKind, rate: f64) -> f64 {
    match kind {
        MaterialKind::Fluid => rate / 1000.0,
        _ => rate,
    }
}

/// 把配方操作量 / 原料 / 副产物组装成 Plan IR。
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_plan(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    mode: &str,
    ops: &[(RecipeId, f64)],
    raw: &[(MaterialId, f64)],
    byproducts: &[(MaterialId, f64)],
    metrics: PlanMetrics,
    notes: Vec<String>,
    elapsed_ms: f64,
) -> Plan {
    assemble_plan_with_choices(
        g,
        an,
        target,
        rate_per_min,
        mode,
        ops,
        raw,
        byproducts,
        None,
        metrics,
        notes,
        elapsed_ms,
    )
}

/// 带 OR 槽选择覆盖的组装（exact 模式用 LP 的决定）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_plan_with_choices(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    mode: &str,
    ops: &[(RecipeId, f64)],
    raw: &[(MaterialId, f64)],
    byproducts: &[(MaterialId, f64)],
    alt_choices: Option<&std::collections::HashMap<RecipeId, Vec<MaterialId>>>,
    mut metrics: PlanMetrics,
    mut notes: Vec<String>,
    elapsed_ms: f64,
) -> Plan {
    // 概率产出说明
    if g.chances.is_empty() {
        notes.push("概率产出无数据（JEI 不导出），按 100% 计；可用 --chances 提供覆盖表".to_string());
    } else {
        notes.push(format!(
            "已应用 {} 条概率覆盖（产出按期望值计）",
            g.chances.len()
        ));
    }
    let mut planned: Vec<PlannedRecipe> = Vec::new();
    let mut total_machines = 0.0f64;
    let mut total_machines_int = 0u64;
    let mut consume_eu_t = 0.0f64;
    let mut generate_eu_t = 0.0f64;
    let mut net_eu_per_min = 0.0f64;
    for &(rid, op) in ops {
        if op <= EPS {
            continue;
        }
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        // GT 数据：机器数 / EU
        let (machine_count, machine_count_int, eut, tier, eu_per_min) = match &r.gt {
            Some(gt) => {
                let machines = if gt.duration_ticks > 0 {
                    op * gt.duration_ticks as f64 / 1200.0
                } else {
                    0.0
                };
                if gt.consumes_energy() {
                    total_machines += machines;
                    total_machines_int += machines.ceil() as u64;
                    consume_eu_t += machines * gt.total_eu_t;
                    net_eu_per_min += op * gt.total_eu;
                } else if gt.generates_energy() {
                    total_machines += machines;
                    total_machines_int += machines.ceil() as u64;
                    generate_eu_t += machines * gt.total_eu_t;
                    net_eu_per_min -= op * gt.total_eu;
                }
                (
                    Some(machines),
                    Some(machines.ceil() as u64),
                    Some(gt.total_eu_t),
                    gt.tier.clone(),
                    Some(op * gt.total_eu),
                )
            }
            None => (None, None, None, None, None),
        };
        let mut inputs = Vec::new();
        for (si, slot) in r.inputs.iter().enumerate() {
            // LP 决定的 OR 槽选择优先
            let picked: Option<(MaterialId, u64)> = alt_choices
                .and_then(|ch| ch.get(&rid))
                .and_then(|picks| picks.get(si))
                .and_then(|&m| slot.alts.iter().find(|&&(am, _)| am == m).copied());
            let alt = picked.or_else(|| {
                cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot)
            });
            let Some((am, aq)) = alt else {
                continue;
            };
            inputs.push(PlanEntry {
                material: g.material_dto(am),
                rate_per_min: op * mat_norm_qty(g, am, aq),
                chance: None,
            });
        }
        let mut outputs = Vec::new();
        for slot in &r.outputs {
            let Some((om, oq)) = slot.primary() else {
                continue;
            };
            let chance = g.output_chance(rid, om);
            outputs.push(PlanEntry {
                material: g.material_dto(om),
                rate_per_min: op * mat_norm_qty(g, om, oq) * chance,
                chance: (chance < 1.0 - 1e-9).then_some(chance),
            });
        }
        planned.push(PlannedRecipe {
            recipe: g.recipe_full_id(rid),
            category: cat.ty.clone(),
            category_title: cat.title.clone(),
            ops_per_min: op,
            machine_count,
            machine_count_int,
            eut,
            tier,
            eu_per_min,
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
    for &(m, qty) in raw {
        if qty <= EPS {
            continue;
        }
        let info = g.material(m);
        estimated_cost += norm_rate(info.key.kind, qty);
        if info.key.kind.is_fluid() {
            raw_fluids += qty;
        } else {
            raw_items += qty;
        }
        raw_entries.push(PlanEntry {
            material: g.material_dto(m),
            rate_per_min: qty,
            chance: None,
        });
    }
    for &(m, qty) in byproducts {
        if qty <= EPS {
            continue;
        }
        byproduct_entries.push(PlanEntry {
            material: g.material_dto(m),
            rate_per_min: qty,
            chance: None,
        });
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

    let totals = PlanTotals {        distinct_recipes: planned.len(),
        recipe_ops_per_min: planned.iter().map(|p| p.ops_per_min).sum(),
        raw_items_per_min: raw_items,
        raw_fluids_mb_per_min: raw_fluids,
        estimated_cost,
        total_machines,
        total_machines_int,
        net_eu_t: consume_eu_t - generate_eu_t,
        consume_eu_t,
        generate_eu_t,
        net_eu_per_min,
        objective_score: an.cost.weights.material * estimated_cost
            + an.cost.weights.eu * net_eu_per_min
            + an.cost.weights.machine * total_machines,
    };

    metrics.search_ms = elapsed_ms;
    Plan {
        target: g.material_dto(target),
        rate_per_min,
        mode: mode.to_string(),
        recipes: planned,
        raw_materials: raw_entries,
        byproducts: byproduct_entries,
        totals,
        metrics,
        notes,
        elapsed_ms,
    }
}
