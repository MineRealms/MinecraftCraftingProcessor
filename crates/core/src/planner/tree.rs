//! 确定性展开（tree mode）：工作队列 + 环内线性缩放。
//!
//! 算法：
//! 1. 需求队列按材料展开，每种材料用启发式最优配方（可被 overrides 覆盖）；
//! 2. 环内材料复用已选配方，按需求量线性放大（迭代收敛）；
//! 3. 其他输出按"抵原料 → 抵需求 → 记副产物"顺序处理；
//! 4. 输入槽取单位成本最便宜的候选。
//!
//! `expand_tree` 是纯展开内核，供 tree / beam 共用：
//! beam 以贪心选择为基线，扰动"某材料换配方"后用完整展开评估。

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::plan::Plan;
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
    /// 最大电压等级（None = 不限制）。
    pub max_tier: Option<u8>,
}

impl PlanRequest {
    pub fn new(target: MaterialId, rate_per_min: f64) -> Self {
        Self {
            target,
            rate_per_min,
            max_ops: 200_000,
            max_tier: None,
        }
    }
}

/// 选配方：优先成本库的 best_recipe；若超出电压等级则找等级内最便宜的。
fn pick_best_recipe(
    g: &KnowledgeGraph,
    an: &Analysis,
    m: MaterialId,
    max_tier: Option<u8>,
) -> Option<RecipeId> {
    if let Some(best) = an.cost.best_recipe[m as usize] {
        if crate::util::tier_allowed(g, best, max_tier) {
            return Some(best);
        }
    }
    let mut best: Option<(f64, RecipeId)> = None;
    for &rid in &g.producers[m as usize] {
        if !g.is_plannable(rid) || !crate::util::tier_allowed(g, rid, max_tier) {
            continue;
        }
        let r = g.recipe(rid);
        if r.inputs.is_empty() {
            continue;
        }
        let Some(oq) = output_qty_of(r, m) else {
            continue;
        };
        let rc = crate::util::recipe_cost(g, &an.cost.unit_cost, Some(&an.cost.cyclic), r);
        if !rc.is_finite() {
            continue;
        }
        let nq = mat_norm_qty(g, m, oq);
        if nq <= 0.0 {
            continue;
        }
        let per = rc / nq;
        if best.as_ref().map(|(b, _)| per < *b).unwrap_or(true) {
            best = Some((per, rid));
        }
    }
    best.map(|(_, r)| r)
}

/// 展开结果（未组装 Plan）。
pub(crate) struct TreeResult {
    pub ops: Vec<(RecipeId, f64)>,
    pub raw: Vec<(MaterialId, f64)>,
    pub byproducts: Vec<(MaterialId, f64)>,
    /// 实际使用的配方选择（材料 → 配方）。
    pub choices: HashMap<MaterialId, RecipeId>,
    pub notes: Vec<String>,
    /// 估算成本（原始物品当量）。
    pub estimated_cost: f64,
    /// 总操作量。
    pub ops_total: f64,
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

/// 记录备注（去重）。
fn note(notes: &mut Vec<String>, msg: String) {
    if !notes.contains(&msg) {
        notes.push(msg);
    }
}

/// 确定性展开内核。`overrides` 指定材料的配方选择（未指定则用启发式最优）。
pub(crate) fn expand_tree(
    g: &KnowledgeGraph,
    an: &Analysis,
    req: &PlanRequest,
    overrides: &HashMap<MaterialId, RecipeId>,
) -> TreeResult {
    let n = g.materials.len();
    let mut notes: Vec<String> = Vec::new();

    let mut demand: Vec<f64> = vec![0.0; n];
    let mut raw: Vec<f64> = vec![0.0; n];
    let mut byproduct: Vec<f64> = vec![0.0; n];
    let mut chosen: Vec<Option<RecipeId>> = vec![None; n];
    for (&m, &r) in overrides {
        chosen[m as usize] = Some(r);
    }
    let mut ops: Vec<f64> = vec![0.0; g.recipes.len()];
    let mut queued = vec![false; n];
    let mut queue: VecDeque<MaterialId> = VecDeque::new();
    // 每材料展开次数（循环保险：超限按外部输入处理）
    let mut visits: Vec<u16> = vec![0; n];
    let mut visit_limit_hit = false;
    // 无可达生产路线的材料（聚合为一条备注）
    let mut no_route: Vec<MaterialId> = Vec::new();

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
            note(
                &mut notes,
                format!(
                    "需求发散（{} 达到 {:e}），已中止展开",
                    g.material_id_str(m),
                    qty
                ),
            );
            break;
        }
        // 循环保险：同一材料展开次数超限后按外部输入处理
        visits[m as usize] += 1;
        if visits[m as usize] > 16 {
            visit_limit_hit = true;
            raw[m as usize] += qty;
            continue;
        }

        // 源材料（无可规划生产者）：直接记账
        if g.is_source(m) {
            raw[m as usize] += qty;
            continue;
        }

