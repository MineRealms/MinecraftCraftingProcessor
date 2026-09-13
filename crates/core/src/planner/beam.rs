//! Beam Search（beam mode）：配方分配的局部搜索。
//!
//! 设计（与逐步需求展开不同，更适合本领域）：
//! 1. 以确定性展开（tree）的贪心解为基线；
//! 2. 每个候选状态 = 一份"材料 → 配方"选择表；
//! 3. 扰动：把某材料的配方换成次优候选（top-N）；
//! 4. 用**完整展开**评估每个候选（保证候选都是完整方案）；
//! 5. 保留得分最优的 K 份选择表进入下一轮，直到无改进或轮数用尽。
//!
//! 两条路径：
//! - CPU 采样：每轮采样若干材料做扰动（默认）；
//! - 批量评估（GPU）：枚举**全部**单点扰动 → 批量粗筛 → CPU 精评 top-K，
//!   最后再跑一轮 CPU 采样兜底（保证不劣于纯 CPU 路径）。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::plan::Plan;
use crate::planner::tree::{expand_with_choices, PlanRequest, TreeResult};
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
    /// 每份选择表采样扰动的材料数（CPU 路径）。
    pub sample_per_state: usize,
    /// 操作量罚项（每 op 的原料当量代价）。
    pub ops_penalty: f64,
    /// 最大电压等级（None = 不限制）。
    pub max_tier: Option<u8>,
}

impl Default for BeamOptions {
    fn default() -> Self {
        Self {
            beam_width: 8,
            candidate_limit: 3,
            max_iterations: 12,
            sample_per_state: 24,
            ops_penalty: 0.001,
            max_tier: None,
        }
    }
}

/// 批量候选评估器（GPU 实现挂在这里；返回得分，越小越好）。
pub trait BatchEvaluator {
    /// 用基线方案初始化评估上下文（如构建评估子图、上传缓冲）。
    /// 默认实现为空（无状态评估器可忽略）。
    fn prepare(
        &mut self,
        _g: &KnowledgeGraph,
        _an: &Analysis,
        _target: MaterialId,
        _rate_per_min: f64,
        _choices: &HashMap<MaterialId, RecipeId>,
    ) {
    }

    fn evaluate(&self, choices: &[HashMap<MaterialId, RecipeId>]) -> Vec<f64>;

    /// 评估器名称（写入计划备注）
    fn name(&self) -> String {
        "batch".to_string()
    }
}

