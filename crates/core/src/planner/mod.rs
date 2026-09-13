//! 规划器层：确定性展开（tree）与 Beam Search（beam）。
//!
//! 两种模式共享 Plan IR 组装（`assemble_plan`）。

pub mod beam;
pub mod tree;

pub use beam::{plan_beam, BeamOptions};
pub use tree::{plan_tree, PlanRequest};

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeId};
use crate::plan::{Plan, PlanEntry, PlanTotals, PlannedRecipe};
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
    notes: Vec<String>,
    elapsed_ms: f64,
) -> Plan {
    let mut planned: Vec<PlannedRecipe> = Vec::new();
    for &(rid, op) in ops {
        if op <= EPS {
            continue;
        }
        let r = g.recipe(rid);
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
            recipe: g.recipe_full_id(rid),
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
        });
    }
    for &(m, qty) in byproducts {
        if qty <= EPS {
            continue;
        }
        byproduct_entries.push(PlanEntry {
            material: g.material_dto(m),
            rate_per_min: qty,
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

    let totals = PlanTotals {
        distinct_recipes: planned.len(),
        recipe_ops_per_min: planned.iter().map(|p| p.ops_per_min).sum(),
        raw_items_per_min: raw_items,
        raw_fluids_mb_per_min: raw_fluids,
        estimated_cost,
    };

    Plan {
        target: g.material_dto(target),
        rate_per_min,
        mode: mode.to_string(),
        recipes: planned,
        raw_materials: raw_entries,
        byproducts: byproduct_entries,
        totals,
        notes,
        elapsed_ms,
    }
}
