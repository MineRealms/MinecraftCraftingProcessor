//! gt-planner-core: JEI 配方 → 生产知识图谱 → 规划求解
//!
//! 三层 IR：
//! - Knowledge IR: `model` / `graph`（事实层）
//! - Search IR:    `algo`（算法层，M2）
//! - Plan IR:      `plan`（结果层，M3）

pub mod error;
pub mod graph;
pub mod intern;
pub mod model;
pub mod parser;
pub mod raw;

pub use error::{Error, Result};
pub use graph::{DatasetMeta, GraphStats, KnowledgeGraph};
pub use model::{
    CategoryId, IngredientDto, MaterialDto, MaterialId, MaterialKind, MaterialKey, RecipeId,
    RecipeNode, Slot, SlotDto,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
