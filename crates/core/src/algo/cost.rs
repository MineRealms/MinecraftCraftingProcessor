//! 启发式成本数据库。
//!
//! 成本计算采用**SCC 拓扑序 + 入口定价 + 分量内 BFS**（而非 min-plus 值迭代）：
//!
//! 1. 按 SCC 凝聚图的拓扑序处理（依赖先算）；
//! 2. 每个分量先只用**外部输入**（已定稿的上游材料）计算"入口价"；
//! 3. 分量内做 BFS 轮次传播（每材料只定稿一次，绝不二次下调）；
//! 4. 原料 / 可采集资源 = 1.0（每单位；流体按桶）。
//!
//! 为什么不用值迭代：JEI 数据里存在材料放大环（1 锭 → 2 线 → 2 缆 →
//! 电解回收 288mB ≈ 2 锭的铜液），纯物料模型下环是"无限资源"，值迭代会
//! 把成本一路收缩到 0 并污染全图。真实游戏靠耗电阻止这种循环，而本数据
//! 集没有 EU/时长数据，因此必须在定价阶段显式切断环：**环内定价只用
//! 入口 + 单向传播，不做迭代下探**。

use std::collections::VecDeque;

use crate::algo::scc::SccResult;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use crate::util::{mat_norm_qty, output_qty_of};

/// 成本/深度启发式数据库。
#[derive(Debug, Clone)]
pub struct CostDb {
    pub unit_cost: Vec<f64>,
    pub depth: Vec<u32>,
    pub best_recipe: Vec<Option<RecipeId>>,
    pub cyclic: Vec<bool>,
    /// 深度值迭代轮数（成本计算不涉及）。
    pub iterations: usize,
    pub converged: bool,
}

/// 锚点候选裁决：严格更便宜 > 非回收 > 更浅（更接近原料）> 保留当前。
fn better_candidate(
    g: &KnowledgeGraph,
    cand: f64,
    cand_rid: RecipeId,
    cand_depth: u32,
    best: f64,
    best_rid: Option<RecipeId>,
    best_depth: u32,
) -> bool {
    if !cand.is_finite() {
        return false;
    }
    if !best.is_finite() {
        return true;
    }
    if cand < best - 1e-12 * (1.0 + cand.abs()) {
        return true;
    }
    if (cand - best).abs() <= 1e-9 * (1.0 + cand.abs().max(best.abs())) {
        if let Some(br) = best_rid {
            let cand_rec = g.is_recycling(cand_rid);
            let best_rec = g.is_recycling(br);
            if cand_rec != best_rec {
                return best_rec && !cand_rec;
            }
            // 同成本同类别：优先更浅（更接近原料）的路线
            if cand_depth != best_depth {
                return cand_depth < best_depth;
            }
        }
    }
    false
}

/// 回收类配方的成本惩罚系数：拆成品回炉不应作为"从零生产"的首选路线。
const RECYCLING_COST_PENALTY: f64 = 4.0;

/// 单条配方的可生产性/成本计算（含回收惩罚）。
/// 返回 `Some(cost)` 当且仅当每个输入槽都能从 `usable(m) == true` 的候选中选出一个。
fn recipe_cost_filtered<F>(
    g: &KnowledgeGraph,
    unit: &[f64],
    r: &crate::model::RecipeNode,
    penalty: f64,
    usable: F,
) -> Option<f64>
where
    F: Fn(MaterialId) -> bool,
{
    let mut rc = 0.0;
    for slot in &r.inputs {
        let mut slot_best = f64::INFINITY;
        for &(am, aq) in &slot.alts {
            if usable(am) {
                let v = unit[am as usize] * mat_norm_qty(g, am, aq);
                if v < slot_best {
                    slot_best = v;
                }
            }
        }
        if !slot_best.is_finite() {
            return None;
        }
        rc += slot_best;
    }
    Some(rc * penalty)
}

/// 计算材料 m 经配方 r 的单位成本（前提：每个输入槽都有可用候选）。
fn unit_cost_of_recipe<F>(
    g: &KnowledgeGraph,
    unit: &[f64],
    r: &crate::model::RecipeNode,
    m: MaterialId,
    penalty: f64,
    usable: F,
) -> Option<f64>
where
    F: Fn(MaterialId) -> bool,
{
    let out_q = output_qty_of(r, m)?;
    let nq = mat_norm_qty(g, m, out_q);
    if nq <= 0.0 {
        return None;
    }
    let rc = recipe_cost_filtered(g, unit, r, penalty, usable)?;
    Some(rc / nq)
}

/// 配方成本惩罚系数。
fn penalty_of(g: &KnowledgeGraph, rid: RecipeId) -> f64 {
    if g.is_recycling(rid) {
        RECYCLING_COST_PENALTY
    } else {
        1.0
    }
}

