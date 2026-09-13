//! gt-planner-core: JEI 配方 → 生产知识图谱 → 规划求解
//!
//! 三层 IR：
//! - Knowledge IR: `model` / `graph`（事实层）
//! - Search IR:    `algo`（算法层，M2）
//! - Plan IR:      `plan`（结果层，M3）

pub mod algo;
pub mod analysis;
pub mod chances;
pub mod error;
pub mod graph;
pub mod intern;
pub mod model;
pub mod names;
pub mod parser;
pub mod plan;
pub mod planner;
pub mod process;
pub mod raw;
pub mod solver;
pub mod util;

pub use analysis::Analysis;
pub use algo::{Condensation, CostVector, CostWeights};
pub use chances::ChanceStore;
pub use error::{Error, Result};
pub use graph::{DatasetMeta, GraphStats, KnowledgeGraph};
pub use model::{
    CategoryId, EnergyIo, GtRecipeInfo, IngredientDto, MaterialDto, MaterialId, MaterialKind,
    MaterialKey, RecipeId, RecipeNode, Slot, SlotDto,
};
pub use names::{NameEntry, NameStore};
pub use plan::{Plan, PlanEntry, PlanTotals, PlannedRecipe};
pub use planner::{
    greedy_choices, plan_beam, plan_beam_with_evaluator, plan_mcts, plan_tree, BatchEvaluator, BeamOptions,
    MctsOptions, PlanRequest,
};
pub use process::{ProcessFlow, ProcessGraph, ProcessStep};
pub use solver::{plan_exact, ExactOptions};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
