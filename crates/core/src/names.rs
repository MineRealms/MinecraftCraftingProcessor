//! jei_names.json：id → 中英文名（机器/材料/方块/物品/流体）。
//!
//! 注意：join 用 **id**，不要用 key（同一 tagprefix 的 key 会被多种材料共享）。

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Deserialize;

use crate::error::Result;

/// 名称条目。
#[derive(Debug, Clone, Default)]
pub struct NameEntry {
    pub en: String,
    pub zh: String,
    /// 机器专用：等级名（LV/MV/…）
    pub tier: Option<String>,
    /// 机器专用：等级序号
    pub tier_index: Option<u8>,
}

/// 名称库。
#[derive(Debug, Clone, Default)]
pub struct NameStore {
    map: HashMap<String, NameEntry>,
}

#[derive(Debug, Deserialize)]
struct RawNames {
    #[serde(default)]
    machines: Vec<RawName>,
    #[serde(default)]
    materials: Vec<RawName>,
    #[serde(default)]
    blocks: Vec<RawName>,
    #[serde(default)]
    items: Vec<RawName>,
    #[serde(default)]
    fluids: Vec<RawName>,
}

#[derive(Debug, Deserialize)]
struct RawName {
    id: String,
    #[serde(default)]
    en: String,
    #[serde(default)]
    zh: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    tier_index: Option<u8>,
}

impl NameStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从文件加载。
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::with_capacity(1 << 20, file);
        let raw: RawNames = serde_json::from_reader(reader)?;
        let mut map: HashMap<String, NameEntry> = HashMap::with_capacity(
            raw.machines.len() + raw.materials.len() + raw.blocks.len() + raw.items.len()
                + raw.fluids.len(),
        );
        let mut insert = |items: Vec<RawName>, with_tier: bool| {
            for n in items {
                let e = NameEntry {
                    en: n.en,
                    zh: n.zh,
                    tier: if with_tier { n.tier } else { None },
                    tier_index: if with_tier { n.tier_index } else { None },
                };
                map.insert(n.id, e);
            }
        };
        insert(raw.blocks, false);
        insert(raw.items, false);
        insert(raw.fluids, false);
        insert(raw.materials, false);
        insert(raw.machines, true); // 机器最后插入，覆盖同名方块条目（带 tier）
        Ok(Self { map })
    }

    pub fn get(&self, id: &str) -> Option<&NameEntry> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 中文名（缺失时回退到给定 display）。
    pub fn zh_or<'a>(&'a self, id: &str, fallback: &'a str) -> &'a str {
        match self.map.get(id) {
            Some(e) if !e.zh.is_empty() => &e.zh,
            _ => fallback,
        }
    }

    /// 英文名（缺失时回退到给定 display）。
    pub fn en_or<'a>(&'a self, id: &str, fallback: &'a str) -> &'a str {
        match self.map.get(id) {
            Some(e) if !e.en.is_empty() => &e.en,
            _ => fallback,
        }
    }
}
