# gt-planner 详细设计文档

> 版本 0.1 · 2026-09-13 · 状态：Phase 0/1

## 1. 目标与范围

输入一份 JEI 导出的 GTCEu 配方集（`jei_recipes.json`），回答：

- **"我要每分钟造 X 个量子处理器，需要什么？"** → 原料清单 / 每步配方次数 / 机器数量 / 副产物
- **"这个材料有哪些路线？"** → 生产/消耗它的配方，槽位 OR 候选
- **"两条路线哪个省？"** → 启发式成本 + 支配剪枝 + 多方案对比（后续）

不做的事（当前阶段）：不模拟 tick 级物流、不处理电力网络拓扑、不依赖游戏内数据（时长/EU 由可选覆盖层提供）。

## 2. 数据源剖析

`jei_recipes.json`（54MB 紧凑 / 126MB 缩进，实测）：

```
Root
├── format: "gtmfo_jei_recipes"
├── version: 1
├── minecraft_version: "1.20.1"
├── exported_at, include_hidden
├── categories: [95 个]
│   ├── type: "gtceu:chemical_reactor"      ← 机器/配方类型
│   ├── title: "Chemical Reactor"
│   ├── recipe_class
│   ├── catalysts: [{type,id,count}]        ← 能跑该分类的机器
│   └── recipes: [~55582 个]
│       ├── id: "gtceu:chemical_reactor/acetic_acid_from_elements"
│       ├── inputs:  [Slot]
│       └── outputs: [Slot]
└── summary: {category_count:95, recipe_count:55582}
```

Slot 与 Ingredient：

```
Slot { name: "slot_4", ingredients: [Ingredient...] }   ← ingredients 是多选一（OR）
Ingredient 三种形态：
  {type:"item",  id, count}                 // 物品，count=需求数量
  {type:"fluid", id, amount}                // 流体，amount=mB
  {type:"<其他>", value}                    // 兜底（本数据集未出现，解析器仍支持）
  + 可选 nbt: SNBT 字符串                   // 实测 288,280 处
```

实测统计：item 509,302 次，fluid 17,900 次，无 `value` 形态，95 分类。

### 数据语义要点

1. **一个槽多候选 = OR**：规划时必须挑一个候选（默认取启发式最便宜的）。
2. **唯一键**：配方用 `(category_type, recipe_id)`；不同分类下 recipe id 可重复。
3. **NBT 即身份**：同 id 不同 NBT 是不同材料，主键含 NBT。
4. **无时长/EU**：JEI API 不含 chanced output 概率、配方时长、耗电。
   → 设计 `recipe_times.json` 覆盖层（可选）：
   `{"gtceu:chemical_reactor/acetic_acid_from_elements": {"duration_ticks": 200, "eu_per_tick": 30}}`
   无覆盖时机器数显示"未知"，只算物料与操作次数。
5. **信息类页面**：如 `gtceu:ore_crushing`（矿石处理信息）inputs/outputs 为空，按需过滤。
6. **空槽跳过**：解析时丢弃 ingredients 为空的槽。

## 3. 三层 IR

### IR-1 Knowledge IR（`model.rs` / `graph.rs`）

事实层，只描述"世界里有什么生产关系"：

```rust
MaterialKey { kind: MaterialKind, id: InternedStr, nbt: Option<InternedStr> }
// 字符串驻留：509k 次出现 → 仅 ~3 万个唯一 id 真正存字符串

MaterialInfo { key, producers: Vec<RecipeId>, consumers: Vec<RecipeId> }
RecipeNode   { category: CategoryId, id: String, inputs: Vec<Slot>, outputs: Vec<Slot> }
Slot         { name: String, alts: Vec<(MaterialId, u64)> }   // u64: 物品=个 / 流体=mB
CategoryInfo { ty, title, recipe_class, catalysts, recipe_count }

KnowledgeGraph {
    materials, recipes, categories,
    index: HashMap<MaterialKey, MaterialId>,
    recipe_index: HashMap<(CategoryId, String), RecipeId>,
    producers: Vec<Vec<RecipeId>>,   // 物化在 graph 上加速查询
    consumers: Vec<Vec<RecipeId>>,
    strings: Interner,
    stats: GraphStats,
}
```

