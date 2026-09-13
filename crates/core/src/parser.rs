//! 解析器：JEI JSON（Raw 层）→ Knowledge IR。
//!
//! 关键处理：
//! - 材料主键 = (kind, id, nbt)，NBT 参与身份
//! - 槽内 `ingredients` 为 OR 候选，空槽丢弃
//! - 建立 producers / consumers 邻接表（排序去重）
//! - 记录数据集元信息与统计

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::error::Result;
use crate::graph::{DatasetMeta, GraphStats, KnowledgeGraph};
use crate::intern::Interner;
use crate::model::{
    Catalyst, CategoryId, CategoryInfo, MaterialId, MaterialInfo, MaterialKey, MaterialKind,
    RecipeId, RecipeNode, Slot,
};
use crate::raw::{RawCategory, RawIngredient, RawRecipe, RawRoot, RawSlot};

/// 从文件加载并构建知识图谱。
///
/// 注意：当前为一次性全量解析（54MB 紧凑 JSON 实测可行），
/// 后续可替换为流式 Visitor 或 bincode 缓存。
pub fn load_file(path: &Path) -> Result<KnowledgeGraph> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let root: RawRoot = serde_json::from_reader(reader)?;
    build_from_root(root)
}

/// 从字符串加载（测试用）。
pub fn from_json_str(s: &str) -> Result<KnowledgeGraph> {
    let root: RawRoot = serde_json::from_str(s)?;
    build_from_root(root)
}

/// 从 Raw 根对象构建知识图谱。
pub fn build_from_root(root: RawRoot) -> Result<KnowledgeGraph> {
    let mut b = Builder::new();
    b.meta = DatasetMeta {
        format: root.format,
        version: root.version,
        minecraft_version: root.minecraft_version,
        exported_at: root.exported_at,
        include_hidden: root.include_hidden,
        summary_category_count: root.summary.as_ref().map(|s| s.category_count),
        summary_recipe_count: root.summary.as_ref().map(|s| s.recipe_count),
    };
    for cat in &root.categories {
        b.add_category(cat);
    }
    Ok(b.finish())
}

/// NBT 空串归一为 None。
fn norm_nbt(nbt: Option<&str>) -> Option<&str> {
    nbt.filter(|s| !s.is_empty())
}

/// 材料可读名：短 id，下划线转空格；含 NBT 追加标记。
fn make_display(id: &str, has_nbt: bool) -> String {
    let short = id.rsplit(':').next().unwrap_or(id);
    let mut s = short.replace('_', " ");
    if has_nbt {
        s.push_str(" [NBT]");
    }
    s
}

struct Builder {
    strings: Interner,
    materials: Vec<MaterialInfo>,
    index: HashMap<MaterialKey, MaterialId>,
    recipes: Vec<RecipeNode>,
    recipe_index: HashMap<(CategoryId, String), RecipeId>,
    categories: Vec<CategoryInfo>,
    producers: Vec<Vec<RecipeId>>,
    consumers: Vec<Vec<RecipeId>>,
    meta: DatasetMeta,
    stats: GraphStats,
}

impl Builder {
    fn new() -> Self {
        Self {
            strings: Interner::new(),
            materials: Vec::new(),
            index: HashMap::new(),
            recipes: Vec::new(),
            recipe_index: HashMap::new(),
            categories: Vec::new(),
            producers: Vec::new(),
            consumers: Vec::new(),
            meta: DatasetMeta::default(),
            stats: GraphStats::default(),
        }
    }

    /// 驻留一个材料，返回稠密 id。
    fn intern_material(&mut self, ty: &str, id: &str, nbt: Option<&str>) -> MaterialId {
        let kind = match ty {
            "item" => MaterialKind::Item,
            "fluid" => MaterialKind::Fluid,
            other => MaterialKind::Other(self.strings.intern(other)),
        };
        let id_i = self.strings.intern(id);
        let nbt_i = norm_nbt(nbt).map(|n| self.strings.intern(n));
        let key = MaterialKey {
            kind,
            id: id_i,
            nbt: nbt_i,
        };
        if let Some(&mid) = self.index.get(&key) {
            return mid;
        }
        let mid = self.materials.len() as MaterialId;
        self.materials.push(MaterialInfo {
            key,
            display: make_display(id, nbt_i.is_some()),
        });
        self.index.insert(key, mid);
        self.producers.push(Vec::new());
        self.consumers.push(Vec::new());

        match kind {
            MaterialKind::Item => self.stats.item_count += 1,
            MaterialKind::Fluid => self.stats.fluid_count += 1,
            MaterialKind::Other(_) => self.stats.other_count += 1,
        }
        if nbt_i.is_some() {
            self.stats.nbt_variant_count += 1;
        }
        mid
    }

