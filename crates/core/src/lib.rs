//! gt-planner-core: JEI 配方 → 生产知识图谱 → 规划求解
//!
//! 三层 IR：
//! - Knowledge IR: `model` / `graph`（事实层）
//! - Search IR:    `algo`（算法层）
//! - Plan IR:      `plan`（结果层）

pub mod error;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
