# gt-planner

**GregTech 工业生产规划器** —— 把 JEI 配方数据编译成生产知识图谱，再用约束求解器算出最优工厂方案。

> 定位：不是"配方查询器"，而是一个 mini 的 **AI-driven EDA / Operations Research Engine**。
> 输入 `jei_recipes.json`（GTCEu 导出，95 分类 / 5.5 万配方），输出"每分钟 X 个目标产物"的完整生产计划。

## 核心管线

```
JEI JSON
   │  parser（解析 + 字符串驻留）
   ▼
Knowledge IR（材料 / 配方 / 分类 的事实库）
   │  graph lowering（二部图 + 生产者/消费者索引）
   ▼
Search IR（Material ⇄ Recipe 二部图，AND-OR 超图）
   │  algo（SCC 压缩 / 支配剪枝 / 启发式成本数据库）
   ▼
Planner（确定性展开 + Beam Search 混合）
   │
   ▼
Plan IR（每分钟配方次数 / 机器数 / 原料清单 / 副产物）
   │
   ▼
CLI（gtp） + Web API（axum） + 前端可视化（图谱 / 规划器）
```

## 关键设计决策

| 决策 | 说明 |
|---|---|
| **不用配方树** | GregTech 存在循环（电解水↔氢氧），树会无限展开；用 AND-OR 二部超图表示 |
| **三层 IR 分离** | Knowledge IR（事实）/ Search IR（算法用）/ Plan IR（结果）互不污染 |
| **槽 OR 语义** | 一个槽的 `ingredients` 是多选一；规划器需要按启发式成本选择候选 |
| **NBT 参与主键** | 288k 处 NBT，同 id 不同 NBT 是不同材料；材料键 = (kind, id, nbt) |
| **成本单位归一** | 物品 = 个，流体 = 桶(1000mB)，统一切换为"原始物品当量" |
| **时长/EU 数据缺失** | JEI 不导出配方时长与耗电 → 预留 `recipe_times.json` 覆盖层，无覆盖时只算物料与次数 |
| **GPU 只做状态评估** | 图算法（Tarjan/DFS）留在 CPU；GPU 负责大规模候选状态打分与 Top-K（Phase 3） |

## 目录结构

```
gt-planner/
├── Cargo.toml               # workspace
├── docs/
│   ├── PLAN.md              # 详细架构设计（必读）
│   └── PROGRESS.md          # 进度跟踪（每轮更新）
├── crates/
│   ├── core/                # 纯 Rust 核心库：parser / ir / graph / algo / planner
│   ├── cli/                 # gtp 命令行工具
│   └── server/              # axum Web 服务 + 静态前端托管
└── web/                     # 零构建前端（原生 JS + cytoscape.js）
```

## 快速开始

```powershell
# 编译
cargo build

# 查看数据集统计
cargo run -p gt-planner-cli --release -- stats --data H:\Tools\jei_recipes.json

# 查询材料
cargo run -p gt-planner-cli --release -- find "circuit" --data H:\Tools\jei_recipes.json

# 规划：每分钟 60 个量子处理器
cargo run -p gt-planner-cli --release -- plan gtceu:quantum_processor --rate 60 --data H:\Tools\jei_recipes.json

# 启动 Web 服务
cargo run -p gt-planner-server --release -- --data H:\Tools\jei_recipes.json
# 打开 http://127.0.0.1:8787
```

## 开发阶段

- [x] Phase 0 项目骨架 + 文档
- [x] Phase 1 解析器 + Knowledge Graph + CLI stats（57MB JSON ~1.5s）
- [x] Phase 2 SCC / 剪枝 / 启发式 + 确定性展开
- [x] Phase 3 Beam Search 规划器 + Plan IR
- [x] Phase 4 Web API + 前端可视化（axum + cytoscape，含图谱筛选/分层布局）
- [x] Phase 5 GPU 加速（wgpu）：候选批量评估 + **CSR 线性求解器**（`gtp flow`）
- [x] Phase 6 LP 精确求解（good_lp + microlp，OR 槽内决策 + MILP-lite 机器取整）
- [x] Phase 7 过程优化升级：CostVector（物料/EU/机器时间）、SCC 凝聚图一级公民、
      多目标预设（economy/power/speed）、Process IR 物料流图、概率产出覆盖层
- [x] Phase 8 MCTS 规划模式（UCT）+ 多方案对比（`gtp pareto`）
- [ ] Phase 9 未来：GPU simplex/interior-point、多根 MCTS、Pareto 前沿自动搜索

规划模式：`tree`（毫秒级）/ `beam`（GPU 粗筛 + CPU 局部搜索）/ `mcts`（蒙特卡洛树搜索）/ `exact`（LP 精确）

详见 `docs/PLAN.md` 与 `docs/PROGRESS.md`（含数据踩坑记录）。
