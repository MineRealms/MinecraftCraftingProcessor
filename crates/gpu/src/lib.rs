//! gt-planner-gpu：wgpu 批量候选评估。
//!
//! 用途：beam 局部搜索的"大规模候选评估"加速。
//! 给定子图 + 一批候选配方选择表，GPU 并行求解每个候选的
//! 物料平衡定点迭代（Gauss-Seidel 缩放），输出候选得分。

pub mod eval;
pub mod subgraph;

pub use eval::{GpuEvaluator, GpuUnavailable};
pub use subgraph::{CandidateTable, SubgraphData};