fn score(res: &TreeResult, ops_penalty: f64) -> f64 {
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

/// 材料 m 的替代配方（按单位成本升序；`current` 会被排除）。
pub fn recipe_alternatives(
    g: &KnowledgeGraph,
    an: &Analysis,
    m: MaterialId,
    current: Option<RecipeId>,
    limit: usize,
    max_tier: Option<u8>,
) -> Vec<RecipeId> {
    let mut cands: Vec<(RecipeId, f64)> = Vec::new();
    for &rid in &g.producers[m as usize] {
        if Some(rid) == current || !g.is_plannable(rid) || !crate::util::tier_allowed(g, rid, max_tier)
        {
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

/// 材料 m 的替代配方（排除当前选择）。
fn alternative_recipes(
    g: &KnowledgeGraph,
    an: &Analysis,
    m: MaterialId,
    current: RecipeId,
    limit: usize,
    max_tier: Option<u8>,
) -> Vec<RecipeId> {
    recipe_alternatives(g, an, m, Some(current), limit, max_tier)
}

/// 局部搜索状态。
struct SearchState {
    best: TreeResult,
    best_score: f64,
    frontier: Vec<HashMap<MaterialId, RecipeId>>,
    seen: HashSet<u64>,
    evals: usize,
    rounds: usize,
    improved: bool,
}

/// CPU 采样局部搜索（每轮采样若干材料扰动 + 完整展开评估）。
fn cpu_local_search(
    g: &KnowledgeGraph,
    an: &Analysis,
    req: &PlanRequest,
    opts: &BeamOptions,
    mut st: SearchState,
    debug: bool,
) -> SearchState {
    for _round in 0..opts.max_iterations {
        st.rounds += 1;
        let mut cands: Vec<(f64, HashMap<MaterialId, RecipeId>, TreeResult)> = Vec::new();

        for choices in &st.frontier {
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
                    for alt in alternative_recipes(g, an, m, cur, opts.candidate_limit, opts.max_tier)
                    {
                    let mut child = choices.clone();
                    child.insert(m, alt);
                    // 用完整展开评估；以实际生效的选择表作为规范状态
                    let res = expand_with_choices(g, an, req, &child);
                    st.evals += 1;
                    let canonical = res.choices.clone();
                    let h = choices_hash(&canonical);
                    if !st.seen.insert(h) {
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
                "beam(cpu) round {}: evals={} best_candidate={:.4} current_best={:.4}",
                st.rounds, st.evals, cands[0].0, st.best_score
            );
        }

        if cands[0].0 < st.best_score - 1e-9 {
            let (sc, ch, res) = cands.remove(0);
            st.best_score = sc;
            st.best = res;
            st.improved = true;
            st.frontier.clear();
            st.frontier.push(ch);
            for (_, c, _) in cands.iter().take(opts.beam_width.saturating_sub(1)) {
                st.frontier.push(c.clone());
            }
        } else {
            break; // 无改进，局部最优
        }
    }
    st
}

/// Beam Search 规划（贪心基线 + 配方分配局部搜索，CPU 路径）。
pub fn plan_beam(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    opts: &BeamOptions,
) -> Plan {
    plan_beam_with_evaluator(g, an, target, rate_per_min, opts, None)
}

/// 带批量评估器的 Beam：
/// - 提供 `evaluator`（如 GPU）时：枚举全部单点扰动 → 批量粗筛 → CPU 精评 top-K，
///   最后再跑 CPU 采样兜底；
/// - 未提供时：仅 CPU 采样扰动 + 完整展开。
pub fn plan_beam_with_evaluator(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    opts: &BeamOptions,
    mut evaluator: Option<&mut dyn BatchEvaluator>,
) -> Plan {
    let t0 = Instant::now();
    let req = PlanRequest {
        target,
        rate_per_min,
        max_ops: 200_000,
        max_tier: opts.max_tier,
    };
    let debug = std::env::var_os("GTP_BEAM_DEBUG").is_some();

    // 基线：贪心展开
    let empty: HashMap<MaterialId, RecipeId> = HashMap::new();
    let baseline = expand_with_choices(g, an, &req, &empty);
    let baseline_score = score(&baseline, opts.ops_penalty);
    if let Some(ev) = evaluator.as_deref_mut() {
        ev.prepare(g, an, target, rate_per_min, &baseline.choices);
    }

    let mut st = SearchState {
        best: baseline,
        best_score: baseline_score,
        frontier: Vec::new(),
        seen: HashSet::new(),
        evals: 0,
        rounds: 0,
        improved: false,
    };
    st.frontier.push(st.best.choices.clone());
    st.seen.insert(choices_hash(&st.best.choices));

    let evaluator_name = evaluator.as_deref().map(|e| e.name()).unwrap_or_default();
    let mut batch_evaluated = 0usize;

    // ---- 批量评估路径（GPU 粗筛 + CPU 精评） ----
    if let Some(ev) = evaluator.as_deref() {
        for _round in 0..opts.max_iterations {
            st.rounds += 1;
            let mut cand_choices: Vec<HashMap<MaterialId, RecipeId>> = Vec::new();
            for choices in &st.frontier {
                let mut keys: Vec<MaterialId> = choices.keys().copied().collect();
                keys.sort_unstable();
                for m in keys {
                    let Some(&cur) = choices.get(&m) else {
                        continue;
                    };
                for alt in alternative_recipes(g, an, m, cur, opts.candidate_limit, opts.max_tier) {
                        let mut child = choices.clone();
                        child.insert(m, alt);
                        if st.seen.insert(choices_hash(&child)) {
                            cand_choices.push(child);
                        }
                    }
                }
            }
            if cand_choices.is_empty() {
                break;
            }
            let scores = ev.evaluate(&cand_choices);
            batch_evaluated += cand_choices.len();

            // 取粗筛得分有限的前 K（K 取 beam_width 与 48 的较大值，提升精评覆盖）
            let topk = opts.beam_width.max(48);
            let mut idx: Vec<usize> = (0..cand_choices.len())
                .filter(|&i| scores.get(i).map(|s| s.is_finite()).unwrap_or(false))
                .collect();
            idx.sort_by(|&a, &b| scores[a].total_cmp(&scores[b]));
            idx.truncate(topk);
            if idx.is_empty() {
                break;
            }

            let mut round_best: Option<(f64, usize, TreeResult)> = None;
            for &i in &idx {
                let res = expand_with_choices(g, an, &req, &cand_choices[i]);
                st.evals += 1;
                let sc = score(&res, opts.ops_penalty);
                if round_best.as_ref().map(|(s, _, _)| sc < *s).unwrap_or(true) {
                    round_best = Some((sc, i, res));
                }
            }
            let (sc, best_i, res) = round_best.expect("top-K 非空");

            if debug {
                eprintln!(
                    "beam(batch) round {}: cands={} best={:.4} current={:.4}",
                    st.rounds,
                    cand_choices.len(),
                    sc,
                    st.best_score
                );
            }

            if sc < st.best_score - 1e-9 {
                st.best_score = sc;
                st.best = res;
                st.improved = true;
                st.frontier.clear();
                st.frontier.push(cand_choices[best_i].clone());
                for &i in idx
                    .iter()
                    .filter(|&&i| i != best_i)
                    .take(opts.beam_width.saturating_sub(1))
                {
                    st.frontier.push(cand_choices[i].clone());
                }
            } else {
                break;
            }
        }
    }

    // ---- CPU 采样兜底（保证不劣于纯 CPU 路径） ----
    st = cpu_local_search(g, an, &req, opts, st, debug);

    let mut notes = st.best.notes;
    if evaluator.as_deref().is_some() {
        notes.push(format!(
            "批量评估器 {}：粗筛 {} 个候选，CPU 精评 {} 个",
            evaluator_name, batch_evaluated, st.evals
        ));
    }
    notes.push(format!(
        "Beam 局部搜索：{} 轮，评估 {} 个候选{}",
        st.rounds,
        st.evals,
        if st.improved {
            ""
        } else {
            "（贪心基线已是最优）"
        }
    ));
    notes.push("原版配方无 GT 时长/耗电数据，机器数与 EU 仅统计 GT 配方".to_string());

    super::assemble_plan(
        g,
        an,
        target,
        rate_per_min,
        "beam",
        &st.best.ops,
        &st.best.raw,
        &st.best.byproducts,
        notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    )
}
