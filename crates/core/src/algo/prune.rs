//! 支配剪枝：同产物配方中，输入向量逐分量 ≤ 另一条、且至少一个分量严格更小的
//! 配方"支配"另一条（被支配者不可能是更优路线）。
//!
//! 注意：支配关系是**按产物**记录的——一条配方可能对产物 A 被支配，
//! 但对产物 B 仍然唯一可用，因此不能全局删除。

use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::util::{cheapest_alt, mat_norm_qty, output_qty_of};

/// 返回 `dominated_for[recipe] = 该配方被支配的材料列表`（按材料 id 升序）。
pub fn dominance_pruning(g: &KnowledgeGraph, unit_cost: &[f64]) -> Vec<Vec<MaterialId>> {
    let mut dominated_for: Vec<Vec<MaterialId>> = vec![Vec::new(); g.recipes.len()];

    for m in 0..g.materials.len() {
        let prods = &g.producers[m];
        if prods.len() < 2 || prods.len() > 256 {
            continue;
        }

        // 每个生产配方：归一化到"每单位 m"的最便宜输入向量
        let mut specs: Vec<(RecipeId, Vec<(MaterialId, f64)>)> = Vec::with_capacity(prods.len());
        for &rid in prods {
            let r = g.recipe(rid);
            // 信息页 / 无输入配方不参与支配比较
            if !g.is_plannable(rid) || r.inputs.is_empty() {
                continue;
            }
            let Some(q) = output_qty_of(r, m as MaterialId) else {
                continue;
            };
            let qn = mat_norm_qty(g, m as MaterialId, q);
            if qn <= 0.0 {
                continue;
            }
            let mut vec: Vec<(MaterialId, f64)> = Vec::with_capacity(r.inputs.len());
            let mut usable = true;
            for slot in &r.inputs {
                match cheapest_alt(g, unit_cost, None, slot) {
                    Some((am, aq)) => vec.push((am, mat_norm_qty(g, am, aq) / qn)),
                    None => {
                        usable = false;
                        break;
                    }
                }
            }
            if !usable {
                continue;
            }
            vec.sort_unstable_by_key(|&(mm, _)| mm);
            let mut merged: Vec<(MaterialId, f64)> = Vec::with_capacity(vec.len());
            for (mm, qty) in vec {
                if let Some(last) = merged.last_mut() {
                    if last.0 == mm {
                        last.1 += qty;
                        continue;
                    }
                }
                merged.push((mm, qty));
            }
            specs.push((rid, merged));
        }

        for i in 0..specs.len() {
            for j in 0..specs.len() {
                if i == j {
                    continue;
                }
                if dominates(&specs[i].1, &specs[j].1) {
                    dominated_for[specs[j].0 as usize].push(m as MaterialId);
                }
            }
        }
    }

    dominated_for
}

/// A 是否严格支配 B：A 每个输入分量 ≤ B，且至少一个分量严格 <。
/// 约定 `a`、`b` 按材料 id 升序。
fn dominates(a: &[(MaterialId, f64)], b: &[(MaterialId, f64)]) -> bool {
    let mut i = 0usize;
    let mut j = 0usize;
    let mut strict = false;
    while i < a.len() {
        while j < b.len() && b[j].0 < a[i].0 {
            // b 多出的输入：a 视为 0 → 严格更省
            strict = true;
            j += 1;
        }
        if j >= b.len() || b[j].0 != a[i].0 {
            // a 有 b 没有的输入 → a 不支配 b
            return false;
        }
        if a[i].1 > b[j].1 + 1e-12 {
            return false;
        }
        if a[i].1 < b[j].1 - 1e-12 {
            strict = true;
        }
        i += 1;
        j += 1;
    }
    if j < b.len() {
        strict = true;
    }
    strict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::scc::{build_adjacency, tarjan};
    use crate::model::MaterialKind;
    use crate::parser::from_json_str;

    const FIXTURE: &str = r#"{
      "format": "test", "version": 1, "minecraft_version": "1.20.1",
      "categories": [
        {
          "type": "test:craft", "title": "Craft", "catalysts": [],
          "recipes": [
            {
              "id": "test:craft/good",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 2}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:dust", "count": 1}]}]
            },
            {
              "id": "test:craft/bad",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 3}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:dust", "count": 1}]}]
            },
            {
              "id": "test:craft/special",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:special_ore", "count": 1}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:dust", "count": 1}]}]
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn marks_dominated_recipe() {
        let g = from_json_str(FIXTURE).unwrap();
        let (n, adj) = build_adjacency(&g);
        let scc = tarjan(n, &adj);
        let db = crate::algo::cost::build(&g, &scc, 32);
        let dom = dominance_pruning(&g, &db.unit_cost);

        let good = g.recipe_index[&(0, "test:craft/good".to_string())];
        let bad = g.recipe_index[&(0, "test:craft/bad".to_string())];
        let special = g.recipe_index[&(0, "test:craft/special".to_string())];
        let dust = g.find_material(MaterialKind::Item, "test:dust", None).unwrap();

        assert!(dom[bad as usize].contains(&dust), "bad 应被 good 支配");
        assert!(dom[good as usize].is_empty());
        assert!(dom[special as usize].is_empty(), "特殊路线不可比较");
    }
}
