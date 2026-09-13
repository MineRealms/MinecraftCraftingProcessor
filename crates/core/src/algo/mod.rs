//! Search IR 算法层：SCC 压缩 / 启发式成本 / 支配剪枝。

pub mod cost;
pub mod prune;
pub mod scc;

pub use cost::CostDb;
pub use scc::SccResult;
