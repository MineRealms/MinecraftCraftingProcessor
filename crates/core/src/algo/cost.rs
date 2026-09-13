//! 启发式成本数据库（CostVector：物料 / EU / 机器时间）。
//!
//! 每个材料维护三维成本向量：
//! - `material`：原始物品当量（物品=个，流体=桶）
//! - `eu`：累计 EU（只计耗电；发电不抵扣，避免等离子发电类配方被过度偏好）
//! - `machine`：累计机器时间（machine-minutes，= Σ duration/1200）
//!
//! 路线选择用**加权综合分** `score = w_m·material + w_e·eu + w_t·machine`，
//! 权重可配置（economy / power / speed / balanced 预设）。
//!
//! 数值稳定性：所有比较用相对容差；每个材料在分量内**只定稿一次**，
//! 切断"材料放大环"（1 锭 → 2 线 → 2 缆 → 回收 ≈ 2 锭）造成的成本收缩。
//!
//! 定价流程（与 SCC 凝聚图配合）：
//! 1. 按凝聚图拓扑序处理（`Condensation`）；
//! 2. 入口定价：只用分量外、已定稿的输入；
//! 3. 分量内 BFS 轮次：每材料只定稿一次；
//! 4. 无入口分量 → 世界资源（挖矿/采集）= [1, 0, 0]；
//! 5. best_recipe 并列裁决：入口锚点 > 非回收 > 深度更浅 > 非同一 SCC。

use serde::Serialize;

use crate::algo::condensation::Condensation;
use crate::graph::KnowledgeGraph;
use crate::model::RecipeId;
use crate::util::{mat_norm_qty, output_qty_of};

/// 成本权重。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CostWeights {
    pub material: f64,
    pub eu: f64,
    pub machine: f64,
}

impl Default for CostWeights {
    fn default() -> Self {
        // balanced：1 原始物品 ≈ 1 万 EU ≈ 1 机器分钟
        Self {
            material: 1.0,
            eu: 1e-4,
            machine: 1.0,
        }
    }
}

impl CostWeights {
    /// 预设：balanced / economy / power / speed。
    pub fn preset(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "balanced" => Some(Self::default()),
            "economy" | "material" => Some(Self {
                material: 1.0,
                eu: 1e-6,
                machine: 0.1,
            }),
            "power" | "eu" => Some(Self {
                material: 0.05,
                eu: 1e-4,
                machine: 0.1,
            }),
            "speed" | "time" => Some(Self {
                material: 0.1,
                eu: 1e-6,
                machine: 1.0,
            }),
            _ => None,
        }
    }
}

/// 成本向量。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CostVector {
    pub material: f64,
    pub eu: f64,
    /// 机器时间（machine-minutes）
    pub machine: f64,
}

impl CostVector {
    pub const INF: CostVector = CostVector {
        material: f64::INFINITY,
        eu: f64::INFINITY,
        machine: f64::INFINITY,
    };

    /// 原始资源（挖矿/采集）。
    pub const RAW: CostVector = CostVector {
        material: 1.0,
        eu: 0.0,
        machine: 0.0,
    };

    pub fn is_finite(&self) -> bool {
        self.material.is_finite() && self.eu.is_finite() && self.machine.is_finite()
    }

    pub fn score(&self, w: &CostWeights) -> f64 {
        w.material * self.material + w.eu * self.eu + w.machine * self.machine
    }

    pub fn add(self, o: CostVector) -> CostVector {
        CostVector {
            material: self.material + o.material,
            eu: self.eu + o.eu,
            machine: self.machine + o.machine,
        }
    }

    pub fn scale(self, k: f64) -> CostVector {
        CostVector {
            material: self.material * k,
            eu: self.eu * k,
            machine: self.machine * k,
        }
    }

    /// 除以产出数量（归一）。
    pub fn per_unit(self, nq: f64) -> CostVector {
        if nq > 0.0 {
            self.scale(1.0 / nq)
        } else {
            Self::INF
        }
    }
}

