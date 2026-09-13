//! MCTS（蒙特卡洛树搜索）规划器。
//!
//! 与 beam 的区别：beam 保留 top-K 并做贪婪下降；MCTS 用 UCT 平衡
//! "利用（当前最优子树）"与"探索（欠采样动作）"，更适合大搜索空间。
//!
//! 搜索空间：配方分配（材料 → 配方）。
//! - 节点 = 一份选择表；动作 = 把某材料的配方换成 top-N 替代之一；
//! - 评估（rollout）= 完整展开（`expand_tree`），得分越低越好；
//! - 选择 = UCT：`mean_reward + c·sqrt(ln(parent.visits)/visits)`，
//!   reward = −score；
//! - 迭代：选择 → 扩展一个未尝试动作 → 评估 → 回传。
//!
//! 保证不劣于 tree 基线（根节点即基线）。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::analysis::Analysis;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::plan::Plan;
use crate::planner::beam::recipe_alternatives;
use crate::planner::tree::{expand_with_choices, PlanRequest, TreeResult};

/// MCTS 参数。
#[derive(Debug, Clone)]
pub struct MctsOptions {
    /// 模拟（节点评估）次数。
    pub iterations: usize,
    /// UCT 探索常数 c。
    pub exploration: f64,
    /// 每个材料的替代配方数（动作空间）。
    pub candidate_limit: usize,
    /// 最大电压等级。
    pub max_tier: Option<u8>,
}

impl Default for MctsOptions {
    fn default() -> Self {
        Self {
            iterations: 400,
            exploration: 0.7,
            candidate_limit: 3,
            max_tier: None,
        }
    }
}

fn score(res: &TreeResult, ops_penalty: f64) -> f64 {
    res.estimated_cost + ops_penalty * res.ops_total
}

struct Node {
    choices: HashMap<MaterialId, RecipeId>,
    result: Option<TreeResult>,
    score: f64,
    visits: u32,
    total_reward: f64,
    parent: Option<usize>,
    children: Vec<usize>,
    untried: Vec<(MaterialId, RecipeId)>,
    untried_built: bool,
}

impl Node {
    fn mean_reward(&self) -> f64 {
        if self.visits == 0 {
            0.0
        } else {
            self.total_reward / self.visits as f64
        }
    }
}

/// 构建未尝试动作列表（确定性顺序）。
fn build_untried(
    g: &KnowledgeGraph,
    an: &Analysis,
    node: &Node,
    candidate_limit: usize,
    max_tier: Option<u8>,
) -> Vec<(MaterialId, RecipeId)> {
    let mut keys: Vec<MaterialId> = node.choices.keys().copied().collect();
    keys.sort_unstable();
    let mut out = Vec::new();
    for m in keys {
        let Some(&cur) = node.choices.get(&m) else {
            continue;
        };
        for alt in recipe_alternatives(g, an, m, Some(cur), candidate_limit, max_tier) {
            out.push((m, alt));
        }
    }
    out
}

