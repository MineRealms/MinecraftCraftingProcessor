//! JEI JSON 的 serde 映射（Raw 层）。
//!
//! 只做结构映射，不做语义处理；语义在 `parser` 中完成。

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RawRoot {
    pub format: String,
    pub version: u32,
    pub minecraft_version: String,
    #[serde(default)]
    pub exported_at: String,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub categories: Vec<RawCategory>,
    #[serde(default)]
    pub summary: Option<RawSummary>,
}

#[derive(Debug, Deserialize)]
pub struct RawSummary {
    #[serde(default)]
    pub category_count: u64,
    #[serde(default)]
    pub recipe_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct RawCategory {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub recipe_class: Option<String>,
    #[serde(default)]
    pub catalysts: Vec<RawCatalyst>,
    #[serde(default)]
    pub recipes: Vec<RawRecipe>,
}

#[derive(Debug, Deserialize)]
pub struct RawCatalyst {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct RawRecipe {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub inputs: Vec<RawSlot>,
    #[serde(default)]
    pub outputs: Vec<RawSlot>,
    #[serde(default)]
    pub gt: Option<RawGt>,
}

/// GT 配方需求块（GTCEu API 导出）。
#[derive(Debug, Deserialize)]
pub struct RawGt {
    #[serde(default)]
    pub recipe_type: Option<String>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub parallels: u32,
    #[serde(default)]
    pub oc_level: u32,
    #[serde(default)]
    pub eut: Option<f64>,
    #[serde(default)]
    pub amperage: Option<f64>,
    #[serde(default)]
    pub energy_io: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub tier_index: Option<u8>,
    #[serde(default)]
    pub voltage: Option<f64>,
    #[serde(default)]
    pub total_eu_t: Option<f64>,
    #[serde(default)]
    pub total_eu: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct RawSlot {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ingredients: Vec<RawIngredient>,
}

/// 原料三形态：
/// - `{type:"item",  id, count}`
/// - `{type:"fluid", id, amount}`
/// - `{type:"<其他>", value}`（兜底）
/// 任意形态都可带 `nbt`（SNBT 字符串）。
#[derive(Debug, Deserialize)]
pub struct RawIngredient {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub amount: Option<u64>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub nbt: Option<String>,
}
