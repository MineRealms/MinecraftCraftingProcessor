# 进度跟踪

> 规则：每轮开发完成后更新此文档 + `git commit` + `cargo build/check` 验证。
> 图例：✅ 完成 · 🔄 进行中 · ⬜ 未开始 · ⚠️ 有已知问题

## 当前状态

- 最新提交：`M0 骨架与文档`
- 编译状态：✅ `cargo check`（M0 时点）
- 真实数据验证：⬜ 尚未跑通 54MB 数据集

## 里程碑看板

### M0 项目骨架 + 文档 ✅
- [x] workspace 目录结构（core / cli / server / web）
- [x] `git init`（分支 main）
- [x] README.md（项目说明）
- [x] docs/PLAN.md（详细架构设计）
- [x] docs/PROGRESS.md（本文件）
- [x] .gitignore
- [x] 首次提交 + `cargo check` 验证

### M1 解析器 + Knowledge IR ⬜
- [ ] `model.rs`：MaterialKey / Slot / RecipeNode / CategoryInfo / 字符串驻留
- [ ] `parser.rs`：serde 直读 → IR 构建（含 NBT / OR 槽 / 空槽跳过）
- [ ] `graph.rs`：生产者/消费者索引 + GraphStats
- [ ] CLI `gtp stats`：解析真实数据，统计与 JSON summary 对照
- [ ] 合成 fixture 单元测试
- [ ] 提交 + 编译验证 + 真实数据冒烟

### M2 图算法 + 确定性展开 ⬜
- [ ] `algo/scc.rs`：迭代版 Tarjan
- [ ] `algo/cost.rs`：min-plus 值迭代成本/深度数据库
- [ ] `algo/prune.rs`：支配剪枝
- [ ] `planner/tree.rs`：工作队列确定性展开 + 环内线性缩放
- [ ] CLI `gtp find` / `gtp recipes` / `gtp plan --mode tree`
- [ ] 提交 + 编译验证

### M3 Beam Search + Plan IR ⬜
- [ ] `plan.rs`：Plan IR 序列化
- [ ] `planner/beam.rs`：状态展开 / f=g+h / 去重 / Top-K
- [ ] CLI `gtp plan --mode beam`
- [ ] tree vs beam 对照测试
- [ ] 提交 + 编译验证

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
