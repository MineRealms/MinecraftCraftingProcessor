//! Search IR 算法层：SCC 压缩 / 凝聚图 / 启发式成本 / 支配剪枝。

pub mod condensation;
pub mod cost;
pub mod prune;
pub mod scc;

pub use condensation::Condensation;
pub use cost::{CostDb, CostVector, CostWeights};
pub use scc::SccResult;