/// 构建成本数据库。`max_iters` 只控制深度值迭代。
pub fn build(g: &KnowledgeGraph, scc: &SccResult, max_iters: usize) -> CostDb {
    let n = g.materials.len();
    let m_count = n;
    let comp_count = scc.sizes.len();
    let cyclic: Vec<bool> = (0..n)
        .map(|m| scc.sizes[scc.comp[m] as usize] > 1)
        .collect();

    // ---- 0) 深度值迭代（整数，环不会造成收缩；先算深度用于锚点裁决） ----
    let mut depth = vec![u32::MAX; n];
    for m in 0..n {
        if g.producers[m].is_empty() || g.harvestable[m] {
            depth[m] = 0;
        }
    }
    let mut converged = false;
    let mut iterations = 0;
    for _ in 0..max_iters {
        iterations += 1;
        let mut changed = false;
        for (rid, r) in g.recipes.iter().enumerate() {
            if r.outputs.is_empty() || !g.is_plannable(rid as RecipeId) {
                continue;
            }
            let rd = crate::util::recipe_depth(&depth, r);
            if rd != u32::MAX {
                for slot in &r.outputs {
                    for &(m, _) in &slot.alts {
                        if rd < depth[m as usize] {
                            depth[m as usize] = rd;
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }

    // ---- 1) SCC 凝聚图的拓扑序 ----
    let mut comp_materials: Vec<Vec<MaterialId>> = vec![Vec::new(); comp_count];
    for m in 0..n {
        comp_materials[scc.comp[m] as usize].push(m as MaterialId);
    }
    let mut pairs: Vec<u64> = Vec::new();
    for (rid, r) in g.recipes.iter().enumerate() {
        if r.inputs.is_empty() || r.outputs.is_empty() || !g.is_plannable(rid as RecipeId) {
            continue;
        }
        for in_slot in &r.inputs {
            for &(im, _) in &in_slot.alts {
                let a = scc.comp[im as usize];
                for out_slot in &r.outputs {
                    for &(om, _) in &out_slot.alts {
                        let b = scc.comp[om as usize];
                        if a != b {
                            pairs.push(((a as u64) << 32) | b as u64);
                        }
                    }
                }
            }
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    let mut comp_adj: Vec<Vec<u32>> = vec![Vec::new(); comp_count];
    let mut indeg = vec![0u32; comp_count];
    for &p in &pairs {
        let a = (p >> 32) as u32;
        let b = (p & 0xffff_ffff) as u32;
        comp_adj[a as usize].push(b);
        indeg[b as usize] += 1;
    }
    let mut queue: VecDeque<u32> = (0..comp_count as u32)
        .filter(|&c| indeg[c as usize] == 0)
        .collect();
    let mut topo: Vec<u32> = Vec::with_capacity(comp_count);
    while let Some(c) = queue.pop_front() {
        topo.push(c);
        for i in 0..comp_adj[c as usize].len() {
            let d = comp_adj[c as usize][i];
            indeg[d as usize] -= 1;
            if indeg[d as usize] == 0 {
                queue.push_back(d);
            }
        }
    }
    // 安全兜底：理论上凝聚图无环，若仍有剩余按 id 追加
    if topo.len() < comp_count {
        let mut in_topo = vec![false; comp_count];
        for &c in &topo {
            in_topo[c as usize] = true;
        }
        for c in 0..comp_count as u32 {
            if !in_topo[c as usize] {
                topo.push(c);
            }
        }
    }

    // ---- 2) 逐分量定价 ----
    let mut unit = vec![f64::INFINITY; n];
    let mut finalized = vec![false; n];
    // 入口锚点：让材料第一次获得有限成本的那条"无环入口"配方
    let mut anchor: Vec<Option<RecipeId>> = vec![None; n];

    for &comp in &topo {
        let mats = &comp_materials[comp as usize];
        if mats.is_empty() {
            continue;
        }

        // 1a) 入口价：只用外部（已定稿且不在本分量）的输入
        for &m in mats {
            if g.producers[m as usize].is_empty() || g.harvestable[m as usize] {
                unit[m as usize] = 1.0;
                finalized[m as usize] = true;
                continue;
            }
            let mut best = f64::INFINITY;
            let mut best_rid: Option<RecipeId> = None;
            let mut best_depth = u32::MAX;
            for &rid in &g.producers[m as usize] {
                if !g.is_plannable(rid) {
                    continue;
                }
                let r = g.recipe(rid);
                if r.inputs.is_empty() {
                    continue;
                }
                if let Some(c) = unit_cost_of_recipe(g, &unit, r, m, penalty_of(g, rid), |am| {
                    finalized[am as usize] && scc.comp[am as usize] != comp
                }) {
                    let rd = crate::util::recipe_depth(&depth, r);
                    if better_candidate(g, c, rid, rd, best, best_rid, best_depth) {
                        best = c;
                        best_rid = Some(rid);
                        best_depth = rd;
                    }
                }
            }
            if best.is_finite() {
                unit[m as usize] = best;
                finalized[m as usize] = true;
                anchor[m as usize] = best_rid;
            }
        }

        // 1b) 分量内 BFS 轮次：每个材料只定稿一次（绝不二次下调，切断放大环）
        for _round in 0..64 {
            let mut changed = false;
            for &m in mats {
                if finalized[m as usize] {
                    continue;
                }
                let mut best = f64::INFINITY;
                let mut best_rid: Option<RecipeId> = None;
                let mut best_depth = u32::MAX;
                for &rid in &g.producers[m as usize] {
                    if !g.is_plannable(rid) {
                        continue;
                    }
                    let r = g.recipe(rid);
                    if r.inputs.is_empty() {
                        continue;
                    }
                    if let Some(c) = unit_cost_of_recipe(
                        g,
                        &unit,
                        r,
                        m,
                        penalty_of(g, rid),
                        |am| finalized[am as usize],
                    ) {
                        let rd = crate::util::recipe_depth(&depth, r);
                        if better_candidate(g, c, rid, rd, best, best_rid, best_depth) {
                            best = c;
                            best_rid = Some(rid);
                            best_depth = rd;
                        }
                    }
                }
                if best.is_finite() {
                    unit[m as usize] = best;
                    finalized[m as usize] = true;
                    anchor[m as usize] = best_rid;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // 1c) 无入口兜底：整分量视为世界资源（挖矿/采集），成本 1.0
        if !mats.iter().any(|&m| finalized[m as usize]) {
            for &m in mats {
                unit[m as usize] = 1.0;
                finalized[m as usize] = true;
            }
        }
    }

    // ---- 2) 无入口 SCC 兜底 ----
    // 没有任何材料能从外部输入定价的分量 = 世界资源（如 raw_copper 与
    // raw_copper_block 的压缩环：实际靠挖矿获得，JEI 里没有"挖矿"配方）。
    // 整分量按可采集资源定价（成本 1.0），避免整条矿石链断裂。
    // 注意：必须放在拓扑循环内（上游先兜底，下游才能继续定价）。

    // ---- 3) 冻结成本后挑选最佳配方 ----
    // 裁决：严格更便宜 > 并列时优先非同一 SCC（打破循环偏好）
    let mut best: Vec<Option<RecipeId>> = vec![None; n];
    let mut best_cost = vec![f64::INFINITY; n];
    for (rid, r) in g.recipes.iter().enumerate() {
        if r.outputs.is_empty() || r.inputs.is_empty() || !g.is_plannable(rid as RecipeId) {
            continue;
        }
        let Some(rc) = recipe_cost_filtered(g, &unit, r, penalty_of(g, rid as RecipeId), |am| {
            unit[am as usize].is_finite()
        }) else {
            continue;
        };
        let same_scc =
            |r2: RecipeId, m: MaterialId| scc.comp[m_count + r2 as usize] == scc.comp[m as usize];
        for slot in &r.outputs {
            for &(m, oq) in &slot.alts {
                let nq = mat_norm_qty(g, m, oq);
                if nq <= 0.0 {
                    continue;
                }
                let c = rc / nq;
                let better = match best[m as usize] {
                    None => true,
                    Some(cur) => {
                        if c < best_cost[m as usize] - 1e-12 * (1.0 + c.abs()) {
                            true
                        } else if (c - best_cost[m as usize]).abs()
                            <= 1e-9 * (1.0 + c.abs().max(best_cost[m as usize].abs()))
                        {
                            // 并列裁决：入口锚点 > 非回收 > 非同一 SCC > 保留当前
                            let cand_is_anchor = anchor[m as usize] == Some(rid as RecipeId);
                            let cur_is_anchor = anchor[m as usize] == Some(cur);
                            if cand_is_anchor && !cur_is_anchor {
                                true
                            } else if !cand_is_anchor && cur_is_anchor {
                                false
                            } else {
                                let cand_rec = g.is_recycling(rid as RecipeId);
                                let cur_rec = g.is_recycling(cur);
                                if !cand_rec && cur_rec {
                                    true
                                } else if cand_rec && !cur_rec {
                                    false
                                } else {
                                    same_scc(cur, m) && !same_scc(rid as RecipeId, m)
                                }
                            }
                        } else {
                            false
                        }
                    }
                };
                if better {
                    best_cost[m as usize] = c;
                    best[m as usize] = Some(rid as RecipeId);
                }
            }
        }
    }

    CostDb {
        unit_cost: unit,
        depth,
        best_recipe: best,
        cyclic,
        iterations,
        converged,
    }
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
              "id": "test:craft/plate_from_ingot",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 1}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:plate", "count": 1}]}]
            },
            {
              "id": "test:craft/block_from_plates",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:plate", "count": 9}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:block", "count": 1}]}]
            },
            {
              "id": "test:craft/expensive_block",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 99}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:block", "count": 1}]}]
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn costs_and_depth() {
        let g = from_json_str(FIXTURE).unwrap();
        let (n, adj) = build_adjacency(&g);
        let scc = tarjan(n, &adj);
        let db = build(&g, &scc, 32);

        let ingot = g.find_material(MaterialKind::Item, "test:ingot", None).unwrap();
        let plate = g.find_material(MaterialKind::Item, "test:plate", None).unwrap();
        let block = g.find_material(MaterialKind::Item, "test:block", None).unwrap();

        assert_eq!(db.unit_cost[ingot as usize], 1.0);
        assert_eq!(db.unit_cost[plate as usize], 1.0);
        assert!((db.unit_cost[block as usize] - 9.0).abs() < 1e-9, "block 应选 9 板路线");
        assert_eq!(db.depth[ingot as usize], 0);
        assert_eq!(db.depth[plate as usize], 1);
        // 深度取所有路线的最小值（99 锭直造 block 深度为 1）
        assert_eq!(db.depth[block as usize], 1);
        let best = db.best_recipe[block as usize].unwrap();
        assert_eq!(g.recipe(best).id, "test:craft/block_from_plates");
    }

    /// 净守恒循环（1:1 往返）不能让循环路线胜出。
    #[test]
    fn conservative_cycle_does_not_beat_raw_route() {
        let g = from_json_str(
            r#"{
          "format": "test", "version": 1, "minecraft_version": "1.20.1",
          "categories": [
            {
              "type": "test:craft", "title": "Craft", "catalysts": [],
              "recipes": [
                {
                  "id": "test:craft/raw_to_a",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 2}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:a", "count": 1}]}]
                },
                {
                  "id": "test:craft/a_to_b",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:a", "count": 1}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:b", "count": 1}]}]
                },
                {
                  "id": "test:craft/b_to_a",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:b", "count": 1}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:a", "count": 1}]}]
                }
              ]
            }
          ]
        }"#,
        )
        .unwrap();
        let (n, adj) = build_adjacency(&g);
        let scc = tarjan(n, &adj);
        let db = build(&g, &scc, 32);

        let a = g.find_material(MaterialKind::Item, "test:a", None).unwrap();
        assert!(db.cyclic[a as usize]);
        assert!((db.unit_cost[a as usize] - 2.0).abs() < 1e-9, "a 应走矿石路线");
        let best = db.best_recipe[a as usize].unwrap();
        assert_eq!(g.recipe(best).id, "test:craft/raw_to_a", "应优先非循环路线");
    }

    /// 材料放大环（1 锭 → 2 线 → 电解 2 锭）不能把成本收缩到 0。
    #[test]
    fn amplifying_cycle_is_cut() {
        let g = from_json_str(
            r#"{
          "format": "test", "version": 1, "minecraft_version": "1.20.1",
          "categories": [
            {
              "type": "test:craft", "title": "Craft", "catalysts": [],
              "recipes": [
                {
                  "id": "test:craft/ore_to_ingot",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 2}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 1}]}]
                },
                {
                  "id": "test:craft/ingot_to_wire",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 1}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:wire", "count": 2}]}]
                },
                {
                  "id": "test:craft/wire_to_ingot",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:wire", "count": 2}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 1}]}]
                }
              ]
            }
          ]
        }"#,
        )
        .unwrap();
        let (n, adj) = build_adjacency(&g);
        let scc = tarjan(n, &adj);
        let db = build(&g, &scc, 32);

        let ingot = g.find_material(MaterialKind::Item, "test:ingot", None).unwrap();
        let wire = g.find_material(MaterialKind::Item, "test:wire", None).unwrap();
        assert!(db.cyclic[ingot as usize]);
        assert!((db.unit_cost[ingot as usize] - 2.0).abs() < 1e-9, "锭应锚定矿石路线");
        assert!((db.unit_cost[wire as usize] - 1.0).abs() < 1e-9, "线 = 锭/2");
        let best = db.best_recipe[ingot as usize].unwrap();
        assert_eq!(g.recipe(best).id, "test:craft/ore_to_ingot");
    }
}
