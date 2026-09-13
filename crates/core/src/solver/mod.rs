//! LP 精确求解（exact mode）。
//!
//! 模型（物料平衡线性规划，多目标加权）：
//! - 变量：
//!   - 每条配方操作量 `ops_r >= 0`
//!   - 每种材料外部输入 `ext_m >= 0`
//!   - **OR 槽选择变量** `f_{r,s,i} >= 0`（第 s 槽选第 i 个候选的"填充量"）
//! - 约束：
//!   - 物料平衡：`Σ 产出·ops + ext − Σ 消耗 >= 目标需求`
//!   - 槽填充：多候选槽 `Σ_i f_{s,i} = ops_r`（LP 内决定选哪个候选，
//!     不再像启发式那样贪心取最便宜）
//! - 目标（多目标加权）：
//!   `min Σ ext_m·价格_m + Σ ops_r·(w_eu·EU_r + w_machine·机器时间_r) + ε·Σ ops_r`
//!   价格/权重来自成本库（CostVector/CostWeights）。
//!
//! 与启发式规划器的差异：
//! - LP 天然正确处理循环与 OR 槽联合决策；
//! - 纯物料模型下"材料放大环"会让 LP 免费刷材料 → 默认排除回收类配方；
//! - 操作量是速率，可以为小数（机器台数取整在 Plan 中给出 MILP-lite 结果）。

use std::collections::HashMap;
use std::time::Instant;

use good_lp::{default_solver, variable, Expression, ProblemVariables, Solution, SolverModel};

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, MaterialKind, RecipeId};
use crate::plan::{Plan, PlanMetrics};
use crate::util::mat_norm_qty;

const EPS: f64 = 1e-9;
/// 中间产物"进口"惩罚倍数：只有原料/可采集材料能按 1.0 计价进口，
/// 中间产物尽量自产（保证 LP 目标是"从原料造出全链"的口径）。
const INTERMEDIATE_IMPORT_PENALTY: f64 = 1000.0;
/// 排序用的"不可达"价格（仅用于槽候选排序）。
const UNREACHABLE_PRICE: f64 = 1e6;
/// 每槽最多参与 LP 的候选数。
const MAX_SLOT_ALTS: usize = 4;
/// 每个材料最多展开的生产者数（按单位成本取 top-N，保证深链覆盖、子图不爆）。
const MAX_PRODUCERS_PER_MATERIAL: usize = 32;

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
            max_materials: 6000,
            max_recipes: 30000,
            ops_penalty: 1e-6,
            max_tier: None,
        }
    }
}

/// LP 输入槽：单候选直接消耗；多候选由 LP 决定。
enum LpInput {
    Direct { material: usize, qty: f64 },
    Choice { alts: Vec<(usize, f64)> },
}

struct LpRecipe {
    rid: RecipeId,
    inputs: Vec<LpInput>,
    outputs: Vec<(usize, f64)>,
}

/// 生产者的单位成本（用于子图 BFS 取 top-N）。
fn producer_score(g: &KnowledgeGraph, an: &Analysis, rid: RecipeId, m: MaterialId) -> f64 {
    let r = g.recipe(rid);
    let rc = crate::util::recipe_cost(g, &an.cost.unit_cost, Some(&an.cost.cyclic), r);
    let Some(oq) = crate::util::output_qty_of(r, m) else {
        return f64::INFINITY;
    };
    let nq = mat_norm_qty(g, m, oq);
    if rc.is_finite() && nq > 0.0 {
        rc / nq
    } else {
        f64::INFINITY
    }
}

/// 配方增益：Σ 主输出归一量 / Σ 最便宜输入归一量。
fn recipe_gain(g: &KnowledgeGraph, an: &Analysis, r: &crate::model::RecipeNode) -> f64 {
    let mut out = 0.0;
    for slot in &r.outputs {
        if let Some((m, q)) = slot.primary() {
            out += mat_norm_qty(g, m, q);
        }
    }
    let mut inp = 0.0;
    for slot in &r.inputs {
        if let Some((m, q)) = crate::util::cheapest_alt(
            g,
            &an.cost.unit_cost,
            Some(&an.cost.cyclic),
            slot,
        ) {
            inp += mat_norm_qty(g, m, q);
        }
    }
    if inp > 0.0 {
        out / inp
    } else {
        f64::INFINITY
    }
}

