//! Tarjan SCC（迭代版，避免递归爆栈）。
//!
//! 节点空间 = materials ∪ recipes：
//! - 材料 m → 消耗它的配方 r（m → r）
//! - 配方 r → 它产出的材料 m（r → m）
//!
//! 二部图性质：边只存在于材料↔配方之间，因此大小 > 1 的 SCC 即生产循环
//! （如 水 → 电解机 → 氢/氧 → … → 水）。

use crate::graph::KnowledgeGraph;

/// SCC 结果。
#[derive(Debug, Clone)]
pub struct SccResult {
    /// 节点 → 分量 id。
    pub comp: Vec<u32>,
    /// 分量 id → 节点数。
    pub sizes: Vec<u32>,
    /// 节点总数（= 材料数 + 配方数）。
    pub node_count: usize,
}

impl SccResult {
    /// 该分量是否构成循环（二部图中 size > 1 即为环）。
    pub fn is_cyclic(&self, comp_id: u32) -> bool {
        self.sizes[comp_id as usize] > 1
    }

    /// 节点是否在环中。
    pub fn node_cyclic(&self, node: u32) -> bool {
        self.is_cyclic(self.comp[node as usize])
    }
}

/// 构建二部有向图邻接表（**只含可规划配方**）。
///
/// 节点编号：`[0, m_count)` 材料，`[m_count, m_count+r_count)` 配方。
/// 信息页（diagram / tag_recipes / *_info）不参与，否则标签页会把
/// 大量不相关材料连成巨型 SCC，破坏环检测与定价。
pub fn build_adjacency(g: &KnowledgeGraph) -> (usize, Vec<Vec<u32>>) {
    let m_count = g.materials.len();
    let r_count = g.recipes.len();
    let n = m_count + r_count;
    let mut adj: Vec<Vec<u32>> = Vec::with_capacity(n);

    for m in 0..m_count {
        adj.push(
            g.consumers[m]
                .iter()
                .filter(|&&r| g.is_plannable(r))
                .map(|&r| m_count as u32 + r)
                .collect(),
        );
    }
    for r in 0..r_count {
        if !g.is_plannable(r as u32) {
            adj.push(Vec::new());
            continue;
        }
        let mut outs: Vec<u32> = Vec::new();
        for slot in &g.recipes[r].outputs {
            for &(mm, _) in &slot.alts {
                outs.push(mm);
            }
        }
        outs.sort_unstable();
        outs.dedup();
        adj.push(outs);
    }
    (n, adj)
}

const NIL: u32 = u32::MAX;

/// 迭代版 Tarjan。
pub fn tarjan(n: usize, adj: &[Vec<u32>]) -> SccResult {
    let mut index: u32 = 0;
    let mut indices = vec![NIL; n];
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut comp = vec![NIL; n];
    let mut sizes: Vec<u32> = Vec::new();
    // 显式调用栈：(节点, 下一个待访问的子节点下标)
    let mut call: Vec<(u32, usize)> = Vec::new();

    for start in 0..n {
        if indices[start] != NIL {
            continue;
        }
        call.push((start as u32, 0));
        while let Some(&mut (v, ref mut ci)) = call.last_mut() {
            let v_us = v as usize;
            if *ci == 0 {
                indices[v_us] = index;
                lowlink[v_us] = index;
                index += 1;
                stack.push(v);
                on_stack[v_us] = true;
            }
            if *ci < adj[v_us].len() {
                let w = adj[v_us][*ci];
                *ci += 1;
                let w_us = w as usize;
                if indices[w_us] == NIL {
                    call.push((w, 0));
                } else if on_stack[w_us] {
                    lowlink[v_us] = lowlink[v_us].min(indices[w_us]);
                }
            } else {
                call.pop();
                if let Some(&(parent, _)) = call.last() {
                    let p = parent as usize;
                    lowlink[p] = lowlink[p].min(lowlink[v_us]);
                }
                if lowlink[v_us] == indices[v_us] {
                    let cid = sizes.len() as u32;
                    let mut size = 0u32;
                    loop {
                        let w = stack.pop().expect("SCC 栈不应为空");
                        on_stack[w as usize] = false;
                        comp[w as usize] = cid;
                        size += 1;
                        if w == v {
                            break;
                        }
                    }
                    sizes.push(size);
                }
            }
        }
    }

    SccResult {
        comp,
        sizes,
        node_count: n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::from_json_str;

    const FIXTURE: &str = r#"{
      "format": "test", "version": 1, "minecraft_version": "1.20.1",
      "categories": [
        {
          "type": "test:chem", "title": "Chem", "catalysts": [],
          "recipes": [
            {
              "id": "test:chem/loop_a",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:b", "count": 1}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:a", "count": 1}]}]
            },
            {
              "id": "test:chem/loop_b",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:a", "count": 1}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:b", "count": 1}]}]
            },
            {
              "id": "test:chem/raw_to_c",
              "inputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:ore", "count": 1}]}],
              "outputs": [{"name": "s", "ingredients": [{"type": "item", "id": "test:c", "count": 1}]}]
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn detects_cycle() {
        let g = from_json_str(FIXTURE).unwrap();
        let (n, adj) = build_adjacency(&g);
        let scc = tarjan(n, &adj);
        let m = g.materials.len();

        let a = g.find_material(crate::model::MaterialKind::Item, "test:a", None).unwrap();
        let b = g.find_material(crate::model::MaterialKind::Item, "test:b", None).unwrap();
        let c = g.find_material(crate::model::MaterialKind::Item, "test:c", None).unwrap();
        let ore = g.find_material(crate::model::MaterialKind::Item, "test:ore", None).unwrap();

        assert_eq!(scc.comp[a as usize], scc.comp[b as usize], "a 与 b 应在同一 SCC");
        assert!(scc.node_cyclic(a));
        assert!(scc.node_cyclic(b));
        assert!(!scc.node_cyclic(c));
        assert!(!scc.node_cyclic(ore));
        // 环内的两个配方也应在同一 SCC
        let r0 = 0u32;
        let r1 = 1u32;
        assert_eq!(scc.comp[m + r0 as usize], scc.comp[m + r1 as usize]);
    }
}
