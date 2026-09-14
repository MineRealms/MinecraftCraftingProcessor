# 进度跟踪

> 规则：每轮开发完成后更新此文档 + `git commit` + `cargo build/check` 验证。
> 图例：✅ 完成 · 🔄 进行中 · ⬜ 未开始 · ⚠️ 有已知问题

## 当前状态

- 最新提交：`M12 学术化 README + Benchmark 图表`
- 编译状态：✅ `cargo build --workspace` + `cargo test`（core 15 + gpu 3 单测全过）
- README：CRN 形式化（化学计量矩阵 / SCC 商图 / Bellman 价值函数 / Pareto / LP-MILP / Petri 网映射）
  + 6 张基准图表（`docs/bench/`，由 `scripts/gen_benchmarks.py` 生成）

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

### M4 Web API + 前端 ✅
- [x] axum 路由：stats / categories / search / material / graph / plan(POST)
- [x] tower-http 静态托管 `web/`（cytoscape 已本地 vendor）
- [x] 前端四页：搜索 / 材料详情（槽 OR 展开）/ 规划器 / 图谱（cytoscape）
- [x] 冒烟测试：自动启动 → 全部端点验证 → 自动关闭
- [x] 提交 + 编译验证

**启动方式**（常驻服务，Ctrl+C 停止）：
```
cargo run -p gt-planner-server --release -- --data H:\Tools\jei_recipes.json
# 浏览器打开 http://127.0.0.1:8787
```

### M12 学术化 README + Benchmark 图表 ✅
- [x] README 重写为论文风格：问题定义（CRN / 化学计量矩阵）→ 数学形式化（SCC 商图、
      cycle-safe 价值函数、多目标 Pareto、LP/MILP、chance constraint、Timed Stochastic Petri Net 映射）
      → 算法 → 工程 → Benchmarks → 路线图
- [x] `scripts/gen_benchmarks.py` 生成 6 张基准图（`docs/bench/`）：
      求解质量对比 / 规模扩展（log-log）/ Pareto 前沿 / GPU 加速 + BiCGSTAB 收敛 /
      SCC 规模分布 + 分类分布 / LP 下界 gap
- [x] README 末尾标注：§5 基准数据为**基准模拟（synthetic baseline）**，真实实测值见 §7
- [x] 提交 + 推送

### M11 日志 / 性能计数器 / 运行记录 ✅
- [x] **结构化日志**：core/gpu 关键路径 `log` 埋点（构图、Search IR、LP、BiCGSTAB 批量）；
      CLI `-v` / `RUST_LOG`；服务器默认 info（含每次规划请求日志）
- [x] **性能计数器 `PlanMetrics`**：贯穿 Plan IR → CLI 输出 → API → 前端：
      展开次数 / 评估次数 / 轮次 / GPU 候选数 / GPU 求解数 / 收敛数 / 迭代合计 /
      LP 变量数 / 约束数 / 状态 / 目标值 / 分析耗时
- [x] **运行记录 `PlanRecord`（JSONL）**：
      - CLI `--record <path>` 追加一条
      - 服务器 `--log-dir logs` 自动记录每次规划（`runs.jsonl`）
      - `GET /api/history?limit=N` 读取最近记录
      - 时间戳自实现 UTC ISO8601（零依赖，含单测）
- [x] **`GET /api/metrics`**：数据集/分析（SCC、循环分量、成本迭代）/ 运行时计数器
      （请求数、平均耗时、模式分布、记录数、运行时长）
- [x] **前端更新**：
      - 高级选项面板（Beam 宽度 / 候选数 / 轮数 / MCTS 模拟 / 允许回收 / 屏蔽放大环）
      - 运行指标面板（自动刷新）+ 运行历史表（点击回填目标/速率/模式）
      - 概率产出徽章（`chance < 1` 显示百分比）
      - 流程图在语言切换时正确重渲染；计划结果展示性能指标行
- [x] 冒烟：3 种模式记录写入 `runs.jsonl`、`/api/history`、`/api/metrics`、前端元素齐全

