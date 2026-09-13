//! Search IR 聚合层：一次构建 SCC / 凝聚图 / 成本数据库 / 支配剪枝，供规划器复用。

use std::time::Instant;

use crate::algo::condensation::Condensation;
use crate::algo::cost::{self, CostDb, CostWeights};
use crate::algo::prune;
use crate::algo::scc::{self, SccResult};
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};

/// 预计算分析结果（Search IR）。
pub struct Analysis {
    pub scc: SccResult,
    /// SCC 凝聚图（循环系统作为一级公民）。
    pub cond: Condensation,
    pub cost: CostDb,
    /// `dominated_for[recipe]`：该配方被支配的产物列表（升序）。
    pub dominated_for: Vec<Vec<MaterialId>>,
    pub build_ms: f64,
}

impl Analysis {
    /// 默认权重（balanced）构建。
    pub fn build(g: &KnowledgeGraph) -> Self {
        Self::build_with(g, 48, CostWeights::default())
    }

    /// 指定权重构建（成本向量路线随权重变化）。
    pub fn build_with(g: &KnowledgeGraph, cost_iters: usize, weights: CostWeights) -> Self {
        let t0 = Instant::now();
        let (n, adj) = scc::build_adjacency(g);
        let scc = scc::tarjan(n, &adj);
        let cond = Condensation::build(g, &scc);
        let cost = cost::build(g, &cond, cost_iters, weights);
        let dominated_for = prune::dominance_pruning(g, &cost.unit_cost);
        let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
        Self {
            scc,
            cond,
            cost,
            dominated_for,
            build_ms,
        }
    }

    /// 配方 `rid` 对产物 `m` 是否被支配。
    pub fn is_dominated_for(&self, rid: RecipeId, m: MaterialId) -> bool {
        self.dominated_for[rid as usize].binary_search(&m).is_ok()
    }

    /// 材料是否处于生产循环中。
    pub fn material_cyclic(&self, m: MaterialId) -> bool {
        self.cost.cyclic[m as usize]
    }
}