### IR-2 Search IR（`algo/*`）

Knowledge 图天然是 **AND-OR 超图**，降级为 **二部有向图**供算法使用：

```
Material m ──consumed by──▶ Recipe r        (m → r)
Recipe r   ──produces────▶ Material m       (r → m)

例：Copper Plate ──▶ [Bender Recipe] ──▶ Copper Plate ×2
```

节点空间 = materials ∪ recipes（用 `u32` 偏移区分）。在此图上做：

- **SCC 压缩**（Tarjan，迭代版避免递归爆栈）：GT 有电解水↔氢氧这类环，压缩后成 DAG。
- **支配剪枝**：同产物两条配方 A、B，若 `inputs(A) ⊆ inputs(B)`（逐材料分量）且 A 不更贵 → 剪掉 B。
- **启发式成本数据库**：min-plus 值迭代，给出每个材料的
  - `unit_cost`：折算为"原始物品当量"（raw item = 1.0；流体 = 每桶 1.0）
  - `depth`：最小配方深度（raw = 0）
  - `cyclic`：是否处于 SCC 环中

### IR-3 Plan IR（`plan.rs`）

```rust
Plan {
    target, rate_per_min,
    recipes: [PlannedRecipe { recipe, category, ops_per_min,
                              machine_count: Option<f64>,   // 需时长覆盖层
                              inputs: [(Material, rate)], outputs: [...] }],
    raw_materials: [(Material, rate_per_min)],     // 最终需要挖的
    byproducts:    [(Material, rate_per_min)],     // 顺带产出的
    totals: { recipe_ops_per_min, distinct_recipes, total_raw_rate },
    meta: { mode, elapsed_ms, score, notes }       // notes 记录循环/深度截断
}
```

## 4. 求解器设计

### 4.1 确定性展开（tree mode，快速答案）

```
工作队列：demand: Map<Material, f64>
1. demand[target] = rate / primary_out_qty
2. 反复取 demand 最大的材料 m：
   - 若 m 是原料（无生产者）→ 记入 raw，清 demand
   - 否则选"每单位成本最低"的配方 r = best_recipe(m)（成本来自启发式 DB）
   - ops = demand[m] / out_qty(m)；对 r 每个输出 o：produced[o] += ops*qty
     若 demand[o] 已被满足则记为副产物
   - 对 r 每个输入槽：选最便宜候选 alt，demand[alt] += ops*alt_qty
3. 若 m 已被选过配方（环），直接按比例放大该配方 ops（线性缩放），迭代至收敛
4. 保护：max_depth / max_ops / 迭代上限；截断时写入 meta.notes
```

特点：O(展开节点数)，毫秒级；得到单一确定解，用于交互式查询。

### 4.2 Beam Search（beam mode，质量解）

状态：

```
SearchState {
    demand:   Map<Material, f64>,   // 未满足需求
    chosen:   Map<Material, RecipeId>,  // 每种材料选定的配方
    ops:      Map<RecipeId, f64>,
    raw:      Map<Material, f64>,
    score: f64, depth: usize,
}
f(state) = g + h
  g = Σ raw[m]·cost[m] + λ·Σ ops          // 已落实的原料 + 操作数罚项
  h = Σ demand[m]·cost[m]                  // 剩余需求的启发式估计
```

每层：选 `demand·cost` 最大的材料 → 枚举 top-N 候选配方（按单位成本排序）→ 子状态 → 打分 → 去重（demand 签名）→ 保留 top-K。终止：demand 空（complete）或达到迭代上限（best-effort，记录未满足需求）。

### 4.3 循环处理策略

