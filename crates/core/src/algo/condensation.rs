//! SCC 凝聚图（一级公民）。
//!
//! 生产图（材料 ⇄ 配方）压缩成 SCC 后是一张 DAG；工业优化里
//! **一个 SCC = 一个循环系统**（如电解水↔氢氧、锭↔粒↔块↔缆回收环），
//! 对外可以看成一个"超节点"：
//!
//! ```text
//! 超节点(电解水循环)
//!   输入: 水 / 电
//!   输出: 氢 / 氧
//!   内部: 水回路（不对外暴露）
//! ```
//!
//! 本模块把凝聚图显式化，供成本库（拓扑定价）与 LP（子图构建）复用。

use crate::algo::scc::SccResult;
use crate::graph::KnowledgeGraph;
use crate::model::{MaterialId, RecipeId};
use std::collections::VecDeque;

/// SCC 凝聚图。
pub struct Condensation {
    pub comp_count: usize,
    /// 材料 → 分量 id
    pub comp_of_material: Vec<u32>,
    /// 配方 → 分量 id（配方节点在二部图中的编号 = materials.len() + rid）
    pub comp_of_recipe: Vec<u32>,
    /// 分量 → 材料列表
    pub materials_in_comp: Vec<Vec<MaterialId>>,
    /// 分量 → 配方列表
    pub recipes_in_comp: Vec<Vec<RecipeId>>,
    /// 拓扑序（依赖在前：输入分量先于输出分量）
    pub topo_order: Vec<u32>,
    /// 分量 → 下游分量（它产出的材料被哪些分量消费）
    pub comp_downstream: Vec<Vec<u32>>,
    /// 分量 → 上游分量（它消费的材料由哪些分量生产）
    pub comp_upstream: Vec<Vec<u32>>,
    /// 环分量（size > 1）
    pub cyclic: Vec<bool>,
}

impl Condensation {
    pub fn build(g: &KnowledgeGraph, scc: &SccResult) -> Self {
        let m_count = g.materials.len();
        let r_count = g.recipes.len();
        let comp_count = scc.sizes.len();

        let comp_of_material: Vec<u32> = (0..m_count).map(|m| scc.comp[m]).collect();
        let comp_of_recipe: Vec<u32> = (0..r_count).map(|r| scc.comp[m_count + r]).collect();

        let mut materials_in_comp: Vec<Vec<MaterialId>> = vec![Vec::new(); comp_count];
        for m in 0..m_count {
            materials_in_comp[comp_of_material[m] as usize].push(m as MaterialId);
        }
        let mut recipes_in_comp: Vec<Vec<RecipeId>> = vec![Vec::new(); comp_count];
        for r in 0..r_count {
            recipes_in_comp[comp_of_recipe[r] as usize].push(r as RecipeId);
        }

        // 分量级依赖：配方 r 消费材料 i、产出材料 o ⇒ comp(i) → comp(o)
        let mut pairs: Vec<u64> = Vec::new();
        for (rid, r) in g.recipes.iter().enumerate() {
            if r.inputs.is_empty() || r.outputs.is_empty() || !g.is_plannable(rid as RecipeId) {
                continue;
            }
            for in_slot in &r.inputs {
                for &(im, _) in &in_slot.alts {
                    let a = comp_of_material[im as usize];
                    for out_slot in &r.outputs {
                        for &(om, _) in &out_slot.alts {
                            let b = comp_of_material[om as usize];
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

        let mut comp_downstream: Vec<Vec<u32>> = vec![Vec::new(); comp_count];
        let mut comp_upstream: Vec<Vec<u32>> = vec![Vec::new(); comp_count];
        let mut indeg = vec![0u32; comp_count];
        for &p in &pairs {
            let a = (p >> 32) as u32;
            let b = (p & 0xffff_ffff) as u32;
            comp_downstream[a as usize].push(b);
            comp_upstream[b as usize].push(a);
            indeg[b as usize] += 1;
        }

        // Kahn 拓扑排序（凝聚图必然无环）
        let mut queue: VecDeque<u32> = (0..comp_count as u32)
            .filter(|&c| indeg[c as usize] == 0)
            .collect();
        let mut topo_order: Vec<u32> = Vec::with_capacity(comp_count);
        while let Some(c) = queue.pop_front() {
            topo_order.push(c);
            for i in 0..comp_downstream[c as usize].len() {
                let d = comp_downstream[c as usize][i];
                indeg[d as usize] -= 1;
                if indeg[d as usize] == 0 {
                    queue.push_back(d);
                }
            }
        }
        // 安全兜底：理论上不会触发
        if topo_order.len() < comp_count {
            let mut seen = vec![false; comp_count];
            for &c in &topo_order {
                seen[c as usize] = true;
            }
            for c in 0..comp_count as u32 {
                if !seen[c as usize] {
                    topo_order.push(c);
                }
            }
        }

        let cyclic = (0..comp_count).map(|c| scc.sizes[c] > 1).collect();

        Self {
            comp_count,
            comp_of_material,
            comp_of_recipe,
            materials_in_comp,
            recipes_in_comp,
            topo_order,
            comp_downstream,
            comp_upstream,
            cyclic,
        }
    }

    /// 该材料是否在循环分量中。
    pub fn material_cyclic(&self, m: MaterialId) -> bool {
        self.cyclic[self.comp_of_material[m as usize] as usize]
    }

    /// 统计信息（供 API / 文档）。
    pub fn cyclic_component_count(&self) -> usize {
        self.cyclic.iter().filter(|&&c| c).count()
    }
}