/// 配方自身的每操作成本（不含输入）：EU 与机器时间来自 GT 数据。
/// 注意：发电配方不抵扣 EU（避免生成类配方被过度偏好）。
fn recipe_const_vec(r: &crate::model::RecipeNode) -> CostVector {
    match &r.gt {
        Some(gt) => CostVector {
            material: 0.0,
            eu: if gt.consumes_energy() { gt.total_eu } else { 0.0 },
            machine: gt.duration_ticks as f64 / 1200.0,
        },
        None => CostVector::default(),
    }
}

/// 成本/深度启发式数据库。
#[derive(Debug, Clone)]
pub struct CostDb {
    /// 每个材料的最优路线成本向量（与 best_recipe 一致）。
    pub vectors: Vec<CostVector>,
    /// 加权综合成本（= vectors[i].score(weights)），供选择器使用。
    pub unit_cost: Vec<f64>,
    pub depth: Vec<u32>,
    pub best_recipe: Vec<Option<RecipeId>>,
    pub cyclic: Vec<bool>,
    pub iterations: usize,
    pub converged: bool,
    pub weights: CostWeights,
}

/// 相对容差。
fn tie_tol(a: f64, b: f64) -> f64 {
    1e-9 * (1.0 + a.abs().max(b.abs()))
}

/// 配方成本向量（所有输入槽取加权最便宜的可用候选 + 配方自身常量）。
fn recipe_cost_vec<F>(
    g: &KnowledgeGraph,
    costs: &[CostVector],
    w: &CostWeights,
    r: &crate::model::RecipeNode,
    penalty: f64,
    usable: F,
) -> Option<CostVector>
where
    F: Fn(crate::model::MaterialId) -> bool,
{
    let mut sum = recipe_const_vec(r);
    for slot in &r.inputs {
        let mut best: Option<CostVector> = None;
        let mut best_score = f64::INFINITY;
        for &(m, q) in &slot.alts {
            if !usable(m) {
                continue;
            }
            let v = costs[m as usize];
            if !v.is_finite() {
                continue;
            }
            let s = v.scale(mat_norm_qty(g, m, q)).score(w);
            if s < best_score {
                best_score = s;
                best = Some(v.scale(mat_norm_qty(g, m, q)));
            }
        }
        sum = sum.add(best?);
    }
    Some(sum.scale(penalty))
}

/// 材料 m 经配方 r 的单位成本向量。
fn unit_cost_of_recipe<F>(
    g: &KnowledgeGraph,
    costs: &[CostVector],
    w: &CostWeights,
    r: &crate::model::RecipeNode,
    m: crate::model::MaterialId,
    penalty: f64,
    usable: F,
) -> Option<CostVector>
where
    F: Fn(crate::model::MaterialId) -> bool,
{
    let out_q = output_qty_of(r, m)?;
    let nq = mat_norm_qty(g, m, out_q);
    if nq <= 0.0 {
        return None;
    }
    let rc = recipe_cost_vec(g, costs, w, r, penalty, usable)?;
    Some(rc.per_unit(nq))
}

/// 配方成本惩罚系数。
fn penalty_of(g: &KnowledgeGraph, rid: RecipeId) -> f64 {
    if g.is_recycling(rid) {
        4.0
    } else {
        1.0
    }
}

/// 候选裁决：综合分严格更小 > 并列时非回收 > 更浅（更接近原料）> 保留当前。
#[allow(clippy::too_many_arguments)]
fn better_candidate(
    g: &KnowledgeGraph,
    cand: CostVector,
    cand_rid: RecipeId,
    cand_depth: u32,
    best: CostVector,
    best_rid: Option<RecipeId>,
    best_depth: u32,
    w: &CostWeights,
) -> bool {
    if !cand.is_finite() {
        return false;
    }
    if !best.is_finite() {
        return true;
    }
    let cs = cand.score(w);
    let bs = best.score(w);
    if cs < bs - tie_tol(cs, bs) {
        return true;
    }
    if (cs - bs).abs() <= tie_tol(cs, bs) {
        if let Some(br) = best_rid {
            let cand_rec = g.is_recycling(cand_rid);
            let best_rec = g.is_recycling(br);
            if cand_rec != best_rec {
                return best_rec && !cand_rec;
            }
            if cand_depth != best_depth {
                return cand_depth < best_depth;
            }
        }
    }
    false
}

