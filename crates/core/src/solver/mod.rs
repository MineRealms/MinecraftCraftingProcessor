//! LP 精确求解（exact mode）。
//!
//! 模型（物料平衡线性规划）：
//! - 变量：每条配方的操作量 `ops_r >= 0`；每种材料的"外部输入" `ext_m >= 0`
//! - 约束：对每种材料 m：
//!   `Σ 产出(m)·ops + ext_m − Σ 消耗(m)·ops >= 目标需求(m)`
//! - 目标：最小化 `Σ ext_m·单价 + ε·Σ ops_r`
//!   （物品单价 = 1/个；流体 = 1/桶；ε = 1e-6 用于在等价解中少开配方）
//!
//! 与启发式规划器的差异：
//! - LP 天然正确处理循环（物料平衡），不需要 SCC/锚点/展开上限；
//! - 但纯物料模型下"材料放大环"（1 锭 → 2 线 → 回收 2 锭）会让 LP 免费刷材料，
//!   因此默认排除回收类配方（可配置开启）；
//! - 槽的 OR 语义取成本最便宜的候选（精确 MILP 需要整数变量，留待后续）。
//!
//! 求解子图从目标材料沿生产者 BFS 构建，受 `max_materials` / `max_recipes` 限制。

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use good_lp::{default_solver, variable, Expression, ProblemVariables, Solution, SolverModel};

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeId};
use crate::plan::Plan;
use crate::util::cheapest_alt;

const EPS: f64 = 1e-9;

/// 精确求解参数。
#[derive(Debug, Clone)]
pub struct ExactOptions {
    /// 是否允许回收类配方（默认 false：纯物料模型下回收环会变成免费材料）
    pub include_recycling: bool,
    /// 是否屏蔽"循环 + 材料放大"配方（默认 false）。
    /// 注意：该启发式会误伤正常制造（如 1 晶圆切出多颗 CPU、线材拉细），
    /// 仅在需要"保守上界"时开启。
    pub block_amplification: bool,
    /// 子图材料上限
    pub max_materials: usize,
    /// 子图配方上限
    pub max_recipes: usize,
    /// 操作量罚项
    pub ops_penalty: f64,
    /// 最大电压等级（None = 不限制）
    pub max_tier: Option<u8>,
}

impl Default for ExactOptions {
    fn default() -> Self {
        Self {
            include_recycling: false,
            block_amplification: false,
            max_materials: 2000,
            max_recipes: 12000,
            ops_penalty: 1e-6,
            max_tier: None,
        }
    }
}

/// 配方增益：Σ 主输出归一量 / Σ 最便宜输入归一量。
fn recipe_gain(g: &KnowledgeGraph, an: &Analysis, r: &crate::model::RecipeNode) -> f64 {
    let mut out = 0.0;
    for slot in &r.outputs {
        if let Some((m, q)) = slot.primary() {
            out += crate::util::mat_norm_qty(g, m, q);
        }
    }
    let mut inp = 0.0;
    for slot in &r.inputs {
        if let Some((m, q)) = cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot) {
            inp += crate::util::mat_norm_qty(g, m, q);
        }
    }
    if inp > 0.0 {
        out / inp
    } else {
        f64::INFINITY
    }
}

struct LpRecipe {
    rid: RecipeId,
    inputs: Vec<(usize, f64)>,
    outputs: Vec<(usize, f64)>,
}