### M10 GPU 矩阵求解器升级：BiCGSTAB ✅
- [x] `gpu/linear.rs` 重写：**BiCGSTAB**（双共轭梯度稳定化，处理非对称 A）替代朴素定点迭代；
      每候选一个 workgroup（256 线程），迭代内 SpMV + 归约点积；breakdown 保护
- [x] **真实残差校验**：循环结束后重算 ‖rhs − A·x‖ 再判定收敛
      （BiCGSTAB 可能"假收敛"：中间步 ‖s‖ 恰好很小但并非解，发散系统尤甚）
- [x] beam 评估器（`eval.rs`）改为直接构建 CSR 系统 + BiCGSTAB 批量求解 + CPU 评分；
      未收敛解做钳制（1e6）+ 温和罚分（1e3），保留排序信息
- [x] `gtp flow` 输出 收敛/迭代/GPU残差/CPU残差；未收敛时给出"放大环"提示
- [x] 单测：CSR 构建 / spmv / consumed 反推 / CPU Jacobi 收敛（3 个）
- [x] 实测：QP 固定贪心分配含回收放大环 → 正确报告"未收敛（残差 90）"而非假收敛；
      beam(GPU) 46.6 vs beam(CPU) 97.8（同口径）

### M9 P2 收官（评审剩余项） ✅
- [x] **Process IR**（`process.rs`）：物料流图——步骤节点（ops/机器/等级/EU）+ 流边（速率）+
      外部输入/目标/副产物；`gtp plan --process` 文本渲染；`POST /api/process`；前端"流程图"按钮
      （cytoscape 分层渲染，按流图自动分层）
- [x] **副产物概率覆盖层**（`chances.rs`）：`jei_chances.json`（recipe/output/chance），
      产出按期望值计入 tree/LP/Plan（PlanEntry 带 `chance` 字段）；无数据时明确注明"按 100% 计"
- [x] **MCTS**（`planner/mcts.rs`）：UCT 平衡利用/探索，节点=配方分配、动作=单材料换配方、
      评估=完整展开；保证不劣于 tree 基线；`--mode mcts --mcts-iterations N`；实测 300 次模拟 83ms，
      得分 7192 → 69.6（balanced 权重）
- [x] **GPU 线性求解器**（`gpu/linear.rs`）：CSR 稀疏矩阵 + `x ← x + ω·(rhs − A·x)` 松弛 Jacobi，
      批量 dispatch（每候选一套系统）；`gtp flow` 命令直接展示物料平衡解；
      发散检测（材料放大环）并给出 ω/模式建议
- [x] 文档同步更新（PROGRESS/README/PLAN）

**模型边界（如实记录）**：`gtp flow` 的固定贪心分配若包含"放大环"（如回收类），
线性系统谱半径 > 1 会发散（ω<1 只能减缓）；评估器通过钳制 + 发散罚分处理，
精确方案请用 exact（排除回收）/beam。

### M8 过程优化升级（响应外部评审的 P0/P1） ✅
- [x] **P0 CostVector**：成本从标量升级为 `{material, eu, machine}` 三维向量；
      路线选择用加权综合分，权重预设 `balanced/economy/power/speed`（CLI `--objective` / API / 前端下拉）
- [x] **P0 EU 进入第一层模型**：EU 与机器时间计入成本库与 LP 目标函数；
      `gtp pareto` 输出省料/省电/省机时三方案对比
- [x] **P0 凝聚图一级公民**：`algo/condensation.rs`（SCC → 超节点 DAG：材料/配方分组、拓扑序、上下游）
- [x] **P0 OR 槽延迟决策**：LP 内为多候选槽建"填充变量"`Σ f_i = ops_r`，由优化器联合选择（不再贪心）
- [x] **P1 MILP-lite**：机器数 = ceil(ops×duration/1200)，Plan/前端同时展示小数与整数台数
- [x] **P1 成本分解**：计划输出 目标分 = w_m·原料 + w_eu·EU/min + w_machine·机器数
- [x] **P1 子图构建重写（关键 bug 修复）**：
      1) `is_source`：producers 非空但全是标签页/信息页的矿石此前被误判为"中间产物"（进口价 ×1000），修复后整条链可自产；
      2) best-first 按 **(深度, 成本)** 排序 + 每材料全量生产者（上限 32）：纯按成本会陷入"微型粉↔小堆粉"封闭家族，永远够不到矿石链；
      3) 中间产物"进口"惩罚（原料 1.0 / 中间产物 1000×）：LP 被迫从原料自产全链。
