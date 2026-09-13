//! Knowledge IR：材料 / 配方 / 分类的事实图谱 + 生产者/消费者索引。

use std::collections::HashMap;

use serde::Serialize;

use crate::intern::Interner;
use crate::model::{
    CategoryId, CategoryInfo, IngredientDto, MaterialDto, MaterialId, MaterialInfo, MaterialKey,
    MaterialKind, RecipeId, RecipeNode, Slot, SlotDto,
};
use crate::names::NameStore;

/// 数据集元信息（来自 JSON 头部）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DatasetMeta {
    pub format: String,
    pub version: u32,
    pub minecraft_version: String,
    pub exported_at: String,
    pub include_hidden: bool,
    pub summary_category_count: Option<u64>,
    pub summary_recipe_count: Option<u64>,
}

/// 图谱统计。
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphStats {
    pub material_count: usize,
    pub item_count: usize,
    pub fluid_count: usize,
    pub other_count: usize,
    pub nbt_variant_count: usize,
    pub recipe_count: usize,
    pub empty_recipe_count: usize,
    pub category_count: usize,
    /// 材料→配方 消耗链接数（槽候选展开后去重）。
    pub consume_links: usize,
    /// 配方→材料 产出链接数（槽候选展开后去重）。
    pub produce_links: usize,
    /// 无生产者材料（最终原料）。
    pub raw_material_count: usize,
    /// 无消费者材料（最终产物）。
    pub leaf_material_count: usize,
    /// 可参与规划的配方数（排除 diagram 类信息页）。
    pub plannable_recipe_count: usize,
    /// 可"采集"材料数（存在无输入配方，如空气/水收集）。
    pub harvestable_material_count: usize,
}

/// 生产知识图谱（Knowledge IR 的容器）。
///
/// 布局选择：`materials` / `recipes` 稠密数组 + `producers` / `consumers`
/// 邻接表，全部按 id 排序去重，保证查询确定性与缓存友好。
pub struct KnowledgeGraph {
    pub strings: Interner,
    pub materials: Vec<MaterialInfo>,
    pub recipes: Vec<RecipeNode>,
    pub categories: Vec<CategoryInfo>,
    pub index: HashMap<MaterialKey, MaterialId>,
    pub recipe_index: HashMap<(CategoryId, String), RecipeId>,
    /// producers[m] = 产出材料 m 的配方（已排序去重）。
    pub producers: Vec<Vec<RecipeId>>,
    /// consumers[m] = 消耗材料 m 的配方（已排序去重）。
    pub consumers: Vec<Vec<RecipeId>>,
    /// 配方是否可参与规划（false = diagram 类信息页，规划时忽略）。
    pub plannable: Vec<bool>,
    /// 材料是否可"采集"（存在无输入的可规划配方，如空气/水收集）。
    pub harvestable: Vec<bool>,
    /// 名称库（jei_names.json；可为空）。
    pub names: NameStore,
    pub meta: DatasetMeta,
    pub stats: GraphStats,
}

impl KnowledgeGraph {
    pub fn material(&self, id: MaterialId) -> &MaterialInfo {
        &self.materials[id as usize]
    }

    pub fn recipe(&self, id: RecipeId) -> &RecipeNode {
        &self.recipes[id as usize]
    }

    pub fn category(&self, id: CategoryId) -> &CategoryInfo {
        &self.categories[id as usize]
    }

    pub fn kind_str(&self, kind: MaterialKind) -> &str {
        match kind {
            MaterialKind::Item => "item",
            MaterialKind::Fluid => "fluid",
            MaterialKind::Other(s) => self.strings.get(s),
        }
    }

    pub fn material_id_str(&self, id: MaterialId) -> &str {
        self.strings.get(self.material(id).key.id)
    }

    pub fn display(&self, id: MaterialId) -> &str {
        &self.material(id).display
    }

    /// 是否有配方能生产该材料。
    pub fn is_raw(&self, id: MaterialId) -> bool {
        self.producers[id as usize].is_empty()
    }

