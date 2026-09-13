//! Beam Search（beam mode）：配方分配的局部搜索。
//!
//! 设计（与逐步需求展开不同，更适合本领域）：
//! 1. 以确定性展开（tree）的贪心解为基线；
//! 2. 每个候选状态 = 一份"材料 → 配方"选择表；
//! 3. 扰动：采样若干材料，把其配方换成次优候选（top-N）；
//! 4. 用**完整展开**评估每个候选（保证候选都是完整方案）；
//! 5. 保留得分最优的 K 份选择表进入下一轮，直到无改进或轮数用尽。
//!
//! 得分 = 估算成本（原始物品当量）+ λ·总操作量。
//! 好处：任何时刻都有完整方案；保证不劣于 tree 基线；
//! 后续 GPU 化的目标正是"批量评估候选选择表"这一步。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::plan::Plan;
use crate::planner::tree::{expand_with_choices, PlanRequest};
use crate::util::{mat_norm_qty, output_qty_of, recipe_cost};

/// Beam 规划参数。
#[derive(Debug, Clone)]
pub struct BeamOptions {
    /// 每轮保留的候选选择表数量 K。
    pub beam_width: usize,
    /// 每个材料枚举的替代配方数。
    pub candidate_limit: usize,
    /// 局部搜索轮数。
    pub max_iterations: usize,
    /// 每份选择表采样扰动的材料数。
    pub sample_per_state: usize,
    /// 操作量罚项（每 op 的原料当量代价）。
    pub ops_penalty: f64,
}

impl Default for BeamOptions {
    fn default() -> Self {
        Self {
            beam_width: 8,
            candidate_limit: 3,
            max_iterations: 12,
            sample_per_state: 24,
            ops_penalty: 0.001,
        }
    }
}

fn score(res: &crate::planner::tree::TreeResult, ops_penalty: f64) -> f64 {
    res.estimated_cost + ops_penalty * res.ops_total
}

/// 选择表哈希（排序后 FNV）。
fn choices_hash(choices: &HashMap<MaterialId, RecipeId>) -> u64 {
    let mut entries: Vec<(u32, u32)> = choices.iter().map(|(&m, &r)| (m, r)).collect();
    entries.sort_unstable();
    let mut h: u64 = 1469598103934665603;
    for (m, r) in entries {
        h ^= m as u64;
        h = h.wrapping_mul(1099511628211);
        h ^= r as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// 材料 m 的替代配方（按单位成本升序，排除当前选择）。
fn alternative_recipes(
    g: &KnowledgeGraph,
    an: &Analysis,
    m: MaterialId,
    current: RecipeId,
    limit: usize,
) -> Vec<RecipeId> {
    let mut cands: Vec<(RecipeId, f64)> = Vec::new();
    for &rid in &g.producers[m as usize] {
        if rid == current || !g.is_plannable(rid) {
            continue;
        }
        let r = g.recipe(rid);
        if r.inputs.is_empty() || an.is_dominated_for(rid, m) {
            continue;
        }
        let Some(out_q) = output_qty_of(r, m) else {
            continue;
        };
        let rc = recipe_cost(g, &an.cost.unit_cost, Some(&an.cost.cyclic), r);
        if !rc.is_finite() {
            continue;
        }
        let nq = mat_norm_qty(g, m, out_q);
        if nq <= 0.0 {
            continue;
        }
        cands.push((rid, rc / nq));
    }
    cands.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    cands.truncate(limit);
    cands.into_iter().map(|(rid, _)| rid).collect()
}

/// Beam Search 规划（贪心基线 + 配方分配局部搜索）。
pub fn plan_beam(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    opts: &BeamOptions,
) -> Plan {
    let t0 = Instant::now();
    let req = PlanRequest {
        target,
        rate_per_min,
        max_ops: 200_000,
    };
    let debug = std::env::var_os("GTP_BEAM_DEBUG").is_some();

    // 基线：贪心展开
    let empty: HashMap<MaterialId, RecipeId> = HashMap::new();
    let mut best = expand_with_choices(g, an, &req, &empty);
    let mut best_score = score(&best, opts.ops_penalty);
    let mut frontier: Vec<HashMap<MaterialId, RecipeId>> = vec![best.choices.clone()];
    let mut seen: HashSet<u64> = HashSet::new();
    seen.insert(choices_hash(&best.choices));

    let mut evals = 0usize;
    let mut rounds = 0usize;
    let mut improved = false;

    for _round in 0..opts.max_iterations {
        rounds += 1;
        let mut cands: Vec<(f64, HashMap<MaterialId, RecipeId>, crate::planner::tree::TreeResult)> =
            Vec::new();

        for choices in &frontier {
            let mut keys: Vec<MaterialId> = choices.keys().copied().collect();
            keys.sort_unstable();
            let step = (keys.len() / opts.sample_per_state.max(1)).max(1);
            let sampled: Vec<MaterialId> = keys
                .iter()
                .copied()
                .step_by(step)
                .take(opts.sample_per_state)
                .collect();

            for m in sampled {
                let Some(&cur) = choices.get(&m) else {
                    continue;
                };
                for alt in alternative_recipes(g, an, m, cur, opts.candidate_limit) {
                    let mut child = choices.clone();
                    child.insert(m, alt);
                    // 用完整展开评估；以实际生效的选择表作为规范状态
                    let res = expand_with_choices(g, an, &req, &child);
                    evals += 1;
                    let canonical = res.choices.clone();
                    let h = choices_hash(&canonical);
                    if !seen.insert(h) {
                        continue;
                    }
                    let sc = score(&res, opts.ops_penalty);
                    cands.push((sc, canonical, res));
                }
            }
        }

        if cands.is_empty() {
            break;
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));

        if debug {
            eprintln!(
                "beam round {}: evals={} best_candidate={:.4} current_best={:.4}",
                rounds, evals, cands[0].0, best_score
            );
        }

        if cands[0].0 < best_score - 1e-9 {
            // 采纳改进
            let (sc, ch, res) = cands.remove(0);
            best_score = sc;
            best = res;
            improved = true;
            frontier.clear();
            frontier.push(ch);
            for (_, c, _) in cands.iter().take(opts.beam_width.saturating_sub(1)) {
                frontier.push(c.clone());
            }
        } else {
            break; // 无改进，局部最优
        }
    }

    let mut notes = best.notes;
    notes.push(format!(
        "Beam 局部搜索：{} 轮，评估 {} 个候选{}",
        rounds,
        evals,
        if improved { "" } else { "（贪心基线已是最优）" }
    ));
    notes.push("JEI 数据不含配方时长与耗电：机器数量与 EU 消耗未计算".to_string());

    super::assemble_plan(
        g,
        an,
        target,
        rate_per_min,
        "beam",
        &best.ops,
        &best.raw,
        &best.byproducts,
        notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    )
}
