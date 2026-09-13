//! Process IR：物料流图（工艺流程图）。
//!
//! Plan IR 是"配方清单"（每步 ops/机器/EU），Process IR 是**流网络**：
//! - 节点：配方步骤（含 ops/机器数/等级/EU）
//! - 边：物料流（含速率），来源/去向为步骤、外部输入（原料）或输出（目标/副产物）
//!
//! 用途：流程图可视化、物流平衡核对、后续接 GPU 线性求解的输入。

use std::collections::HashMap;

use serde::Serialize;

use crate::model::MaterialDto;
use crate::plan::Plan;

/// 材料流键（kind, id, nbt 三元组，字符串化便于序列化）。
pub type FlowKey = (String, String, Option<String>);

fn key_of(m: &MaterialDto) -> FlowKey {
    (m.kind.clone(), m.id.clone(), m.nbt.clone())
}

/// 工艺流程图。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessGraph {
    pub target: MaterialDto,
    pub rate_per_min: f64,
    pub steps: Vec<ProcessStep>,
    /// 内部流：步骤 → 步骤（含目标产物流，to_step = None 且材料 = 目标）
    pub flows: Vec<ProcessFlow>,
    /// 外部输入（原料）
    pub raw_inputs: Vec<ProcessFlow>,
    /// 副产物
    pub byproducts: Vec<ProcessFlow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessStep {
    pub index: usize,
    pub recipe: String,
    pub category_title: String,
    pub ops_per_min: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_count: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eu_t: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessFlow {
    pub material: MaterialDto,
    pub rate_per_min: f64,
    /// None = 外部输入（原料）
    pub from_step: Option<usize>,
    /// None = 目标产物 / 副产物
    pub to_step: Option<usize>,
}

const EPS: f64 = 1e-9;

impl ProcessGraph {
    /// 从 Plan IR 构建物料流图。
    ///
    /// 匹配策略：按材料贪心配对（先到先得），产出优先满足同材料的消耗，
    /// 剩余产出记副产物，未被满足的消耗记外部输入。
    pub fn from_plan(plan: &Plan) -> Self {
        let steps: Vec<ProcessStep> = plan
            .recipes
            .iter()
            .enumerate()
            .map(|(i, r)| ProcessStep {
                index: i,
                recipe: r.recipe.clone(),
                category_title: r.category_title.clone(),
                ops_per_min: r.ops_per_min,
                machine_count: r.machine_count,
                tier: r.tier.clone(),
                eu_t: r.eut,
            })
            .collect();

        // 产出队列 / 消耗清单
        let mut produced: HashMap<FlowKey, Vec<(usize, f64, MaterialDto)>> = HashMap::new();
        for (i, r) in plan.recipes.iter().enumerate() {
            for o in &r.outputs {
                produced
                    .entry(key_of(&o.material))
                    .or_default()
                    .push((i, o.rate_per_min, o.material.clone()));
            }
        }
        let mut consumed: Vec<(usize, FlowKey, f64, MaterialDto)> = Vec::new();
        for (i, r) in plan.recipes.iter().enumerate() {
            for inp in &r.inputs {
                consumed.push((i, key_of(&inp.material), inp.rate_per_min, inp.material.clone()));
            }
        }
        // 稳定顺序：按材料键排序，保证确定性
        consumed.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let mut flows: Vec<ProcessFlow> = Vec::new();
        let mut raw_inputs: Vec<ProcessFlow> = Vec::new();

        for (to_step, key, rate, material) in consumed {
            let mut need = rate;
            if let Some(queue) = produced.get_mut(&key) {
                for entry in queue.iter_mut() {
                    if need <= EPS {
                        break;
                    }
                    if entry.1 <= EPS {
                        continue;
                    }
                    let take = need.min(entry.1);
                    entry.1 -= take;
                    need -= take;
                    flows.push(ProcessFlow {
                        material: material.clone(),
                        rate_per_min: take,
                        from_step: Some(entry.0),
                        to_step: Some(to_step),
                    });
                }
            }
            if need > EPS {
                raw_inputs.push(ProcessFlow {
                    material,
                    rate_per_min: need,
                    from_step: None,
                    to_step: Some(to_step),
                });
            }
        }

        // 剩余产出：目标材料按需求计目标流（超出部分记副产物）；其余 → 副产物
        let target_key = key_of(&plan.target);
        let mut target_remaining = plan.rate_per_min;
        let mut byproducts: Vec<ProcessFlow> = Vec::new();
        let mut keys: Vec<FlowKey> = produced.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let queue = produced.remove(&key).unwrap_or_default();
            for (step, rate, material) in queue {
                if rate <= EPS {
                    continue;
                }
                if key == target_key && target_remaining > EPS {
                    let to_target = rate.min(target_remaining);
                    target_remaining -= to_target;
                    flows.push(ProcessFlow {
                        material: material.clone(),
                        rate_per_min: to_target,
                        from_step: Some(step),
                        to_step: None,
                    });
                    let excess = rate - to_target;
                    if excess > EPS {
                        byproducts.push(ProcessFlow {
                            material,
                            rate_per_min: excess,
                            from_step: Some(step),
                            to_step: None,
                        });
                    }
                } else {
                    byproducts.push(ProcessFlow {
                        material,
                        rate_per_min: rate,
                        from_step: Some(step),
                        to_step: None,
                    });
                }
            }
        }

        // 排序：步骤按 ops 降序（与 Plan 一致）；流按速率降序
        flows.sort_by(|a, b| b.rate_per_min.total_cmp(&a.rate_per_min));
        raw_inputs.sort_by(|a, b| b.rate_per_min.total_cmp(&a.rate_per_min));
        byproducts.sort_by(|a, b| b.rate_per_min.total_cmp(&a.rate_per_min));

        Self {
            target: plan.target.clone(),
            rate_per_min: plan.rate_per_min,
            steps,
            flows,
            raw_inputs,
            byproducts,
        }
    }
}