- [x] 实测：钛粉 exact 从"直接进口"变为 38 配方自产；QP exact 130 配方 / 64.3 原料/min，原料全部为真实矿石

### M7 GT 数据 + 中英文 + tier 过滤 ✅（2026-09-13 追加）
- [x] 解析 `gt` 需求块：duration / eut / amperage / energy_io / tier / tier_index / total_eu
- [x] 解析 `jei_names.json`（12,953 条：machines/materials/blocks/items/fluids 中英文 + 机器 tier）
- [x] Plan IR：每步机器数（ops × duration / 1200）、EU/t、等级、EU/min；汇总机器总数/净功率/耗能/发电
- [x] 电压等级过滤：`--max-tier LV` / API `max_tier` / 前端下拉（超等级的配方不可用）
- [x] CLI/API 全链路中英文（MaterialDto: display_zh / display_en）
- [x] 前端：中英切换（默认中文，localStorage 记忆）；大图（>400 节点）自动不渲染名称；计划表新增机器数/EU/tier 列
- [x] 数据说明：燃料类配方（269 条）无能量字段；概率产出 JEI 不暴露

**实测**：QP 60/min tree = 119 配方 / 279 台机器 / 净功率 -703 EU/t（等离子发电）；`--max-tier LV` 时无法生产（正确降级为外部输入）。

### M5 GPU（wgpu） ✅
- [x] 新 crate `crates/gpu`：wgpu 27 计算管线（RTX 4060 实测；无 GPU 时软件回退/CPU 回退）
- [x] WGSL 核：对每份候选选择表并行做"物料平衡定点迭代"，输出粗筛得分
- [x] 子图**从基线方案构建**（chosen + top-N 替代配方的闭包），保证候选完整记账
- [x] `BatchEvaluator` 抽象：core 定义 trait，GPU 实现；beam 路径 = GPU 全量扰动粗筛 → CPU 精评 top-48 → CPU 采样兜底（保证不劣于纯 CPU）
- [x] CLI `--no-gpu`；服务器 beam 自动使用 GPU
- [x] 实测：QP 60/min 4656 个候选 GPU 粗筛（1.4s，成本 29.31，优于纯 CPU 30.62）

### 图谱优化 ✅（M4.5）
- [x] 服务端：流向修正（输入 → 配方 → 产物）、方向 up/down/both、每配方 top-N 输入/输出剪枝、节点上限、边数量标签、点击跳转材料
- [x] 前端：自研分层布局（层内重心排序，避免堆叠）、筛选器（隐藏回收/只显示材料/隐藏标签/高亮过滤）、`min-zoomed-font-size` 大图保护、批量更新

### M6 多目标优化 ⬜
- [ ] 权重化 F = w_m·M + w_e·E + w_t·T + w_r·R
- [ ] Pareto 前沿输出
- [ ] （可选）MILP / CP-SAT 精确求解

### M6a LP 精确求解 ✅（2026-09-13 追加）
- [x] `solver/mod.rs`：物料平衡 LP（good_lp + microlp 纯 Rust 后端）
- [x] 变量：配方操作量 + 每种材料的外部输入；目标：外部输入按成本库价格计费 + ε·操作量
- [x] 子图构建（目标上游 BFS，可配置上限），循环由 LP 自动处理
- [x] 默认排除回收类配方；可选 `--block-amplification` 保守模式
- [x] 方案含循环配方时明确警告（物料模型无耗电，可能利用放大环）
- [x] CLI `--mode exact` / API `mode: "exact"` / 前端模式选择
- [x] 实测：QP 60/min 目标值 38.99（Optimal，654ms）；铜 3.33 原料/min

**模型边界（重要）**：LP 是"纯物料平衡"模型的精确解；由于没有 EU/时长数据，
它可能利用游戏里靠耗电阻止的材料放大环。要"保守但非最优"的方案用 beam 模式。

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