    /// 配方是否可参与规划。
    pub fn is_plannable(&self, id: RecipeId) -> bool {
        self.plannable[id as usize]
    }

    /// 配方是否属于"回收类"（拆解成品/工具回炉，锚点选择时降优先级）。
    pub fn is_recycling(&self, id: RecipeId) -> bool {
        let r = self.recipe(id);
        self.category(r.category).ty.contains("recycling")
    }

    /// 完整配方名：配方 id 已含分类前缀时直接返回，否则补 `<分类 type>/`。
    pub fn recipe_full_id(&self, id: RecipeId) -> String {
        let r = self.recipe(id);
        let cat = &self.category(r.category).ty;
        if r.id.starts_with(cat.as_str()) {
            r.id.clone()
        } else {
            format!("{}/{}", cat, r.id)
        }
    }

    /// 按精确键查找材料。
    pub fn find_material(
        &self,
        kind: MaterialKind,
        id: &str,
        nbt: Option<&str>,
    ) -> Option<MaterialId> {
        let id_i = self.strings.lookup(id)?;
        let nbt_i = match nbt {
            Some(n) => Some(self.strings.lookup(n)?),
            None => None,
        };
        self.index
            .get(&MaterialKey {
                kind,
                id: id_i,
                nbt: nbt_i,
            })
            .copied()
    }

    /// 模糊搜索：大小写不敏感的子串匹配（完整 id 或可读名）。
    /// 结果按（短名长度, id）排序，短名优先。
    pub fn search(
        &self,
        query: &str,
        kind_filter: Option<MaterialKind>,
        limit: usize,
    ) -> Vec<MaterialId> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(MaterialId, usize, String)> = Vec::new();
        for (i, m) in self.materials.iter().enumerate() {
            if let Some(k) = kind_filter {
                if m.key.kind != k {
                    continue;
                }
            }
            let full = self.strings.get(m.key.id);
            let display = m.display.to_lowercase();
            if full.to_lowercase().contains(&q) || display.contains(&q) {
                let short = full.rsplit(':').next().unwrap_or(full).to_string();
                hits.push((i as MaterialId, short.len(), short));
            }
        }
        hits.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
        hits.truncate(limit);
        hits.into_iter().map(|(id, _, _)| id).collect()
    }

    // ------------------------------------------------------------------
    // DTO 转换
    // ------------------------------------------------------------------

    pub fn material_dto(&self, id: MaterialId) -> MaterialDto {
        let info = self.material(id);
        let full = self.strings.get(info.key.id);
        MaterialDto {
            kind: self.kind_str(info.key.kind).to_string(),
            id: full.to_string(),
            nbt: info.key.nbt.map(|n| self.strings.get(n).to_string()),
            display: info.display.clone(),
            display_zh: self.names.zh_or(full, &info.display).to_string(),
            display_en: self.names.en_or(full, &info.display).to_string(),
        }
    }

    pub fn ingredient_dto(&self, id: MaterialId, qty: u64) -> IngredientDto {
        let info = self.material(id);
        let (count, amount, value) = match info.key.kind {
            MaterialKind::Item => (Some(qty), None, None),
            MaterialKind::Fluid => (None, Some(qty), None),
            MaterialKind::Other(_) => (None, None, Some(qty.to_string())),
        };
        IngredientDto {
            kind: self.kind_str(info.key.kind).to_string(),
            id: self.strings.get(info.key.id).to_string(),
            nbt: info.key.nbt.map(|n| self.strings.get(n).to_string()),
            count,
            amount,
            value,
            display: info.display.clone(),
        }
    }

    pub fn slot_dto(&self, slot: &Slot) -> SlotDto {
        SlotDto {
            name: slot.name.clone(),
            ingredients: slot
                .alts
                .iter()
                .map(|&(m, q)| self.ingredient_dto(m, q))
                .collect(),
        }
    }
}