/// 材料展开优先级：(深度, 最优生产者成本) 字典序。
/// 深度优先保证"接近源材料"的路线先进入子图（避免预算被
/// 封闭转换家族耗尽）；成本用于同深度内的排序。
fn heap_prio(g: &KnowledgeGraph, an: &Analysis, opts: &ExactOptions, m: MaterialId) -> u64 {
    let depth = an.cost.depth[m as usize].min(0xffff) as u64;
    let mut best = f64::INFINITY;
    for &rid in &g.producers[m as usize] {
        if !g.is_plannable(rid) {
            continue;
        }
        if !opts.include_recycling && g.is_recycling(rid) {
            continue;
        }
        if !crate::util::tier_allowed(g, rid, opts.max_tier) {
            continue;
        }
        let s = producer_score(g, an, rid, m);
        if s < best {
            best = s;
        }
    }
    let score_bits = best.to_bits() >> 32; // 取高 32 位（非负浮点保持单调）
    (depth << 32) | score_bits
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
    let weights = an.cost.weights;

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

    // best-first：优先展开"最优路线成本最低"的材料；
    // 每个材料展开全部可用生产者（每材料上限 32）。
    // 为什么不能只取 top-N：纯按启发式成本排序会陷入封闭转换家族
    // （微型粉↔小堆粉↔粉），永远够不到矿石链；best-first + 全量生产者
    // 保证便宜家族与矿石链都能进入子图（受总配方预算约束）。
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, MaterialId)>> =
        std::collections::BinaryHeap::new();
    heap.push(std::cmp::Reverse((heap_prio(g, an, opts, target), target)));

    while let Some(std::cmp::Reverse((_, m))) = heap.pop() {
        // 可用生产者（回收/tier 过滤）
        let mut prods: Vec<(f64, RecipeId)> = Vec::new();
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
            prods.push((producer_score(g, an, rid, m), rid));
        }
        prods.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        prods.truncate(MAX_PRODUCERS_PER_MATERIAL);

        let mut added = 0usize;
        for &(_, rid) in &prods {
            if rec_index.contains_key(&rid) {
                continue;
            }
            let r = g.recipe(rid);
            if r.inputs.is_empty() || r.outputs.is_empty() {
                continue;
            }
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

            // 输入槽：单候选直接；多候选进入 LP（按加权成本排序取前 N）
            let mut inputs: Vec<LpInput> = Vec::new();
            let mut usable = true;
            for slot in &r.inputs {
                let mut alts: Vec<(usize, u64, f64)> = Vec::new(); // (local, qty, score)
                for &(im, q) in &slot.alts {
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
                            heap.push(std::cmp::Reverse((heap_prio(g, an, opts, im), im)));
                            li
                        }
                    };
                    let c = an.cost.unit_cost[im as usize];
                    let price = if c.is_finite() { c } else { UNREACHABLE_PRICE };
                    alts.push((li, q, price * mat_norm_qty(g, im, q)));
                }
                if !usable {
                    break;
                }
                if alts.is_empty() {
                    usable = false;
                    break;
                }
                if alts.len() == 1 {
                    inputs.push(LpInput::Direct {
                        material: alts[0].0,
                        qty: alts[0].1 as f64,
                    });
                } else {
                    alts.sort_by(|a, b| a.2.total_cmp(&b.2));
                    alts.truncate(MAX_SLOT_ALTS);
                    inputs.push(LpInput::Choice {
                        alts: alts.into_iter().map(|(li, q, _)| (li, q as f64)).collect(),
                    });
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
                        heap.push(std::cmp::Reverse((heap_prio(g, an, opts, om), om)));
                        lo
                    }
                };
                outputs.push((lo, oq as f64 * g.output_chance(rid, om)));
            }
            if outputs.is_empty() {
                continue;
            }
            rec_index.insert(rid, lp_recipes.len());
            added += 1;
            lp_recipes.push(LpRecipe {
                rid,
                inputs,
                outputs,
            });
        }
        if std::env::var_os("GTP_DEBUG_SOLVER").is_some() {
            if let Ok(pat) = std::env::var("GTP_DEBUG_MAT") {
                if g.material_id_str(m).contains(&pat) {
                    eprintln!(
                        "solver: pop {} (producers={}, chosen={}, added={}, is_raw={})",
                        g.material_id_str(m),
                        g.producers[m as usize].len(),
                        prods.len(),
                        added,
                        g.is_source(m)
                    );
                }
            }
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
    // OR 槽填充变量：choice_vars[ri][slot] = 各候选变量
    let mut choice_vars: Vec<Vec<Vec<good_lp::Variable>>> = Vec::with_capacity(lp_recipes.len());
    for r in &lp_recipes {
        let mut per_recipe: Vec<Vec<good_lp::Variable>> = Vec::new();
        for input in &r.inputs {
            match input {
                LpInput::Direct { .. } => per_recipe.push(Vec::new()),
                LpInput::Choice { alts } => {
                    let vs: Vec<good_lp::Variable> =
                        alts.iter().map(|_| vars.add(variable().min(0.0))).collect();
                    per_recipe.push(vs);
                }
            }
        }
        choice_vars.push(per_recipe);
    }

    // 目标：外部输入按加权价格 + 操作按 EU/机器时间加权 + ε·ops
    let mut objective = Expression::default();
    for (i, &v) in ext_vars.iter().enumerate() {
        let m = materials[i];
        let is_raw = g.is_source(m);
        let base = if is_raw {
            1.0
        } else {
            INTERMEDIATE_IMPORT_PENALTY
        };
        let coeff = match g.material(m).key.kind {
            MaterialKind::Fluid => weights.material * base * 0.001, // 每 mB
            _ => weights.material * base,                           // 每单位
        };
        if std::env::var_os("GTP_DEBUG_SOLVER").is_some() {
            if let Ok(pat) = std::env::var("GTP_DEBUG_MAT") {
                if g.material_id_str(m).contains(&pat) {
                    eprintln!(
                        "solver: ext {} is_raw={} base={} coeff={} w_m={}",
                        g.material_id_str(m),
                        is_raw,
                        base,
                        coeff,
                        weights.material
                    );
                }
            }
        }
        objective += v * coeff;
    }
    for (ri, r) in lp_recipes.iter().enumerate() {
        let recipe = g.recipe(r.rid);
        let (eu, machine) = match &recipe.gt {
            Some(gt) => (
                if gt.consumes_energy() { gt.total_eu } else { 0.0 },
                gt.duration_ticks as f64 / 1200.0,
            ),
            None => (0.0, 0.0),
        };
        let per_op = weights.eu * eu + weights.machine * machine + opts.ops_penalty;
        objective += ops_vars[ri] * per_op;
    }

    let mut problem = vars.minimise(objective.clone()).using(default_solver);

    // 物料平衡约束
    for i in 0..materials.len() {
        let mut coeffs: HashMap<usize, f64> = HashMap::new(); // key = 局部变量序号（ops / ext 分开处理）
        // 产出
        for (ri, r) in lp_recipes.iter().enumerate() {
            for &(li, q) in &r.outputs {
                if li == i {
                    *coeffs.entry(ri).or_insert(0.0) += q;
                }
            }
        }
        // 消耗（单候选槽）
        for (ri, r) in lp_recipes.iter().enumerate() {
            for input in r.inputs.iter() {
                if let LpInput::Direct { material, qty } = input {
                    if *material == i {
                        *coeffs.entry(ri).or_insert(0.0) -= qty;
                    }
                }
            }
        }
        let mut expr = Expression::default();
        for (ri, c) in coeffs {
            if c.abs() > EPS {
                expr += ops_vars[ri] * c;
            }
        }
        // 消耗（多候选槽）
        for (ri, r) in lp_recipes.iter().enumerate() {
            for (si, input) in r.inputs.iter().enumerate() {
                if let LpInput::Choice { alts } = input {
                    for (ai, &(material, qty)) in alts.iter().enumerate() {
                        if material == i {
                            expr += choice_vars[ri][si][ai] * (-qty);
                        }
                    }
                }
            }
        }
        expr += ext_vars[i];
        let rhs = if materials[i] == target { rate_per_min } else { 0.0 };
        problem.add_constraint(expr.geq(rhs));
    }

    // OR 槽填充约束：Σ f_i = ops_r
    for (ri, r) in lp_recipes.iter().enumerate() {
        for (si, input) in r.inputs.iter().enumerate() {
            if let LpInput::Choice { alts } = input {
                let mut expr = Expression::default();
                for (ai, _) in alts.iter().enumerate() {
                    expr += choice_vars[ri][si][ai];
                }
                expr += ops_vars[ri] * (-1.0);
                problem.add_constraint(expr.eq(0.0));
            }
        }
    }

    // ---------- 3) 求解 ----------
    let solution = problem
        .solve()
        .map_err(|e| format!("LP 求解失败: {}", e))?;
    let status = solution.status();
    log::info!(
        "LP：{} 材料 / {} 配方 / {} 变量，状态 {:?}，目标值 {:.4}",
        materials.len(),
        lp_recipes.len(),
        materials.len() + lp_recipes.len(),
        status,
        solution.eval(&objective)
    );
    if std::env::var_os("GTP_DEBUG_SOLVER").is_some() {
        let mut manual = 0.0f64;
        for (i, &v) in ext_vars.iter().enumerate() {
            let m = materials[i];
            let is_raw = g.is_source(m);
            let base = if is_raw { 1.0 } else { INTERMEDIATE_IMPORT_PENALTY };
            let coeff = match g.material(m).key.kind {
                MaterialKind::Fluid => weights.material * base * 0.001,
                _ => weights.material * base,
            };
            manual += solution.value(v) * coeff;
        }
        for (ri, r) in lp_recipes.iter().enumerate() {
            let recipe = g.recipe(r.rid);
            let (eu, machine) = match &recipe.gt {
                Some(gt) => (
                    if gt.consumes_energy() { gt.total_eu } else { 0.0 },
                    gt.duration_ticks as f64 / 1200.0,
                ),
                None => (0.0, 0.0),
            };
            manual += solution.value(ops_vars[ri])
                * (weights.eu * eu + weights.machine * machine + opts.ops_penalty);
        }
        eprintln!(
            "solver: objective eval={} manual={}",
            solution.eval(&objective),
            manual
        );
    }

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

    // LP 决定的 OR 槽选择：recipe → 每槽选中的输入材料（用于 Plan 展示）
    let mut alt_choices: HashMap<RecipeId, Vec<MaterialId>> = HashMap::new();
    for (ri, r) in lp_recipes.iter().enumerate() {
        let mut picks: Vec<MaterialId> = Vec::new();
        let mut has_choice = false;
        for (si, input) in r.inputs.iter().enumerate() {
            match input {
                LpInput::Direct { material, .. } => picks.push(materials[*material]),
                LpInput::Choice { alts } => {
                    has_choice = true;
                    let mut best_ai = 0usize;
                    let mut best_v = 0.0f64;
                    for (ai, _) in alts.iter().enumerate() {
                        let v = solution.value(choice_vars[ri][si][ai]);
                        if v > best_v {
                            best_v = v;
                            best_ai = ai;
                        }
                    }
                    picks.push(materials[alts[best_ai].0]);
                }
            }
        }
        if has_choice {
            alt_choices.insert(r.rid, picks);
        }
    }

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
        for (si, input) in r.inputs.iter().enumerate() {
            match input {
                LpInput::Direct { material, qty } => consumed[*material] += v * qty,
                LpInput::Choice { alts } => {
                    for (ai, &(material, qty)) in alts.iter().enumerate() {
                        consumed[material] += solution.value(choice_vars[i][si][ai]) * qty;
                    }
                }
            }
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
        "LP 精确求解：{} 材料 / {} 配方，目标值 {:.4}，状态 {:?}",
        materials.len(),
        lp_recipes.len(),
        solution.eval(&objective),
        status
    ));
    notes.push(
        "多目标加权：外部输入按 CostVector 价格计费；操作计入 EU×w_eu + 机器时间×w_machine；OR 槽由 LP 联合决定"
            .to_string(),
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
            "方案包含 {} 条循环配方：物料模型不含耗电惩罚时可能利用放大环；更保守的方案请用 beam 模式",
            cyclic_used
        ));
    }
    if truncated {
        notes.push(format!(
            "子图达到上限（材料 {} / 配方 {}），结果可能非全局最优",
            opts.max_materials, opts.max_recipes
        ));
    }
    notes.push("原版配方无 GT 时长/耗电数据，机器数与 EU 仅统计 GT 配方".to_string());

    let choice_var_count: usize = choice_vars
        .iter()
        .flat_map(|v| v.iter().map(Vec::len))
        .sum();
    let choice_slot_count: usize = choice_vars
        .iter()
        .flat_map(|v| v.iter().filter(|c| !c.is_empty()))
        .count();
    let mut metrics = PlanMetrics::default();
    metrics.lp_variables = Some(materials.len() + lp_recipes.len() + choice_var_count);
    metrics.lp_constraints = Some(materials.len() + choice_slot_count);
    metrics.lp_status = Some(format!("{:?}", status));
    metrics.lp_objective = Some(solution.eval(&objective));
    metrics.evaluations = 1;

    Ok(super::planner::assemble_plan_with_choices(
        g,
        an,
        target,
        rate_per_min,
        "exact",
        &ops,
        &raw,
        &byproducts,
        Some(&alt_choices),
        metrics,
        notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    ))
}
