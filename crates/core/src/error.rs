use thiserror::Error;

/// 核心库统一错误类型。
#[derive(Debug, Error)]
pub enum Error {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("数据格式错误: {0}")]
    Format(String),

    #[error("未找到材料: {0}")]
    MaterialNotFound(String),

    #[error("未找到配方: {0}")]
    RecipeNotFound(String),

    #[error("规划失败: {0}")]
    Planning(String),
}

pub type Result<T> = std::result::Result<T, Error>;
