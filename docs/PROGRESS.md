# 进度跟踪

> 规则：每轮开发完成后更新此文档 + `git commit` + `cargo build/check` 验证。
> 图例：✅ 完成 · 🔄 进行中 · ⬜ 未开始 · ⚠️ 有已知问题

## 当前状态

- 最新提交：`M3 Beam Search + Plan IR`
- 编译状态：✅ `cargo build --workspace` + `cargo test`（13 个单测全过）
- 真实数据验证：✅ 解析 ~1.6s；分析 ~0.25s；tree 规划 ~1ms；beam ~1.1s
- 量子处理器 60/min：tree 成本 66.1 → beam 30.6（beam 以 tree 为基线做配方分配局部搜索）

## 里程碑看板

### M0 项目骨架 + 文档 ✅
- [x] workspace 目录结构（core / cli / server / web）
- [x] `git init`（分支 main）
- [x] README.md（项目说明）
- [x] docs/PLAN.md（详细架构设计）
- [x] docs/PROGRESS.md（本文件）
- [x] .gitignore
- [x] 首次提交 + `cargo check` 验证

### M1 解析器 + Knowledge IR ✅
- [x] `model.rs`：MaterialKey / Slot / RecipeNode / CategoryInfo / 字符串驻留
- [x] `parser.rs`：serde 直读 → IR 构建（含 NBT / OR 槽 / 空槽跳过）
- [x] `graph.rs`：生产者/消费者索引 + GraphStats + DatasetMeta
- [x] CLI `gtp stats` / `gtp find`：解析真实数据，统计与 JSON summary 对照
- [x] 合成 fixture 单元测试（8 个）
- [x] 提交 + 编译验证 + 真实数据冒烟

**实测数据（126MB 缩进版）**：解析+构图 13.3s（debug）；30,145 材料（item 29,508 / fluid 637）、19,909 个 NBT 变体、55,582 配方（与 summary 一致）、消耗/产出链接 377,176 / 113,482、原料 1,876、叶子 17,016、空配方 0。

### M2 图算法 + 确定性展开 ✅
- [x] `algo/scc.rs`：迭代版 Tarjan（只含可规划配方，避免标签页造成巨型 SCC）
- [x] `algo/cost.rs`：SCC 拓扑序 + 入口定价 + 分量内 BFS 的成本数据库
- [x] `algo/prune.rs`：支配剪枝（按产物记录，不全局删除）
- [x] `planner/tree.rs`：工作队列确定性展开 + 环内线性缩放 + 发散/循环保险
- [x] CLI `gtp info` / `gtp recipes` / `gtp plan --mode tree`
- [x] 提交 + 编译验证

**成本模型踩坑记录（重要设计决策）**：
1. 值迭代会被"材料放大环"（1 锭 → 2 线 → 2 缆 → 电解回收 288mB 铜液 ≈ 2 锭）把成本收缩到 0 并污染全图 → 改为 SCC 拓扑序定价；
2. 数据里 `minecraft:tag_recipes/*` 是标签成员列表页（1 输入 → 1835 输出），必须排除出规划；
3. `multiblock_info` 等 `*_info` 分类是结构信息页，排除；
4. `raw_copper` 等"挖矿获得"的材料存在"粗矿↔粗矿块"压缩环且无外部入口 → 无入口 SCC 整体按世界资源定价（成本 1.0）；
5. 回收类配方成本乘 4 惩罚，避免"造工具再拆解"成为首选路线；
6. 并列裁决：入口锚点 > 非回收 > 更浅深度 > 非同一 SCC。

**已知局限**：无配方时长/EU 数据 → 机器数量与电耗未计算；同成本路线的经济性判断仅靠启发式。

### M3 Beam Search + Plan IR ✅
- [x] `plan.rs`：Plan IR 序列化（配方步骤 / 原料 / 副产物 / 汇总 / 备注）
- [x] `planner/mod.rs`：tree/beam 共享 Plan 组装
- [x] `planner/beam.rs`：**配方分配局部搜索式 Beam**（以贪心解为基线，扰动配方选择，完整展开评估，保留 top-K）
- [x] CLI `gtp plan --mode beam`（`--beam-width/--candidates/--max-iterations`）
- [x] 真实数据对照：QP 60/min，tree 66.1 → beam 30.6 原始物品当量（3712 次候选评估，1.1s）

**设计说明**：最初实现了"逐步需求展开"式 beam，实测在循环数据上退化（永动机解、前沿耗尽）。
改为"配方分配 + 完整展开评估"式局部搜索：任何时刻都有完整方案、保证不劣于 tree、
天然可 GPU 并行（批量评估候选选择表）。

### M4 Web API + 前端 ⬜
- [ ] axum 路由（stats / search / material / recipe / graph / plan）
- [ ] tower-http 静态托管 web/
- [ ] 前端四页：搜索 / 材料详情 / 规划器 / 图谱
- [ ] 端到端冒烟：启动服务 + 浏览器可达
- [ ] 提交 + 编译验证

### M5 GPU（wgpu） ⬜
- [ ] 候选状态批量评估 kernel
- [ ] GPU Top-K（radix sort）
- [ ] 与 CPU 版本结果对照

### M6 多目标优化 ⬜
- [ ] 权重化 F = w_m·M + w_e·E + w_t·T + w_r·R
- [ ] Pareto 前沿输出
- [ ] （可选）MILP / CP-SAT 精确求解

## 开发日志

### 2026-09-13 · M0
- 环境：Rust 1.98.1 / cargo 1.98.1 / git 2.51.0（Windows）
- 数据集剖析：95 分类、55,582 配方；ingredient 实测 item 509,302 处、fluid 17,900 处、NBT 288,280 处、无 value 兜底形态
- 决策：NBT 进入材料主键；字符串驻留；时长/EU 走可选覆盖层
- 产出：骨架 + 三份文档 + 首次提交

## 已知问题 / 待办

- ⬜ 54MB JSON 全量解析的性能与内存占用待实测（M1）
- ⬜ 时长/EU 覆盖层 `recipe_times.json` 的加载逻辑（M2 后）
- ⬜ 前端 cytoscape 本地 vendor 是否可用（M4 前确认，缺失则用 CDN）