/// MCTS 规划。
pub fn plan_mcts(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    rate_per_min: f64,
    opts: &MctsOptions,
) -> Plan {
    let t0 = Instant::now();
    let req = PlanRequest {
        target,
        rate_per_min,
        max_ops: 200_000,
        max_tier: opts.max_tier,
    };
    let ops_penalty = 0.001;
    let debug = std::env::var_os("GTP_MCTS_DEBUG").is_some();

    // 根 = tree 基线
    let empty: HashMap<MaterialId, RecipeId> = HashMap::new();
    let baseline = expand_with_choices(g, an, &req, &empty);
    let baseline_score = score(&baseline, ops_penalty);

    let mut nodes: Vec<Node> = Vec::new();
    nodes.push(Node {
        choices: baseline.choices.clone(),
        score: baseline_score,
        result: Some(baseline),
        visits: 0,
        total_reward: 0.0,
        parent: None,
        children: Vec::new(),
        untried: Vec::new(),
        untried_built: false,
    });

    let mut best_idx = 0usize;
    let mut best_score = baseline_score;
    let mut evaluations = 0usize;
    let mut expanded = 0usize;

    for _iter in 0..opts.iterations {
        // ---- 1) 选择 ----
        let mut cur = 0usize;
        loop {
            let node = &nodes[cur];
            if !node.untried_built && node.children.is_empty() {
                // 惰性构建未尝试动作
                let untried = build_untried(g, an, node, opts.candidate_limit, opts.max_tier);
                nodes[cur].untried = untried;
                nodes[cur].untried_built = true;
            }
            if !nodes[cur].untried.is_empty() {
                break; // 可扩展
            }
            if nodes[cur].children.is_empty() {
                break; // 叶子（无动作）
            }
            // UCT
            let parent_visits = nodes[cur].visits.max(1) as f64;
            let mut best_child = nodes[cur].children[0];
            let mut best_uct = f64::NEG_INFINITY;
            for &ch in &nodes[cur].children {
                let c = &nodes[ch];
                let uct = if c.visits == 0 {
                    f64::INFINITY
                } else {
                    c.mean_reward()
                        + opts.exploration * (parent_visits.ln() / c.visits as f64).sqrt()
                };
                if uct > best_uct {
                    best_uct = uct;
                    best_child = ch;
                }
            }
            cur = best_child;
        }

        // ---- 2) 扩展 ----
        let new_idx = if nodes[cur].untried.is_empty() {
            // 无可扩展动作：直接评估当前节点（回传）
            cur
        } else {
            let (m, alt) = nodes[cur].untried.remove(0);
            let mut choices = nodes[cur].choices.clone();
            choices.insert(m, alt);
            let res = expand_with_choices(g, an, &req, &choices);
            evaluations += 1;
            let sc = score(&res, ops_penalty);
            let idx = nodes.len();
            nodes.push(Node {
                choices,
                score: sc,
                result: Some(res),
                visits: 0,
                total_reward: 0.0,
                parent: Some(cur),
                children: Vec::new(),
                untried: Vec::new(),
                untried_built: false,
            });
            nodes[cur].children.push(idx);
            expanded += 1;
            idx
        };

        // ---- 3) 评估 + 回传 ----
        let sc = nodes[new_idx].score;
        if sc < best_score - 1e-9 {
            best_score = sc;
            best_idx = new_idx;
        }
        let reward = -sc;
        let mut walk = Some(new_idx);
        while let Some(i) = walk {
            nodes[i].visits += 1;
            nodes[i].total_reward += reward;
            walk = nodes[i].parent;
        }

        if debug && _iter % 50 == 0 {
            eprintln!(
                "mcts iter {}: nodes={} evals={} best={:.4} baseline={:.4}",
                _iter,
                nodes.len(),
                evaluations,
                best_score,
                baseline_score
            );
        }
    }

    let best = nodes[best_idx]
        .result
        .take()
        .expect("最优节点必须有评估结果");
    let mut notes = best.notes;
    notes.push(format!(
        "MCTS：{} 次模拟 / {} 个节点，最优得分 {:.4}（基线 {:.4}{}）",
        opts.iterations,
        expanded,
        best_score,
        baseline_score,
        if best_score < baseline_score - 1e-9 {
            ""
        } else {
            "，未改进"
        }
    ));

    super::assemble_plan(
        g,
        an,
        target,
        rate_per_min,
        "mcts",
        &best.ops,
        &best.raw,
        &best.byproducts,
        notes,
        t0.elapsed().as_secs_f64() * 1000.0,
    )
}

/// 去重辅助（保留给未来多根搜索）。
#[allow(dead_code)]
fn dedup_actions(actions: Vec<(MaterialId, RecipeId)>) -> Vec<(MaterialId, RecipeId)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in actions {
        if seen.insert(a) {
            out.push(a);
        }
    }
    out
}