        // 选定配方（首次决定后固定，环内复用）。
        // 无可达生产路线（可采集资源 / 纯循环不可达）→ 按外部输入处理。
        let rid = match chosen[m as usize] {
            Some(r) => r,
            None => match pick_best_recipe(g, an, m, req.max_tier) {
                Some(r) => {
                    chosen[m as usize] = Some(r);
                    r
                }
                None => {
                    if !g.harvestable[m as usize] {
                        no_route.push(m);
                    }
                    raw[m as usize] += qty;
                    continue;
                }
            },
        };
        let recipe = g.recipe(rid);
        let Some(out_q) = output_qty_of(recipe, m) else {
            note(
                &mut notes,
                format!(
                    "配方 {} 不产出目标材料，已按原料处理",
                    g.recipe_full_id(rid)
                ),
            );
            raw[m as usize] += qty;
            continue;
        };

        expansions += 1;
        if expansions > req.max_ops {
            truncated = true;
            note(
                &mut notes,
                format!("展开次数达到上限 {}，计划可能不完整", req.max_ops),
            );
            break;
        }

        let ops_add = qty / (mat_norm_qty(g, m, out_q) * g.output_chance(rid, m));
        ops[rid as usize] += ops_add;

        // 其他输出：抵原料 → 抵需求 → 记副产物（概率产出按期望值计）
        for slot in &recipe.outputs {
            let Some((om, oq)) = slot.primary() else {
                continue;
            };
            if om == m {
                continue;
            }
            let mut left = ops_add * mat_norm_qty(g, om, oq) * g.output_chance(rid, om);
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

    if !no_route.is_empty() {
        let mut names: Vec<String> = no_route
            .iter()
            .map(|&m| g.material_id_str(m).to_string())
            .collect();
        names.sort();
        names.dedup();
        let shown: Vec<String> = names.iter().take(5).cloned().collect();
        note(
            &mut notes,
            format!(
                "{} 种材料无可达生产路线，按外部输入处理（前 5：{}{}）",
                names.len(),
                shown.join(", "),
                if names.len() > 5 { " …" } else { "" }
            ),
        );
    }
    if visit_limit_hit {
        note(
            &mut notes,
            "部分材料循环展开次数超限，已按外部输入处理".to_string(),
        );
    }
    if !an.cost.converged {
        note(
            &mut notes,
            "成本迭代未完全收敛（存在复杂循环），估算值可能偏差".to_string(),
        );
    }
    if truncated {
        note(
            &mut notes,
            "结果不完整：存在未满足需求或循环放大".to_string(),
        );
    }

    // ---------------- 汇总 ----------------
    let ops_vec: Vec<(RecipeId, f64)> = ops
        .iter()
        .enumerate()
        .filter(|(_, &op)| op > EPS)
        .map(|(rid, &op)| (rid as RecipeId, op))
        .collect();
    let raw_vec: Vec<(MaterialId, f64)> = (0..n)
        .filter(|&m| raw[m] > EPS)
        .map(|m| (m as MaterialId, raw[m]))
        .collect();
    let byproduct_vec: Vec<(MaterialId, f64)> = (0..n)
        .filter(|&m| byproduct[m] > EPS)
        .map(|m| (m as MaterialId, byproduct[m]))
        .collect();
    let estimated_cost: f64 = raw_vec
        .iter()
        .map(|&(m, q)| super::norm_rate(g.material(m).key.kind, q))
        .sum();
    let ops_total: f64 = ops_vec.iter().map(|&(_, q)| q).sum();
    let choices: HashMap<MaterialId, RecipeId> = chosen
        .iter()
        .enumerate()
        .filter_map(|(m, r)| r.map(|r| (m as MaterialId, r)))
        .collect();

    TreeResult {
        ops: ops_vec,
        raw: raw_vec,
        byproducts: byproduct_vec,
        choices,
        notes,
        estimated_cost,
        ops_total,
    }
}

/// 确定性展开规划。
pub fn plan_tree(g: &KnowledgeGraph, an: &Analysis, req: &PlanRequest) -> Plan {
    let t0 = Instant::now();
    let mut res = expand_tree(g, an, req, &HashMap::new());
    res.notes
        .push("原版配方无 GT 时长/耗电数据，机器数与 EU 仅统计 GT 配方".to_string());
    super::assemble_plan(
        g,
        an,
        req.target,
        req.rate_per_min,
        "tree",
        &res.ops,
        &res.raw,
        &res.byproducts,
        res.notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    )
}

/// 带配方覆盖的展开（beam 局部搜索评估用）。
pub(crate) fn expand_with_choices(
    g: &KnowledgeGraph,
    an: &Analysis,
    req: &PlanRequest,
    choices: &HashMap<MaterialId, RecipeId>,
) -> TreeResult {
    expand_tree(g, an, req, choices)
}

/// 返回贪心展开的配方分配（供 GPU 线性求解 / 流量分析使用）。
pub fn greedy_choices(
    g: &KnowledgeGraph,
    an: &Analysis,
    req: &PlanRequest,
) -> HashMap<MaterialId, RecipeId> {
    expand_with_choices(g, an, req, &HashMap::new()).choices
}