/// 精确求解：返回 Plan 或错误信息。
pub fn plan_exact(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    opts: &ExactOptions,
) -> Result<Plan, String> {
    let t0 = Instant::now();

    // ---------- 1) 构建子图 ----------
    let mut materials: Vec<MaterialId> = vec![target];
    let mut mat_index: HashMap<MaterialId, usize> = HashMap::new();
    mat_index.insert(target, 0);
    let mut lp_recipes: Vec<LpRecipe> = Vec::new();
    let mut rec_index: HashMap<RecipeId, usize> = HashMap::new();
    let mut truncated = false;
    let mut skipped_recycling = 0usize;
    let mut skipped_amplifying = 0usize;
    let m_count = g.materials.len();

    let mut queue: VecDeque<MaterialId> = VecDeque::new();
    queue.push_back(target);

    while let Some(m) = queue.pop_front() {
        for &rid in &g.producers[m as usize] {
            if !g.is_plannable(rid) {
                continue;
            }
            if !opts.include_recycling && g.is_recycling(rid) {
                skipped_recycling += 1;
                continue;
            }
            if !crate::util::tier_allowed(g, rid, opts.max_tier) {
                continue;
            }
            if rec_index.contains_key(&rid) {
                continue;
            }
            let r = g.recipe(rid);
            if r.inputs.is_empty() || r.outputs.is_empty() {
                continue;
            }
            // 可选：屏蔽"循环 + 材料放大"（保守模式）
            if opts.block_amplification {
                let comp = an.scc.comp[m_count + rid as usize];
                if an.scc.sizes[comp as usize] > 1 && recipe_gain(g, an, r) > 1.0 + 1e-9 {
                    skipped_amplifying += 1;
                    continue;
                }
            }
            if lp_recipes.len() >= opts.max_recipes {
                truncated = true;
                break;
            }

            // 输入槽：取最便宜候选；输出槽：取主候选
            let mut inputs: Vec<(usize, f64)> = Vec::new();
            let mut usable = true;
            for slot in &r.inputs {
                match cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot) {
                    Some((im, q)) => {
                        let li = match mat_index.get(&im) {
                            Some(&li) => li,
                            None => {
                                if materials.len() >= opts.max_materials {
                                    truncated = true;
                                    usable = false;
                                    break;
                                }
                                let li = materials.len();
                                materials.push(im);
                                mat_index.insert(im, li);
                                queue.push_back(im);
                                li
                            }
                        };
                        inputs.push((li, q as f64));
                    }
                    None => {
                        usable = false;
                        break;
                    }
                }
            }
            if !usable {
                continue;
            }
            let mut outputs: Vec<(usize, f64)> = Vec::new();
            for slot in &r.outputs {
                let Some((om, oq)) = slot.primary() else {
                    continue;
                };
                let lo = match mat_index.get(&om) {
                    Some(&lo) => lo,
                    None => {
                        if materials.len() >= opts.max_materials {
                            truncated = true;
                            break;
                        }
                        let lo = materials.len();
                        materials.push(om);
                        mat_index.insert(om, lo);
                        queue.push_back(om);
                        lo
                    }
                };
                outputs.push((lo, oq as f64));
            }
            if outputs.is_empty() {
                continue;
            }
            rec_index.insert(rid, lp_recipes.len());
            lp_recipes.push(LpRecipe {
                rid,
                inputs,
                outputs,
            });
        }
        if truncated && lp_recipes.len() >= opts.max_recipes {
            break;
        }
    }

    // ---------- 2) 构建 LP ----------
    let mut vars = ProblemVariables::new();
    let ops_vars: Vec<good_lp::Variable> = lp_recipes
        .iter()
        .map(|_| vars.add(variable().min(0.0)))
        .collect();
    let ext_vars: Vec<good_lp::Variable> = materials
        .iter()
        .map(|_| vars.add(variable().min(0.0)))
        .collect();

    let mut objective = Expression::default();
    for &v in &ops_vars {
        objective += v * opts.ops_penalty;
    }
    // 外部输入按成本库的启发式价格计费（不是统一 1.0）：
    // 否则 LP 会把"中子晶圆"这类复杂物品当 1 个原料直接用。
    for (i, &v) in ext_vars.iter().enumerate() {
        let m = materials[i];
        let uc = an.cost.unit_cost[m as usize];
        let base = if uc.is_finite() { uc } else { 1.0 };
        let coeff = match g.material(m).key.kind {
            MaterialKind::Fluid => base * 0.001, // 每 mB
            _ => base,                           // 每单位
        };
        objective += v * coeff;
    }

    let mut problem = vars.minimise(objective.clone()).using(default_solver);

    // 每个材料一条平衡约束（同一配方的输入/输出系数先合并）
    for i in 0..materials.len() {
        let mut coeffs: HashMap<usize, f64> = HashMap::new();
        for (ri, r) in lp_recipes.iter().enumerate() {
            for &(li, q) in &r.outputs {
                if li == i {
                    *coeffs.entry(ri).or_insert(0.0) += q;
                }
            }
            for &(li, q) in &r.inputs {
                if li == i {
                    *coeffs.entry(ri).or_insert(0.0) -= q;
                }
            }
        }
        let mut expr = Expression::default();
        for (ri, c) in coeffs {
            if c.abs() > EPS {
                expr += ops_vars[ri] * c;
            }
        }
        expr += ext_vars[i];
        let rhs = if materials[i] == target { rate_per_min } else { 0.0 };
        problem.add_constraint(expr.geq(rhs));
    }

    // ---------- 3) 求解 ----------
    let solution = problem
        .solve()
        .map_err(|e| format!("LP 求解失败: {}", e))?;
    let status = solution.status();

    // ---------- 4) 提取结果 ----------
    let ops: Vec<(RecipeId, f64)> = lp_recipes
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let v = solution.value(ops_vars[i]);
            (v > 1e-9).then_some((r.rid, v))
        })
        .collect();
    let raw: Vec<(MaterialId, f64)> = materials
        .iter()
        .enumerate()
        .filter_map(|(i, &m)| {
            let v = solution.value(ext_vars[i]);
            (v > 1e-9).then_some((m, v))
        })
        .collect();

    // 副产物 = 产出 − 消耗 − 目标
    let mut produced = vec![0.0f64; materials.len()];
    let mut consumed = vec![0.0f64; materials.len()];
    for (i, r) in lp_recipes.iter().enumerate() {
        let v = solution.value(ops_vars[i]);
        if v <= EPS {
            continue;
        }
        for &(li, q) in &r.outputs {
            produced[li] += v * q;
        }
        for &(li, q) in &r.inputs {
            consumed[li] += v * q;
        }
    }
    let byproducts: Vec<(MaterialId, f64)> = (0..materials.len())
        .filter_map(|i| {
            let tgt = if materials[i] == target { rate_per_min } else { 0.0 };
            let surplus = produced[i] - consumed[i] - tgt;
            (surplus > 1e-6).then_some((materials[i], surplus))
        })
        .collect();

    // ---------- 5) 组装 Plan ----------
    let mut notes: Vec<String> = Vec::new();
    notes.push(format!(
        "LP 精确求解：{} 材料 / {} 配方 / {} 变量，目标值 {:.4}，状态 {:?}",
        materials.len(),
        lp_recipes.len(),
        materials.len() + lp_recipes.len(),
        solution.eval(&objective),
        status
    ));
    notes.push(
        "外部输入按启发式价格计费（成本库），操作量可为小数".to_string(),
    );
    if skipped_recycling > 0 {
        notes.push(format!(
            "已排除回收类配方（{} 处引用）；可用 include_recycling 开启（纯物料模型下回收环可能刷材料）",
            skipped_recycling
        ));
    }
    if skipped_amplifying > 0 {
        notes.push(format!(
            "已屏蔽 {} 条'循环+材料放大'配方（保守模式，可能误伤正常制造）",
            skipped_amplifying
        ));
    }
    let cyclic_used = ops
        .iter()
        .filter(|(rid, _)| {
            let comp = an.scc.comp[m_count + *rid as usize];
            an.scc.sizes[comp as usize] > 1
        })
        .count();
    if cyclic_used > 0 {
        notes.push(format!(
            "方案包含 {} 条循环配方：物料模型不含耗电，可能利用放大环（实际游戏需耗电）；更保守的方案请用 beam 模式",
            cyclic_used
        ));
    }
    if truncated {
        notes.push(format!(
            "子图达到上限（材料 {} / 配方 {}），结果可能非全局最优",
            opts.max_materials, opts.max_recipes
        ));
    }
    notes.push("JEI 数据不含配方时长与耗电：机器数量与 EU 消耗未计算".to_string());

    Ok(super::planner::assemble_plan(
        g,
        an,
        target,
        rate_per_min,
        "exact",
        &ops,
        &raw,
        &byproducts,
        notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    ))
}
