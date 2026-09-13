//! 评估子图与候选选择表的数据打包（CPU 侧）。
//!
//! 子图**从基线方案构建**：基线里每个材料的所选配方 + top-N 替代配方，
//! 以及它们的输入材料。这样 beam 的全部单点扰动候选都能被完整记账。

use std::collections::HashMap;

use gt_planner_core::analysis::Analysis;
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::{MaterialId, MaterialKind, RecipeId};
use gt_planner_core::planner::recipe_alternatives;
use gt_planner_core::util::cheapest_alt;

/// 评估子图。
///
/// 单位约定：所有数量（cons_qty / out_qty / demand）都用**材料原始单位**
/// （物品=个，流体=mB）；`norm_factor` 负责换算成成本权重（流体 0.001/mB）。
pub struct SubgraphData {
    pub target: MaterialId,
    pub materials: Vec<MaterialId>,
    pub mat_index: HashMap<MaterialId, usize>,
    pub recipes: Vec<RecipeId>,
    pub rec_index: HashMap<RecipeId, usize>,
    /// 每材料：消耗它的 (配方局部 idx, 数量)，CSR 偏移
    pub cons_offsets: Vec<u32>,
    pub cons_recipes: Vec<u32>,
    pub cons_qty: Vec<f32>,
    /// 每材料：成本权重（物品 1.0 / 流体 0.001）
    pub norm_factor: Vec<f32>,
    /// 每配方：主输出 (材料局部 idx, 数量)
    pub recipe_outputs: Vec<Vec<(u32, f32)>>,
    /// 每配方：实际计入 CSR 的输入材料（全局 id，用于候选合法性检查）
    pub recipe_inputs: Vec<Vec<MaterialId>>,
}

#[allow(clippy::too_many_arguments)]
fn add_recipe(
    g: &KnowledgeGraph,
    an: &Analysis,
    rid: RecipeId,
    materials: &mut Vec<MaterialId>,
    mat_index: &mut HashMap<MaterialId, usize>,
    recipes: &mut Vec<RecipeId>,
    rec_index: &mut HashMap<RecipeId, usize>,
    recipe_outputs: &mut Vec<Vec<(u32, f32)>>,
    recipe_inputs: &mut Vec<Vec<MaterialId>>,
) {
    if rec_index.contains_key(&rid) {
        return;
    }
    let r = g.recipe(rid);
    if r.inputs.is_empty() || r.outputs.is_empty() {
        return;
    }
    let mut ins_global: Vec<MaterialId> = Vec::new();
    for slot in &r.inputs {
        if let Some((im, _)) = cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot) {
            if !mat_index.contains_key(&im) {
                let li = materials.len();
                materials.push(im);
                mat_index.insert(im, li);
            }
            ins_global.push(im);
        }
    }
    let mut outs: Vec<(u32, f32)> = Vec::new();
    for slot in &r.outputs {
        let Some((om, oq)) = slot.primary() else {
            continue;
        };
        if !mat_index.contains_key(&om) {
            let lo = materials.len();
            materials.push(om);
            mat_index.insert(om, lo);
        }
        outs.push((mat_index[&om] as u32, oq as f32));
    }
    if outs.is_empty() {
        return;
    }
    rec_index.insert(rid, recipes.len());
    recipes.push(rid);
    recipe_outputs.push(outs);
    recipe_inputs.push(ins_global);
}