/// 构建成本数据库。
pub fn build(
    g: &KnowledgeGraph,
    cond: &Condensation,
    max_iters: usize,
    weights: CostWeights,
) -> CostDb {
    let n = g.materials.len();
    let w = &weights;
    let cyclic: Vec<bool> = (0..n)
        .map(|m| cond.cyclic[cond.comp_of_material[m] as usize])
        .collect();

    // ---- 0) 深度值迭代（整数；先算深度用于锚点裁决） ----
    let mut depth = vec![u32::MAX; n];
    for m in 0..n {
        if g.is_source(m as crate::model::MaterialId) {
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

    // ---- 1) 逐分量定价（CostVector） ----
    let mut costs = vec![CostVector::INF; n];
    let mut finalized = vec![false; n];
    let mut anchor: Vec<Option<RecipeId>> = vec![None; n];

    for &comp in &cond.topo_order {
        let mats = &cond.materials_in_comp[comp as usize];
        if mats.is_empty() {
            continue;
        }

        // 1a) 入口价：只用外部（已定稿且不在本分量）的输入
        for &m in mats {
            if g.is_source(m as crate::model::MaterialId) {
                costs[m as usize] = CostVector::RAW;
                finalized[m as usize] = true;
                continue;
            }
            let mut best = CostVector::INF;
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
                if let Some(c) = unit_cost_of_recipe(g, &costs, w, r, m, penalty_of(g, rid), |am| {
                    finalized[am as usize] && cond.comp_of_material[am as usize] != comp
                }) {
                    let rd = crate::util::recipe_depth(&depth, r);
                    if better_candidate(g, c, rid, rd, best, best_rid, best_depth, w) {
                        best = c;
                        best_rid = Some(rid);
                        best_depth = rd;
                    }
                }
            }
            if best.is_finite() {
                costs[m as usize] = best;
                finalized[m as usize] = true;
                anchor[m as usize] = best_rid;
            }
        }

        // 1b) 分量内 BFS 轮次：每个材料只定稿一次（切断放大环）
        for _round in 0..64 {
            let mut changed = false;
            for &m in mats {
                if finalized[m as usize] {
                    continue;
                }
                let mut best = CostVector::INF;
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
                    if let Some(c) =
                        unit_cost_of_recipe(g, &costs, w, r, m, penalty_of(g, rid), |am| {
                            finalized[am as usize]
                        })
                    {
                        let rd = crate::util::recipe_depth(&depth, r);
                        if better_candidate(g, c, rid, rd, best, best_rid, best_depth, w) {
                            best = c;
                            best_rid = Some(rid);
                            best_depth = rd;
                        }
                    }
                }
                if best.is_finite() {
                    costs[m as usize] = best;
                    finalized[m as usize] = true;
                    anchor[m as usize] = best_rid;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // 1c) 无入口兜底：整分量视为世界资源（挖矿/采集）
        if !mats.iter().any(|&m| finalized[m as usize]) {
            for &m in mats {
                costs[m as usize] = CostVector::RAW;
                finalized[m as usize] = true;
            }
        }
    }

    // ---- 2) 冻结成本后挑选最佳配方（同时定稿向量） ----
    let mut best: Vec<Option<RecipeId>> = vec![None; n];
    let mut best_score = vec![f64::INFINITY; n];
    for (rid, r) in g.recipes.iter().enumerate() {
        if r.outputs.is_empty() || r.inputs.is_empty() || !g.is_plannable(rid as RecipeId) {
            continue;
        }
        let Some(rc) = recipe_cost_vec(g, &costs, w, r, penalty_of(g, rid as RecipeId), |am| {
            costs[am as usize].is_finite()
        }) else {
            continue;
        };
        let same_scc = |r2: RecipeId, m: u32| {
            cond.comp_of_recipe[r2 as usize] == cond.comp_of_material[m as usize]
        };
        for slot in &r.outputs {
            for &(m, oq) in &slot.alts {
                let nq = mat_norm_qty(g, m, oq);
                if nq <= 0.0 {
                    continue;
                }
                let c = rc.per_unit(nq);
                let cs = c.score(w);
                let better = match best[m as usize] {
                    None => true,
                    Some(cur) => {
                        let bcur = best_score[m as usize];
                        if cs < bcur - tie_tol(cs, bcur) {
                            true
                        } else if (cs - bcur).abs() <= tie_tol(cs, bcur) {
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
                    best_score[m as usize] = cs;
                    best[m as usize] = Some(rid as RecipeId);
                    costs[m as usize] = c;
                }
            }
        }
    }

    let unit_cost: Vec<f64> = costs.iter().map(|c| c.score(w)).collect();

    CostDb {
        vectors: costs,
        unit_cost,
        depth,
        best_recipe: best,
        cyclic,
        iterations,
        converged,
        weights,
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

    fn build_db(g: &KnowledgeGraph) -> CostDb {
        let (n, adj) = build_adjacency(g);
        let scc = tarjan(n, &adj);
        let cond = Condensation::build(g, &scc);
        build(g, &cond, 32, CostWeights::default())
    }

    #[test]
    fn costs_and_depth() {
        let g = from_json_str(FIXTURE).unwrap();
        let db = build_db(&g);

        let ingot = g.find_material(MaterialKind::Item, "test:ingot", None).unwrap();
        let plate = g.find_material(MaterialKind::Item, "test:plate", None).unwrap();
        let block = g.find_material(MaterialKind::Item, "test:block", None).unwrap();

        assert_eq!(db.vectors[ingot as usize].material, 1.0);
        assert_eq!(db.vectors[plate as usize].material, 1.0);
        assert!((db.vectors[block as usize].material - 9.0).abs() < 1e-9, "block 应选 9 板路线");
        assert_eq!(db.depth[ingot as usize], 0);
        assert_eq!(db.depth[plate as usize], 1);
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
        let db = build_db(&g);

        let a = g.find_material(MaterialKind::Item, "test:a", None).unwrap();
        assert!(db.cyclic[a as usize]);
        assert!((db.vectors[a as usize].material - 2.0).abs() < 1e-9, "a 应走矿石路线");
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
        let db = build_db(&g);

        let ingot = g.find_material(MaterialKind::Item, "test:ingot", None).unwrap();
        let wire = g.find_material(MaterialKind::Item, "test:wire", None).unwrap();
        assert!(db.cyclic[ingot as usize]);
        assert!((db.vectors[ingot as usize].material - 2.0).abs() < 1e-9, "锭应锚定矿石路线");
        assert!((db.vectors[wire as usize].material - 1.0).abs() < 1e-9, "线 = 锭/2");
        let best = db.best_recipe[ingot as usize].unwrap();
        assert_eq!(g.recipe(best).id, "test:craft/ore_to_ingot");
    }

    /// EU 进入成本：power 预设应选择低 EU 路线。
    #[test]
    fn eu_weight_changes_route() {
        let g = from_json_str(
            r#"{
          "format": "test", "version": 1, "minecraft_version": "1.20.1",
          "categories": [
            {
              "type": "test:craft", "title": "Craft", "catalysts": [],
              "recipes": [
                {
                  "id": "test:craft/cheap_material_high_eu",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 1}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:part", "count": 1}]}],
                  "gt": {"recipe_type": "test", "duration": 1200, "eut": 1000, "amperage": 1, "energy_io": "in", "total_eu_t": 1000, "total_eu": 1200000}
                },
                {
                  "id": "test:craft/expensive_material_low_eu",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ingot", "count": 4}]}],
                  "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:part", "count": 1}]}],
                  "gt": {"recipe_type": "test", "duration": 10, "eut": 1, "amperage": 1, "energy_io": "in", "total_eu_t": 1, "total_eu": 10}
                },
                {
                  "id": "test:craft/ore_to_ingot",
                  "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 2}]}],
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
        let cond = Condensation::build(&g, &scc);

        // economy：物料优先 → 选 1 矿石路线
        let eco = build(
            &g,
            &cond,
            32,
            CostWeights::preset("economy").unwrap(),
        );
        let part = g.find_material(MaterialKind::Item, "test:part", None).unwrap();
        assert_eq!(
            g.recipe(eco.best_recipe[part as usize].unwrap()).id,
            "test:craft/cheap_material_high_eu"
        );

        // power：EU 优先 → 选低 EU 路线（4 锭）
        let power = build(&g, &cond, 32, CostWeights::preset("power").unwrap());
        assert_eq!(
            g.recipe(power.best_recipe[part as usize].unwrap()).id,
            "test:craft/expensive_material_low_eu"
        );
    }
}