    fn ingredient_material(&mut self, ing: &RawIngredient) -> (MaterialId, u64) {
        match ing.ty.as_str() {
            "item" => {
                let id = ing.id.as_deref().unwrap_or("unknown");
                let m = self.intern_material("item", id, ing.nbt.as_deref());
                (m, ing.count.unwrap_or(1))
            }
            "fluid" => {
                let id = ing.id.as_deref().unwrap_or("unknown");
                let m = self.intern_material("fluid", id, ing.nbt.as_deref());
                (m, ing.amount.unwrap_or(0))
            }
            other => {
                let value = ing
                    .value
                    .as_deref()
                    .or(ing.id.as_deref())
                    .unwrap_or("unknown");
                let m = self.intern_material(other, value, ing.nbt.as_deref());
                (m, 1)
            }
        }
    }

    /// 构建槽列表：空槽丢弃；槽内候选去重（保留首次出现）。
    fn build_slots(&mut self, raw_slots: &[RawSlot]) -> Vec<Slot> {
        let mut out = Vec::with_capacity(raw_slots.len());
        for rs in raw_slots {
            let mut alts: Vec<(MaterialId, u64)> = Vec::new();
            for ing in &rs.ingredients {
                let (mid, qty) = self.ingredient_material(ing);
                if qty == 0 {
                    continue;
                }
                if !alts.iter().any(|&(m, _)| m == mid) {
                    alts.push((mid, qty));
                }
            }
            if alts.is_empty() {
                continue;
            }
            out.push(Slot {
                name: rs.name.clone(),
                alts,
            });
        }
        out
    }

    fn add_category(&mut self, raw: &RawCategory) -> CategoryId {
        let cid = self.categories.len() as CategoryId;
        let mut cat = CategoryInfo {
            ty: raw.ty.clone(),
            title: raw.title.clone(),
            recipe_class: raw.recipe_class.clone(),
            catalysts: Vec::new(),
            recipe_count: raw.recipes.len(),
        };
        for c in &raw.catalysts {
            let m = self.intern_material(&c.ty, &c.id, None);
            cat.catalysts.push(Catalyst {
                material: m,
                count: c.count.unwrap_or(1),
            });
        }
        self.categories.push(cat);

        for (ri, rr) in raw.recipes.iter().enumerate() {
            self.add_recipe(cid, &raw.ty, ri, rr);
        }
        cid
    }

    fn add_recipe(
        &mut self,
        cid: CategoryId,
        cat_ty: &str,
        idx: usize,
        raw: &RawRecipe,
    ) -> RecipeId {
        let rid = self.recipes.len() as RecipeId;
        let inputs = self.build_slots(&raw.inputs);
        let outputs = self.build_slots(&raw.outputs);

        for slot in &inputs {
            for &(m, _) in &slot.alts {
                self.consumers[m as usize].push(rid);
            }
        }
        for slot in &outputs {
            for &(m, _) in &slot.alts {
                self.producers[m as usize].push(rid);
            }
        }

        let id = if raw.id.is_empty() {
            format!("{cat_ty}#{idx}")
        } else {
            raw.id.clone()
        };
        self.recipes.push(RecipeNode {
            category: cid,
            id: id.clone(),
            inputs,
            outputs,
        });
        self.recipe_index.insert((cid, id), rid);
        rid
    }

