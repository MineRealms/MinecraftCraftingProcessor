//! 批量候选评估器：基于 GPU BiCGSTAB 线性求解。
//!
//! 对每份候选选择表构建物料平衡线性系统 `A x = rhs`（CSR），
//! 用 `GpuFlowSolver` 批量求解（每候选一个 workgroup），再按
//! `score = Σ 未满足缺口·权重 + λ·Σ x` 打分；未收敛/非有限解加罚。
//!
//! 得分用于 beam 局部搜索的粗筛（top-K 再交给 CPU 完整展开精评）。

use std::collections::HashMap;

use gt_planner_core::analysis::Analysis;
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::{MaterialId, RecipeId};
use gt_planner_core::planner::BatchEvaluator;

use crate::linear::{FlowSystem, GpuFlowSolver, GpuUnavailable, SolveResult};
use crate::subgraph::{build_subgraph_from_plan, CandidateTable, SubgraphData};

/// 每批候选数（控制显存占用）。
const BATCH: usize = 256;
/// 求解迭代上限与收敛容差（打分只需排序精度，不需要 1e-5）。
const MAX_ITERS: u32 = 300;
const TOL: f32 = 1e-3;
/// 未收敛候选的罚分（温和：仍有排序价值）。
const NON_CONVERGED_PENALTY: f32 = 1e3;
/// 解钳制上限（防止发散解污染评分）。
const X_CLAMP: f32 = 1e6;

/// GPU 批量评估器（持有评估子图与线性求解器）。
pub struct GpuEvaluator {
    solver: GpuFlowSolver,
    sg: Option<SubgraphData>,
    demand: Vec<f32>,
}

impl GpuEvaluator {
    /// 创建评估器（不绑定具体方案；`prepare` 时构建子图）。
    pub fn new() -> Result<Self, GpuUnavailable> {
        Ok(Self {
            solver: GpuFlowSolver::new()?,
            sg: None,
            demand: Vec::new(),
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.solver.adapter_name
    }

    /// 用基线方案构建评估子图。
    pub fn prepare_with_plan(
        &mut self,
        g: &KnowledgeGraph,
        an: &Analysis,
        target: MaterialId,
        rate_per_min: f64,
        choices: &HashMap<MaterialId, RecipeId>,
    ) {
        let sg = build_subgraph_from_plan(g, an, target, choices, 3);
        let mut demand = vec![0f32; sg.materials.len()];
        if let Some(&ti) = sg.mat_index.get(&target) {
            demand[ti] = rate_per_min as f32;
        }
        self.demand = demand;
        self.sg = Some(sg);
    }

    /// 求解一批候选并打分。
    fn evaluate_chunk(
        &self,
        sg: &SubgraphData,
        chunk: &[(usize, CandidateTable)],
    ) -> Vec<(usize, f64)> {
        let systems: Vec<FlowSystem> = chunk
            .iter()
            .map(|(_, t)| FlowSystem::from_candidate(sg, t, &self.demand))
            .collect();
        let results: Vec<SolveResult> = self.solver.solve_batch(&systems, MAX_ITERS, TOL);
        let mut out = Vec::with_capacity(chunk.len());
        for ((idx, _), (sys, res)) in chunk.iter().zip(systems.iter().zip(results.iter())) {
            // 钳制解（发散系统会产生巨大/非有限值），保留排序信息
            let mut bad = !res.converged;
            let x: Vec<f32> = res
                .x
                .iter()
                .map(|&v| {
                    if !v.is_finite() || v.abs() > X_CLAMP {
                        bad = true;
                        X_CLAMP
                    } else if v < 0.0 {
                        0.0
                    } else {
                        v
                    }
                })
                .collect();
            let mut s = sys.score(&x, &sg.norm_factor, 0.001);
            if !s.is_finite() {
                s = 1e9;
            } else if bad {
                s += NON_CONVERGED_PENALTY;
            }
            out.push((*idx, s as f64));
        }
        out
    }
}

impl BatchEvaluator for GpuEvaluator {
    fn prepare(
        &mut self,
        g: &KnowledgeGraph,
        an: &Analysis,
        target: MaterialId,
        rate_per_min: f64,
        choices: &HashMap<MaterialId, RecipeId>,
    ) {
        self.prepare_with_plan(g, an, target, rate_per_min, choices);
    }

    fn evaluate(&self, choices: &[HashMap<MaterialId, RecipeId>]) -> Vec<f64> {
        let Some(sg) = self.sg.as_ref() else {
            return vec![f64::INFINITY; choices.len()];
        };

        let mut out: Vec<f64> = vec![f64::INFINITY; choices.len()];
        let mut valid: Vec<(usize, CandidateTable)> = Vec::with_capacity(choices.len());
        for (i, ch) in choices.iter().enumerate() {
            if let Some(t) = CandidateTable::from_choices(sg, ch) {
                valid.push((i, t));
            }
        }
        if std::env::var_os("GTP_DEBUG_GPU").is_some() {
            eprintln!(
                "gpu evaluate: subgraph {} materials / {} recipes, candidates {} valid {}",
                sg.materials.len(),
                sg.recipes.len(),
                choices.len(),
                valid.len()
            );
        }

        for chunk in valid.chunks(BATCH) {
            for (idx, score) in self.evaluate_chunk(sg, chunk) {
                out[idx] = score;
            }
        }
        out
    }

    fn name(&self) -> String {
        format!("wgpu-bicgstab({})", self.solver.adapter_name)
    }
}
