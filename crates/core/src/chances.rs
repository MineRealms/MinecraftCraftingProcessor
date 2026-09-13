//! 概率产出覆盖层（JEI 不导出 chanced output 概率，由外部文件补充）。
//!
//! 文件格式：
//! ```json
//! {
//!   "format": "gtmfo_chances",
//!   "version": 1,
//!   "chances": [
//!     { "recipe": "gtceu:chemical_reactor/xxx", "output": "gtceu:tiny_titanium_dust", "chance": 0.3 }
//!   ]
//! }
//! ```
//!
//! - `recipe` 用完整配方名（`<分类 type>/<配方 id>`，与 Plan 中的 recipe 字段一致）
//! - `output` 用材料 id（含 NBT 时可用 `id#nbt` 前缀匹配，当前按 id 精确匹配）
//! - 未覆盖的产出默认概率 1.0（期望值 = 数量 × 概率）

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Deserialize;

use crate::error::Result;

/// 概率覆盖表。
#[derive(Debug, Clone, Default)]
pub struct ChanceStore {
    map: HashMap<(String, String), f64>,
}

#[derive(Debug, Deserialize)]
struct RawChances {
    #[serde(default)]
    chances: Vec<RawChance>,
}

#[derive(Debug, Deserialize)]
struct RawChance {
    recipe: String,
    output: String,
    chance: f64,
}

impl ChanceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从文件加载。
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::with_capacity(1 << 20, file);
        let raw: RawChances = serde_json::from_reader(reader)?;
        let mut map = HashMap::with_capacity(raw.chances.len());
        for c in raw.chances {
            if c.chance.is_finite() && (0.0..=1.0).contains(&c.chance) {
                map.insert((c.recipe, c.output), c.chance);
            }
        }
        Ok(Self { map })
    }

    pub fn get(&self, recipe_full_id: &str, material_id: &str) -> f64 {
        self.map
            .get(&(recipe_full_id.to_string(), material_id.to_string()))
            .copied()
            .unwrap_or(1.0)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