- SCC 内的材料：允许"就地放大已选配方"，把环当成线性系统做迭代求解；
- 成本 DB 对纯环（无外部输入路径）标记 `INF`，规划时若被迫使用则记入 notes；
- Phase 6 可用 MILP/CP-SAT 精确求解环内配比（LP 松弛 + 整数配平）。

### 4.4 多目标（Phase 6 预留）

```
F = w_m·材料 + w_e·电耗 + w_t·时间 + w_r·机器数
输出 Pareto 前沿：最低材料 / 最低电耗 / 最快 三套方案
```

## 5. Web 架构

```
crates/server (axum)
  GET /api/stats                      数据集统计
  GET /api/categories                 95 分类列表
  GET /api/search?q=&limit=           材料搜索（前缀+包含）
  GET /api/material/{id}?kind=&nbt=   材料详情：生产/消耗配方（含槽 OR 展开）
  GET /api/recipe/{category}/{id}     配方详情
  GET /api/graph?material=&depth=     以材料为中心的子图（cytoscape 格式）
  POST /api/plan                      规划请求 → Plan IR
  GET  /                              静态前端（tower-http ServeDir）

web/ (零构建，原生 JS)
  ├── index.html   标签页：搜索 / 材料 / 规划器 / 图谱
  ├── app.js       前端逻辑（fetch API + 渲染）
  ├── style.css
  └── vendor/cytoscape.min.js   图谱可视化（优先本地 vendor，缺失则 CDN）
```

启动时解析一次 JSON → 构建 KnowledgeGraph → `Arc` 共享；解析耗时预期数秒、内存数百 MB（后续可加 bincode 缓存与 simd-json）。

## 6. 工程结构与里程碑

| 里程碑 | 内容 | 验收标准 | 状态 |
|---|---|---|---|
| M0 | 骨架 + git + 文档 | `cargo check` 通过，首次提交 | ✅ |
| M1 | parser + Knowledge IR + `gtp stats` | 真实 54MB JSON 解析成功，统计与 `summary` 一致 | ⬜ |
| M2 | SCC / 剪枝 / 启发式 + 确定性展开 | 单元测试（合成小数据）+ 真实数据 `gtp plan` 出结果 | ⬜ |
| M3 | Beam Search + Plan IR | beam 解 ≤ tree 解成本；JSON 输出稳定 | ⬜ |
| M4 | axum API + 前端 | 浏览器可搜索/规划/看图，冒烟测试通过 | ⬜ |
| M5 | wgpu GPU 加速 | 大规模候选评估并行化（可选，视规模） | ⬜ |
| M6 | MILP/CP-SAT + Pareto | 多目标输出 | ⬜ |

### 依赖预算（保持精简）

- core：`serde` `serde_json` `thiserror`（无 async、无 unsafe 依赖）
- cli：`clap` + core
- server：`axum` `tokio` `tower-http` + core
- 测试：`cargo test` 单元测试 + `tests/` 集成测试（合成 JSON fixture）

### 风险与对策

| 风险 | 对策 |
|---|---|
| 54MB JSON 解析慢/占内存 | 先直读全量；后续 simd-json / 自定义流式 Visitor / bincode 缓存 |
| 无时长/EU 数据 | `recipe_times.json` 覆盖层；UI 明确显示"未知"而非瞎猜 |
| 环内配方线性迭代不收敛 | 迭代上限 + notes 标注 + 后续 LP 精确解 |
| 候选槽组合爆炸 | 启发式选 cheapest alt 默认；beam 中限量分支 |
| 材料 id 命名混乱 | 提供 display 名（去命名空间/可读化），前端搜索支持模糊匹配 |

## 7. 参考资料

- 用户提供的数据 schema 说明（见 §2）
- 架构灵感：Factorio Helmod / EDA compiler（前端→IR→优化→后端）
- GTCEu recipe 语义：AND-OR 超图，循环存在于化工链（电解、化工循环）