/// 从基线方案构建评估子图。
pub fn build_subgraph_from_plan(
    g: &KnowledgeGraph,
    an: &Analysis,
    target: MaterialId,
    choices: &HashMap<MaterialId, RecipeId>,
    alternatives_per_material: usize,
) -> SubgraphData {
    let mut materials: Vec<MaterialId> = vec![target];
    let mut mat_index: HashMap<MaterialId, usize> = HashMap::new();
    mat_index.insert(target, 0);
    let mut recipes: Vec<RecipeId> = Vec::new();
    let mut rec_index: HashMap<RecipeId, usize> = HashMap::new();
    let mut recipe_outputs: Vec<Vec<(u32, f32)>> = Vec::new();
    let mut recipe_inputs: Vec<Vec<MaterialId>> = Vec::new();

    for (&m, &rid) in choices {
        if !mat_index.contains_key(&m) {
            let mi = materials.len();
            materials.push(m);
            mat_index.insert(m, mi);
        }
        add_recipe(
            g,
            an,
            rid,
            &mut materials,
            &mut mat_index,
            &mut recipes,
            &mut rec_index,
            &mut recipe_outputs,
            &mut recipe_inputs,
        );
        for alt in recipe_alternatives(g, an, m, Some(rid), alternatives_per_material) {
            add_recipe(
                g,
                an,
                alt,
                &mut materials,
                &mut mat_index,
                &mut recipes,
                &mut rec_index,
                &mut recipe_outputs,
                &mut recipe_inputs,
            );
        }
    }

    let n = materials.len();
    // 每材料的消耗条目（按配方输入的最便宜候选）
    let mut per_material: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
    for (ri, rid) in recipes.iter().enumerate() {
        let r = g.recipe(*rid);
        for slot in &r.inputs {
            if let Some((im, q)) = cheapest_alt(g, &an.cost.unit_cost, Some(&an.cost.cyclic), slot)
            {
                if let Some(&li) = mat_index.get(&im) {
                    per_material[li].push((ri as u32, q as f32));
                }
            }
        }
    }
    let mut cons_offsets: Vec<u32> = Vec::with_capacity(n + 1);
    let mut cons_recipes: Vec<u32> = Vec::new();
    let mut cons_qty: Vec<f32> = Vec::new();
    cons_offsets.push(0);
    for li in 0..n {
        for &(ri, q) in &per_material[li] {
            cons_recipes.push(ri);
            cons_qty.push(q);
        }
        cons_offsets.push(cons_recipes.len() as u32);
    }

    let norm_factor: Vec<f32> = materials
        .iter()
        .map(|&m| match g.material(m).key.kind {
            MaterialKind::Fluid => 0.001,
            _ => 1.0,
        })
        .collect();

    SubgraphData {
        target,
        materials,
        mat_index,
        recipes,
        rec_index,
        cons_offsets,
        cons_recipes,
        cons_qty,
        norm_factor,
        recipe_outputs,
        recipe_inputs,
    }
}

/// 候选选择表（GPU 输入）。
pub struct CandidateTable {
    /// 每配方 → 选择它的材料局部 idx（u32::MAX = 未被选）
    pub owner: Vec<u32>,
    /// 每材料 → 所选配方对该材料的产出数量（0 = 无配方），材料原始单位
    pub out_qty: Vec<f32>,
}

impl CandidateTable {
    /// 从"材料 → 配方"选择表构建；若引用了子图外的材料/配方则返回 None。
    pub fn from_choices(
        sg: &SubgraphData,
        choices: &HashMap<MaterialId, RecipeId>,
    ) -> Option<Self> {
        let mut owner = vec![u32::MAX; sg.recipes.len()];
        let mut out_qty = vec![0.0f32; sg.materials.len()];
        for (&m, &rid) in choices {
            let (Some(&mi), Some(&ri)) = (sg.mat_index.get(&m), sg.rec_index.get(&rid)) else {
                return None;
            };
            // 所选配方必须把 m 作为主输出之一
            let oq = sg.recipe_outputs[ri]
                .iter()
                .find(|&&(lm, _)| lm == mi as u32)
                .map(|&(_, q)| q)?;
            if oq <= 0.0 {
                return None;
            }
            // 所有输入必须在子图内（否则 GPU 记账不完整）
            for im in &sg.recipe_inputs[ri] {
                if !sg.mat_index.contains_key(im) {
                    return None;
                }
            }
            owner[ri] = mi as u32;
            out_qty[mi] = oq;
        }
        Some(CandidateTable { owner, out_qty })
    }
}