    fn finish(mut self) -> KnowledgeGraph {
        for v in self.producers.iter_mut().chain(self.consumers.iter_mut()) {
            v.sort_unstable();
            v.dedup();
        }
        let mut stats = self.stats;
        stats.material_count = self.materials.len();
        stats.recipe_count = self.recipes.len();
        stats.category_count = self.categories.len();
        stats.empty_recipe_count = self.recipes.iter().filter(|r| r.is_empty()).count();
        stats.consume_links = self.consumers.iter().map(Vec::len).sum();
        stats.produce_links = self.producers.iter().map(Vec::len).sum();
        stats.raw_material_count = self.producers.iter().filter(|v| v.is_empty()).count();
        stats.leaf_material_count = self.consumers.iter().filter(|v| v.is_empty()).count();

        KnowledgeGraph {
            strings: self.strings,
            materials: self.materials,
            recipes: self.recipes,
            categories: self.categories,
            index: self.index,
            recipe_index: self.recipe_index,
            producers: self.producers,
            consumers: self.consumers,
            meta: self.meta,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MaterialKind;

    const FIXTURE: &str = r#"{
      "format": "gtmfo_jei_recipes",
      "version": 1,
      "minecraft_version": "1.20.1",
      "exported_at": "2026-01-01T00:00:00Z",
      "include_hidden": false,
      "categories": [
        {
          "type": "test:chem",
          "title": "Test Chem",
          "recipe_class": "com.example.Recipe",
          "catalysts": [{"type": "item", "id": "test:machine", "count": 1}],
          "recipes": [
            {
              "id": "test:chem/a",
              "inputs": [
                {"name": "slot_0", "ingredients": [{"type": "item", "id": "test:ore", "count": 2}]},
                {"name": "slot_1", "ingredients": [{"type": "fluid", "id": "test:water", "amount": 1000}]},
                {"name": "slot_2", "ingredients": [
                  {"type": "item", "id": "test:tool", "count": 1, "nbt": "{Damage:1}"},
                  {"type": "item", "id": "test:tool", "count": 1, "nbt": "{Damage:2}"}
                ]},
                {"name": "slot_3", "ingredients": []}
              ],
              "outputs": [
                {"name": "slot_9", "ingredients": [{"type": "item", "id": "test:dust", "count": 1}]}
              ]
            },
            {"id": "test:chem/info", "inputs": [], "outputs": []}
          ]
        },
        {
          "type": "test:assembler",
          "title": "Test Assembler",
          "recipe_class": "com.example.Recipe",
          "catalysts": [],
          "recipes": [
            {
              "id": "",
              "inputs": [{"name": "slot_0", "ingredients": [{"type": "item", "id": "test:dust", "count": 4}]}],
              "outputs": [{"name": "slot_9", "ingredients": [{"type": "item", "id": "test:block", "count": 1}]}]
            }
          ]
        }
      ],
      "summary": {"category_count": 2, "recipe_count": 3}
    }"#;

    fn g() -> KnowledgeGraph {
        from_json_str(FIXTURE).unwrap()
    }

    #[test]
    fn builds_materials_and_stats() {
        let g = g();
        assert_eq!(g.stats.material_count, 7, "machine/ore/water/tool1/tool2/dust/block");
        assert_eq!(g.stats.item_count, 6);
        assert_eq!(g.stats.fluid_count, 1);
        assert_eq!(g.stats.nbt_variant_count, 2);
        assert_eq!(g.stats.recipe_count, 3);
        assert_eq!(g.stats.category_count, 2);
        assert_eq!(g.stats.empty_recipe_count, 1);
        assert_eq!(g.stats.consume_links, 5);
        assert_eq!(g.stats.produce_links, 2);
        assert_eq!(g.stats.raw_material_count, 5);
        assert_eq!(g.stats.leaf_material_count, 2);
    }

    #[test]
    fn nbt_is_part_of_identity() {
        let g = g();
        let t1 = g
            .find_material(MaterialKind::Item, "test:tool", Some("{Damage:1}"))
            .unwrap();
        let t2 = g
            .find_material(MaterialKind::Item, "test:tool", Some("{Damage:2}"))
            .unwrap();
        assert_ne!(t1, t2);
        // 无 NBT 的 test:tool 在本数据集中不存在，不能误匹配到 NBT 变体
        assert!(g.find_material(MaterialKind::Item, "test:tool", None).is_none());
        assert!(g.display(t1).contains("[NBT]"));
    }

    #[test]
    fn empty_slots_dropped() {
        let g = g();
        let rid = *g
            .recipe_index
            .get(&(0, "test:chem/a".to_string()))
            .unwrap();
        assert_eq!(g.recipe(rid).inputs.len(), 3);
    }

    #[test]
    fn fallback_recipe_id() {
        let g = g();
        let rid = *g
            .recipe_index
            .get(&(1, "test:assembler#0".to_string()))
            .unwrap();
        assert_eq!(g.recipe(rid).id, "test:assembler#0");
    }

    #[test]
    fn or_slot_has_two_candidates() {
        let g = g();
        let rid = *g
            .recipe_index
            .get(&(0, "test:chem/a".to_string()))
            .unwrap();
        let slot = &g.recipe(rid).inputs[2];
        assert_eq!(slot.alts.len(), 2);
    }

    #[test]
    fn producer_consumer_index() {
        let g = g();
        let dust = g
            .find_material(MaterialKind::Item, "test:dust", None)
            .unwrap();
        assert_eq!(g.producers[dust as usize].len(), 1);
        assert_eq!(g.consumers[dust as usize].len(), 1);
        assert!(!g.is_raw(dust));
        let ore = g
            .find_material(MaterialKind::Item, "test:ore", None)
            .unwrap();
        assert!(g.is_raw(ore));
    }

    #[test]
    fn search_finds_material() {
        let g = g();
        let hits = g.search("block", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(g.material_id_str(hits[0]), "test:block");
    }
}
