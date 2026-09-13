use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use gt_planner_core::analysis::Analysis;
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::{MaterialId, MaterialKind};
use gt_planner_core::planner::{plan_tree, PlanRequest};

#[derive(Parser)]
#[command(
    name = "gtp",
    version,
    about = "GregTech 工业生产规划器：JEI 配方 → 生产计划"
)]
struct Cli {
    /// 配方 JSON 路径（默认：环境变量 GTP_DATA，否则 ./jei_recipes.json）
    #[arg(long, short, global = true)]
    data: Option<PathBuf>,

    /// 名称 JSON 路径（默认：与配方同目录的 jei_names.json）
    #[arg(long, global = true)]
    names: Option<PathBuf>,

    /// 概率产出覆盖表路径（默认：与配方同目录的 jei_chances.json，可选）
    #[arg(long, global = true)]
    chances: Option<PathBuf>,

    /// 输出调试日志（等价于 RUST_LOG=debug）
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 数据集统计（解析真实 JSON 并输出图谱概况）
    Stats {
        /// 展示配方数最多的前 N 个分类
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// 模糊搜索材料
    Find {
        /// 搜索词（匹配完整 id 或可读名，大小写不敏感）
        query: String,
        /// 结果上限
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// 过滤类型：item / fluid
        #[arg(long)]
        kind: Option<String>,
    },
    /// 查看材料的成本 / 深度 / 配方列表（构建完整分析）
    Info {
        /// 材料 id（如 gtceu:quantum_processor）
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 生产者/消费者展示上限
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 列出生产 / 消耗某材料的配方
    Recipes {
        /// 材料 id
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 结果上限
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// 多目标对比：同一目标用 economy/power/speed 预设各规划一次并对比
    Pareto {
        /// 材料 id
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 目标速率
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
        /// 规划器：tree（快）/ exact（LP）
        #[arg(long, default_value = "tree")]
        mode: String,
        #[arg(long)]
        max_tier: Option<String>,
    },
    /// GPU 线性求解：物料平衡矩阵（固定贪心配方分配），展示流量解
    Flow {
        /// 材料 id
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 目标速率
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
        /// 迭代次数
        #[arg(long, default_value_t = 200)]
        iterations: u32,
        /// 收敛容差（残差范数）
        #[arg(long, default_value_t = 1e-5)]
        tol: f32,
        /// 松弛因子 ω（仅 CPU 回退的 Jacobi 使用；1.0 = 标准 Jacobi）
        #[arg(long, default_value_t = 1.0)]
        omega: f32,
        /// 展示前 N 行（按操作量降序）
        #[arg(long, default_value_t = 25)]
        top: usize,
        #[arg(long)]
        max_tier: Option<String>,
        /// 强制 CPU 回退
        #[arg(long)]
        no_gpu: bool,
    },
    /// 生产计划：每分钟造 N 个目标产物
    Plan {        /// 材料 id
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 目标速率（物品=个/分，流体=mB/分）
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
        /// 模式：tree（确定性展开）/ beam（局部搜索）/ mcts（蒙特卡洛树搜索）/ exact（LP 精确求解）
        #[arg(long, default_value = "tree")]
        mode: String,
        /// Beam 前沿宽度（保留的候选选择表数）
        #[arg(long, default_value_t = 8)]
        beam_width: usize,
        /// Beam 每材料候选替代配方数
        #[arg(long, default_value_t = 3)]
        candidates: usize,
        /// Beam 局部搜索轮数
        #[arg(long, default_value_t = 12)]
        max_iterations: usize,
        /// MCTS 模拟次数
        #[arg(long, default_value_t = 400)]
        mcts_iterations: usize,
        /// 展开操作上限（tree 模式）
        #[arg(long, default_value_t = 200_000)]
        max_ops: usize,
        /// exact 模式：允许回收类配方（默认关闭，纯物料模型下回收环可能刷材料）
        #[arg(long)]
        include_recycling: bool,
        /// exact 模式：屏蔽"循环+材料放大"配方（保守，可能误伤正常制造）
        #[arg(long)]
        block_amplification: bool,
        /// beam 模式：禁用 GPU 批量评估（强制 CPU 路径）
        #[arg(long)]
        no_gpu: bool,
        /// 最大电压等级（等级名 LV/MV/HV/EV/IV/LuV/ZPM/UV/UHV… 或数字）
        #[arg(long)]
        max_tier: Option<String>,
        /// 多目标预设：balanced / economy（省料）/ power（省电）/ speed（省时间）
        #[arg(long)]
        objective: Option<String>,
        /// 额外输出 Process IR（物料流图）
        #[arg(long)]
        process: bool,
        /// 追加一条运行记录（JSONL）到指定文件
        #[arg(long)]
        record: Option<PathBuf>,
        /// 展开每个配方的输入输出明细
        #[arg(long)]
        verbose: bool,
        /// 导出计划 JSON 到文件
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

fn resolve_data(arg: Option<PathBuf>) -> PathBuf {
    arg.or_else(|| std::env::var_os("GTP_DATA").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("jei_recipes.json"))
}

/// 名称文件解析：显式参数 > 环境变量 > 与配方同目录的 jei_names.json。
fn resolve_names(data: &PathBuf, arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return p.exists().then_some(p);
    }
    if let Some(p) = std::env::var_os("GTP_NAMES").map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    let sibling = data.with_file_name("jei_names.json");
    sibling.exists().then_some(sibling)
}

/// 概率覆盖表解析（可选）：显式参数 > 环境变量 > 与配方同目录的 jei_chances.json。
fn resolve_chances(data: &PathBuf, arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return p.exists().then_some(p);
    }
    if let Some(p) = std::env::var_os("GTP_CHANCES").map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    let sibling = data.with_file_name("jei_chances.json");
    sibling.exists().then_some(sibling)
}

/// 解析 --max-tier（等级名或数字）。
fn parse_max_tier(s: &str) -> Option<u8> {
    if let Ok(n) = s.trim().parse::<u8>() {
        return Some(n);
    }
    gt_planner_core::util::tier_index_from_name(s)
}

/// 千分位格式化。
fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 速率格式化。
fn fmt_rate(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else if v >= 1000.0 {
        format!("{:.0}", v)
    } else if v >= 100.0 {
        format!("{:.1}", v)
    } else if v >= 1.0 {
        format!("{:.2}", v)
    } else {
        format!("{:.5}", v)
    }
}

fn kind_from_str(s: &str) -> Option<MaterialKind> {
    match s {
        "item" => Some(MaterialKind::Item),
        "fluid" => Some(MaterialKind::Fluid),
        _ => None,
    }
}

fn load(
    data: &PathBuf,
    names: Option<&PathBuf>,
    chances: Option<&PathBuf>,
) -> Result<KnowledgeGraph, Box<dyn std::error::Error>> {
    if !data.exists() {
        return Err(format!(
            "找不到数据文件：{}\n提示：用 --data <路径> 指定，或设置环境变量 GTP_DATA",
            data.display()
        )
        .into());
    }
    let size_mb = std::fs::metadata(data)?.len() as f64 / 1_048_576.0;
    eprintln!("读取 {} ({:.1} MB) ...", data.display(), size_mb);
    let t0 = Instant::now();
    let mut g = match names {
        Some(np) => {
            eprintln!("读取名称库 {} ...", np.display());
            gt_planner_core::parser::load_with_names(data, np)?
        }
        None => gt_planner_core::parser::load_file(data)?,
    };
    if let Some(cp) = chances {
        eprintln!("读取概率覆盖表 {} ...", cp.display());
        g.chances = gt_planner_core::ChanceStore::load(cp)?;
    }
    eprintln!(
        "解析 + 构图完成，用时 {:.2}s（名称 {} 条 / 概率覆盖 {} 条）",
        t0.elapsed().as_secs_f64(),
        g.names.len(),
        g.chances.len()
    );
    Ok(g)
}

fn build_analysis(g: &KnowledgeGraph, weights: gt_planner_core::CostWeights) -> Analysis {
    eprintln!(
        "构建 Search IR（SCC / 凝聚图 / CostVector / 剪枝；权重 m={} eu={} machine={}）...",
        weights.material, weights.eu, weights.machine
    );
    let t0 = Instant::now();
    let an = Analysis::build_with(g, 48, weights);
    eprintln!(
        "分析完成：{} 轮迭代，收敛={}，用时 {:.2}s",
        an.cost.iterations,
        an.cost.converged,
        t0.elapsed().as_secs_f64()
    );
    an
}

/// 解析 --objective 预设。
fn parse_objective(s: Option<&str>) -> gt_planner_core::CostWeights {
    match s {
        Some(name) => gt_planner_core::CostWeights::preset(name).unwrap_or_else(|| {
            eprintln!("警告：未知 --objective \"{}\"（balanced/economy/power/speed），使用 balanced", name);
            gt_planner_core::CostWeights::default()
        }),
        None => gt_planner_core::CostWeights::default(),
    }
}

/// 解析材料：精确匹配（kind 缺省时先 item 后 fluid），失败给出搜索建议。
fn resolve_material(
    g: &KnowledgeGraph,
    name: &str,
    kind: Option<MaterialKind>,
    nbt: Option<&str>,
) -> Result<MaterialId, String> {
    let mut try_kinds: Vec<MaterialKind> = Vec::new();
    match kind {
        Some(k) => try_kinds.push(k),
        None => {
            try_kinds.push(MaterialKind::Item);
            try_kinds.push(MaterialKind::Fluid);
        }
    }
    for k in &try_kinds {
        if let Some(m) = g.find_material(*k, name, nbt) {
            return Ok(m);
        }
    }
    let hits = g.search(name, kind, 5);
    if hits.is_empty() {
        Err(format!("找不到材料 \"{}\"（可先用 `gtp find` 搜索）", name))
    } else {
        let mut msg = format!("找不到精确匹配的材料 \"{}\"，你是不是想要：\n", name);
        for h in hits {
            let info = g.material(h);
            msg.push_str(&format!(
                "  {} ({}, 生产者 {} 个)\n",
                g.material_id_str(h),
                g.kind_str(info.key.kind),
                g.producers[h as usize].len()
            ));
        }
        Err(msg)
    }
}

fn cmd_stats(g: &KnowledgeGraph, top: usize) {
    let m = &g.meta;
    let s = &g.stats;
    println!("== 数据集 ==");
    println!("format          {} v{}", m.format, m.version);
    println!("minecraft       {}", m.minecraft_version);
    println!("exported_at     {}", m.exported_at);
    println!("include_hidden  {}", m.include_hidden);

    println!();
    println!("== 分类 / 配方 ==");
    match (m.summary_category_count, m.summary_recipe_count) {
        (Some(c), Some(r)) => {
            println!(
                "分类            {} (JSON summary: {})",
                fmt_num(s.category_count),
                fmt_num(c as usize)
            );
            println!(
                "配方            {} (JSON summary: {})",
                fmt_num(s.recipe_count),
                fmt_num(r as usize)
            );
        }
        _ => {
            println!("分类            {}", fmt_num(s.category_count));
            println!("配方            {}", fmt_num(s.recipe_count));
        }
    }
    println!("信息类(空)配方  {}", fmt_num(s.empty_recipe_count));

    println!();
    println!("== 材料 ==");
    println!(
        "材料总数        {}  (item {} / fluid {} / other {})",
        fmt_num(s.material_count),
        fmt_num(s.item_count),
        fmt_num(s.fluid_count),
        fmt_num(s.other_count)
    );
    println!("NBT 变体        {}", fmt_num(s.nbt_variant_count));
    println!(
        "消耗/产出链接   {} / {}",
        fmt_num(s.consume_links),
        fmt_num(s.produce_links)
    );
    println!("原料(无生产者)  {}", fmt_num(s.raw_material_count));
    println!("叶子(无消费者)  {}", fmt_num(s.leaf_material_count));
    println!(
        "可规划配方      {} (排除 diagram/tag/info)",
        fmt_num(s.plannable_recipe_count)
    );
    println!("可采集材料      {}", fmt_num(s.harvestable_material_count));

    println!();
    println!("== 配方数 Top {} 分类 ==", top);
    let mut cats: Vec<usize> = (0..g.categories.len()).collect();
    cats.sort_by_key(|&i| std::cmp::Reverse(g.categories[i].recipe_count));
    for &i in cats.iter().take(top) {
        let c = &g.categories[i];
        println!("{:>7}  {:<45} {}", fmt_num(c.recipe_count), c.ty, c.title);
    }
}

fn cmd_find(g: &KnowledgeGraph, query: &str, limit: usize, kind: Option<&str>) {
    let kf = kind.and_then(kind_from_str);
    if kind.is_some() && kf.is_none() {
        eprintln!("警告：无法识别的 --kind（仅支持 item / fluid），忽略该过滤条件");
    }
    let hits = g.search(query, kf, limit);
    if hits.is_empty() {
        println!("没有匹配 \"{}\" 的材料", query);
        return;
    }
    println!(
        "{:<8} {:<45} {:>6} {:>6}  {}",
        "类型", "ID", "生产者", "消费者", "名称"
    );
    for id in hits {
        let info = g.material(id);
        let kind_s = g.kind_str(info.key.kind);
        let full = g.material_id_str(id);
        let nbt_mark = if info.key.nbt.is_some() { " [NBT]" } else { "" };
        let zh = g.names.zh_or(full, &info.display);
        let en = g.names.en_or(full, &info.display);
        let name = if zh == en {
            zh.to_string()
        } else {
            format!("{} / {}", zh, en)
        };
        println!(
            "{:<8} {:<45} {:>6} {:>6}  {}{}",
            kind_s,
            full,
            g.producers[id as usize].len(),
            g.consumers[id as usize].len(),
            name,
            nbt_mark
        );
    }
}

fn fmt_opt_u32(v: u32) -> String {
    if v == u32::MAX {
        "-".to_string()
    } else {
        v.to_string()
    }
}

fn cmd_info(g: &KnowledgeGraph, an: &Analysis, m: MaterialId, limit: usize) {
    let info = g.material(m);
    println!(
        "材料            {} ({})",
        g.material_id_str(m),
        g.kind_str(info.key.kind)
    );
    let full = g.material_id_str(m);
    let zh = g.names.zh_or(full, &info.display);
    let en = g.names.en_or(full, &info.display);
    if zh == en {
        println!("名称            {}", zh);
    } else {
        println!("名称            {} / {}", zh, en);
    }
    let c = an.cost.unit_cost[m as usize];
    if c.is_finite() {
        println!("成本估计        {:.4} 原始物品当量/单位", c);
    } else {
        println!("成本估计        不可达（生产链依赖循环输入）");
    }
    println!("最小深度        {}", fmt_opt_u32(an.cost.depth[m as usize]));
    println!(
        "循环成员        {}",
        if an.material_cyclic(m) { "是" } else { "否" }
    );
    match an.cost.best_recipe[m as usize] {
        Some(r) => println!("最佳配方        {} (成本最优)", g.recipe_full_id(r)),
        None => println!("最佳配方        无（原料）"),
    }
    let dominated_count = g.producers[m as usize]
        .iter()
        .filter(|&&r| an.is_dominated_for(r, m))
        .count();
    println!("被支配候选      {} 条", dominated_count);

    println!();
    println!("生产配方 ({})", g.producers[m as usize].len());
    for &rid in g.producers[m as usize].iter().take(limit) {
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        let dom = if an.is_dominated_for(rid, m) {
            " [被支配]"
        } else {
            ""
        };
        let out_q = gt_planner_core::util::output_qty_of(r, m).unwrap_or(0);
        let unit = if out_q > 0 {
            let rc = gt_planner_core::util::recipe_cost(
                g,
                &an.cost.unit_cost,
                Some(&an.cost.cyclic),
                r,
            );
            let nq = gt_planner_core::util::mat_norm_qty(g, m, out_q);
            if rc.is_finite() && nq > 0.0 {
                let c = rc / nq;
                if c < 1e-3 {
                    format!("成本 {:.3e}", c)
                } else {
                    format!("成本 {:.4}", c)
                }
            } else {
                "成本 -".to_string()
            }
        } else {
            "成本 -".to_string()
        };
        println!(
            "  x{:<4} {:<40} [{}] {}{}",
            out_q,
            g.recipe_full_id(rid),
            cat.title,
            unit,
            dom
        );
        for slot in &r.inputs {
            let alts: Vec<String> = slot
                .alts
                .iter()
                .map(|&(am, aq)| {
                    let ainfo = g.material(am);
                    format!(
                        "{}x{}{}",
                        g.material_id_str(am),
                        aq,
                        if ainfo.key.kind.is_fluid() { "mB" } else { "" }
                    )
                })
                .collect();
            println!("      {}: {}", slot.name, alts.join(" | "));
        }
    }
    let prod_rest = g.producers[m as usize].len().saturating_sub(limit);
    if prod_rest > 0 {
        println!("  ... 还有 {} 条", prod_rest);
    }

    println!();
    println!("消耗配方 ({})", g.consumers[m as usize].len());
    for &rid in g.consumers[m as usize].iter().take(limit) {
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        let out_q = gt_planner_core::util::output_qty_of(r, m);
        let main_out = r
            .primary_output()
            .map(|(om, oq)| format!("{}x{}", g.material_id_str(om), oq))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<40} [{}] -> {}{}",
            g.recipe_full_id(rid),
            cat.title,
            main_out,
            out_q
                .map(|q| format!(" (含 {}x{})", g.material_id_str(m), q))
                .unwrap_or_default()
        );
    }
    let cons_rest = g.consumers[m as usize].len().saturating_sub(limit);
    if cons_rest > 0 {
        println!("  ... 还有 {} 条", cons_rest);
    }
}

fn cmd_recipes(g: &KnowledgeGraph, m: MaterialId, limit: usize) {
    let info = g.material(m);
    let full = g.material_id_str(m);
    println!(
        "材料 {} ({})，名称 {} / {}",
        full,
        g.kind_str(info.key.kind),
        g.names.zh_or(full, &info.display),
        g.names.en_or(full, &info.display)
    );
    println!();
    println!("== 生产配方 ({}) ==", g.producers[m as usize].len());
    for &rid in g.producers[m as usize].iter().take(limit) {
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        println!("  {}  [{}]", g.recipe_full_id(rid), cat.title);
    }
    let rest = g.producers[m as usize].len().saturating_sub(limit);
    if rest > 0 {
        println!("  ... 还有 {} 条", rest);
    }
    println!();
    println!("== 消耗配方 ({}) ==", g.consumers[m as usize].len());
    for &rid in g.consumers[m as usize].iter().take(limit) {
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        println!("  {}  [{}]", g.recipe_full_id(rid), cat.title);
    }
    let rest = g.consumers[m as usize].len().saturating_sub(limit);
    if rest > 0 {
        println!("  ... 还有 {} 条", rest);
    }
}

fn cmd_plan(
    g: &KnowledgeGraph,
    an: &Analysis,
    m: MaterialId,
    rate: f64,
    mode: &str,
    max_ops: usize,
    beam_width: usize,
    candidates: usize,
    max_iterations: usize,
    mcts_iterations: usize,
    include_recycling: bool,
    block_amplification: bool,
    no_gpu: bool,
    max_tier: Option<u8>,
    show_process: bool,
    record_out: Option<&PathBuf>,
    verbose: bool,
    json_out: Option<&PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut plan = match mode {
        "tree" => {
            let req = PlanRequest {
                target: m,
                rate_per_min: rate,
                max_ops,
                max_tier,
            };
            plan_tree(g, an, &req)
        }
        "beam" => {
            let opts = gt_planner_core::planner::BeamOptions {
                beam_width,
                candidate_limit: candidates,
                max_iterations,
                sample_per_state: 24,
                ops_penalty: 0.001,
                max_tier,
            };
            // GPU 批量评估（不可用则自动回退 CPU）
            let mut evaluator = if no_gpu {
                None
            } else {
                match gt_planner_gpu::GpuEvaluator::new() {
                    Ok(ev) => {
                        eprintln!("GPU 评估器就绪: {}", ev.adapter_name());
                        Some(ev)
                    }
                    Err(e) => {
                        eprintln!("{}；回退 CPU 路径", e);
                        None
                    }
                }
            };
            match evaluator.as_mut() {
                Some(ev) => gt_planner_core::planner::plan_beam_with_evaluator(
                    g, an, m, rate, &opts, Some(ev),
                ),
                None => gt_planner_core::planner::plan_beam(g, an, m, rate, &opts),
            }
        }
        "exact" => {
            let opts = gt_planner_core::solver::ExactOptions {
                include_recycling,
                block_amplification,
                max_tier,
                ..Default::default()
            };
            gt_planner_core::solver::plan_exact(g, an, m, rate, &opts)
                .map_err(|e| format!("exact 求解失败: {e}"))?
        }
        "mcts" => {
            let opts = gt_planner_core::planner::MctsOptions {
                iterations: mcts_iterations,
                candidate_limit: candidates,
                max_tier,
                ..Default::default()
            };
            gt_planner_core::planner::plan_mcts(g, an, m, rate, &opts)
        }
        other => {
            return Err(format!("不支持的模式 \"{}\"（tree / beam / mcts / exact）", other).into());
        }
    };

    plan.metrics.analysis_ms = an.build_ms;

    println!("== 生产计划 ==");
    println!(
        "目标        {} ({}) x {}/min",
        plan.target.display_zh,
        plan.target.id,
        fmt_rate(plan.rate_per_min)
    );
    println!("模式        {} | 耗时 {:.1} ms", plan.mode, plan.elapsed_ms);
    println!(
        "配方种类    {} | 总操作 {}/min | 估算成本 {} 原始物品当量",
        plan.totals.distinct_recipes,
        fmt_rate(plan.totals.recipe_ops_per_min),
        fmt_rate(plan.totals.estimated_cost)
    );
    if plan.totals.total_machines > 0.0 {
        println!(
            "机器总数    {:.1} 台（整数 {}）| 耗电 {}/t | 发电 {}/t | 净功率 {}/t | 净能量 {}/min",
            plan.totals.total_machines,
            plan.totals.total_machines_int,
            fmt_rate(plan.totals.consume_eu_t),
            fmt_rate(plan.totals.generate_eu_t),
            fmt_rate(plan.totals.net_eu_t),
            fmt_rate(plan.totals.net_eu_per_min)
        );
    }
    println!(
        "目标分      {:.2}（物料×{:.2} + EU×{:e} + 机器×{:.2}）",
        plan.totals.objective_score,
        an.cost.weights.material,
        an.cost.weights.eu,
        an.cost.weights.machine
    );
    if plan.totals.raw_fluids_mb_per_min > 0.0 {
        println!(
            "原料汇总    物品 {}/min + 流体 {}/min(mB)",
            fmt_rate(plan.totals.raw_items_per_min),
            fmt_rate(plan.totals.raw_fluids_mb_per_min)
        );
    }

    println!();
    println!("-- 配方步骤（按操作量降序）--");
    for p in &plan.recipes {
        let extra = match (p.machine_count, p.machine_count_int, p.eut, p.tier.as_deref()) {
            (Some(mc), Some(mi), Some(eu), Some(t)) => {
                format!("  |  {:.2} 台({})  {:.0} EU/t  [{}]", mc, mi, eu, t)
            }
            (Some(mc), _, _, _) => format!("  |  {:.2} 台", mc),
            _ => String::new(),
        };
        println!(
            "  {:>10} op/min  [{}] {}{}",
            fmt_rate(p.ops_per_min),
            p.category_title,
            p.recipe,
            extra
        );
        if verbose {
            for e in &p.inputs {
                println!(
                    "        入  {:>10}/min  {}",
                    fmt_rate(e.rate_per_min),
                    e.material.id
                );
            }
            for e in &p.outputs {
                println!(
                    "        出  {:>10}/min  {}",
                    fmt_rate(e.rate_per_min),
                    e.material.id
                );
            }
        }
    }

    println!();
    println!("-- 原料（需要开采 / 外部输入）--");
    if plan.raw_materials.is_empty() {
        println!("  （无）");
    }
    for e in &plan.raw_materials {
        println!(
            "  {:>10}/min  {} ({})",
            fmt_rate(e.rate_per_min),
            e.material.id,
            e.material.kind
        );
    }

    if !plan.byproducts.is_empty() {
        println!();
        println!("-- 副产物 --");
        for e in &plan.byproducts {
            println!(
                "  {:>10}/min  {} ({})",
                fmt_rate(e.rate_per_min),
                e.material.id,
                e.material.kind
            );
        }
    }

    if !plan.notes.is_empty() {
        println!();
        println!("-- 备注 --");
        for n in &plan.notes {
            println!("  · {}", n);
        }
    }

    let m = &plan.metrics;
    println!();
    println!(
        "性能指标    展开 {} | 评估 {} | 轮次 {} | GPU 候选 {} / 求解 {}（收敛 {}，迭代合计 {}）| 分析 {:.0}ms",
        fmt_num(m.expansions),
        fmt_num(m.evaluations),
        fmt_num(m.rounds),
        fmt_num(m.gpu_candidates),
        fmt_num(m.gpu_solves),
        fmt_num(m.gpu_converged),
        fmt_num(m.gpu_iters_total as usize),
        m.analysis_ms
    );
    if let Some(vars) = m.lp_variables {
        println!(
            "LP 规模     {} 变量 / {} 约束 / 状态 {} / 目标值 {}",
            fmt_num(vars),
            fmt_num(m.lp_constraints.unwrap_or(0)),
            m.lp_status.as_deref().unwrap_or("-"),
            m.lp_objective.map(fmt_rate).unwrap_or_else(|| "-".to_string())
        );
    }

    if show_process {
        let pg = gt_planner_core::process::ProcessGraph::from_plan(&plan);
        print_process(&pg);
    }

    if let Some(path) = record_out {
        let rec = gt_planner_core::PlanRecord::from_plan(&plan, None, max_tier);
        gt_planner_core::PlanRecord::append_jsonl(path, &rec)?;
        println!();
        println!("运行记录已追加到 {}", path.display());
    }

    if let Some(path) = json_out {
        let text = serde_json::to_string_pretty(&plan)?;
        std::fs::write(path, text)?;
        println!();
        println!("计划 JSON 已写入 {}", path.display());
    }

    Ok(())
}

/// 渲染 Process IR（物料流图）为文本。
fn print_process(pg: &gt_planner_core::process::ProcessGraph) {
    println!();
    println!("-- 工艺流程图（Process IR）--");
    println!("步骤 ({}):", pg.steps.len());
    for s in &pg.steps {
        let extra = match (s.machine_count, s.tier.as_deref(), s.eu_t) {
            (Some(mc), Some(t), Some(eu)) => format!("  [{:.1} 台 | {:.0} EU/t | {}]", mc, eu, t),
            (Some(mc), _, _) => format!("  [{:.1} 台]", mc),
            _ => String::new(),
        };
        println!(
            "  #{:<3} {:>10} op/min  [{}] {}{}",
            s.index + 1,
            fmt_rate(s.ops_per_min),
            s.category_title,
            s.recipe,
            extra
        );
    }
    println!("流 ({}):", pg.flows.len());
    for f in &pg.flows {
        let from = match f.from_step {
            Some(i) => format!("#{}", i + 1),
            None => "[原料]".to_string(),
        };
        let to = match f.to_step {
            Some(i) => format!("#{}", i + 1),
            None => {
                if f.material.id == pg.target.id {
                    "(目标)".to_string()
                } else {
                    "(副产物)".to_string()
                }
            }
        };
        println!(
            "  {} → {:>10}/min  {}  → {}",
            from,
            fmt_rate(f.rate_per_min),
            f.material.id,
            to
        );
    }
    if !pg.raw_inputs.is_empty() {
        println!("外部输入 ({}):", pg.raw_inputs.len());
        for f in &pg.raw_inputs {
            println!(
                "  [原料] → {:>10}/min  {} → #{}",
                fmt_rate(f.rate_per_min),
                f.material.id,
                f.to_step.map(|i| i + 1).unwrap_or(0)
            );
        }
    }
    if !pg.byproducts.is_empty() {
        println!("副产物 ({}):", pg.byproducts.len());
        for f in &pg.byproducts {
            println!(
                "  #{} → {:>10}/min  {}",
                f.from_step.map(|i| i + 1).unwrap_or(0),
                fmt_rate(f.rate_per_min),
                f.material.id
            );
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let data = resolve_data(cli.data.clone());
    let names = resolve_names(&data, cli.names.clone());
    let chances = resolve_chances(&data, cli.chances.clone());
    match &cli.command {
        Command::Stats { top } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            cmd_stats(&g, *top);
        }
        Command::Find { query, limit, kind } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            cmd_find(&g, query, *limit, kind.as_deref());
        }
        Command::Info {
            material,
            kind,
            nbt,
            limit,
        } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let an = build_analysis(&g, gt_planner_core::CostWeights::default());
            cmd_info(&g, &an, m, *limit);
        }
        Command::Recipes {
            material,
            kind,
            nbt,
            limit,
        } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            cmd_recipes(&g, m, *limit);
        }
        Command::Pareto {
            material,
            kind,
            nbt,
            rate,
            mode,
            max_tier,
        } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let mt = max_tier.as_deref().and_then(parse_max_tier);
            println!(
                "== 多目标对比：{} × {}/min（mode={}）==",
                g.material_dto(m).display_zh,
                fmt_rate(*rate),
                mode
            );
            println!(
                "{:<10} {:>12} {:>16} {:>10} {:>12} {:>8}",
                "预设", "原料/min", "净EU/min", "机器数", "目标分", "耗时ms"
            );
            for name in ["economy", "power", "speed"] {
                let weights = gt_planner_core::CostWeights::preset(name).unwrap();
                let an = build_analysis(&g, weights);
                let plan = match mode.as_str() {
                    "exact" => {
                        let opts = gt_planner_core::solver::ExactOptions {
                            max_tier: mt,
                            ..Default::default()
                        };
                        gt_planner_core::solver::plan_exact(&g, &an, m, *rate, &opts)
                            .map_err(|e| format!("exact 求解失败: {e}"))?
                    }
                    _ => {
                        let req = PlanRequest {
                            target: m,
                            rate_per_min: *rate,
                            max_ops: 200_000,
                            max_tier: mt,
                        };
                        plan_tree(&g, &an, &req)
                    }
                };
                println!(
                    "{:<10} {:>12.2} {:>16.0} {:>10.1} {:>12.2} {:>8.0}",
                    name,
                    plan.totals.estimated_cost,
                    plan.totals.net_eu_per_min,
                    plan.totals.total_machines,
                    plan.totals.objective_score,
                    plan.elapsed_ms
                );
            }
        }
        Command::Flow {
            material,
            kind,
            nbt,
            rate,
            iterations,
            tol,
            omega,
            top,
            max_tier,
            no_gpu,
        } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let an = build_analysis(&g, gt_planner_core::CostWeights::default());
            let mt = max_tier.as_deref().and_then(parse_max_tier);
            let req = PlanRequest {
                target: m,
                rate_per_min: *rate,
                max_ops: 200_000,
                max_tier: mt,
            };
            // 贪心分配 → 评估子图 → 物料平衡线性系统
            let choices = gt_planner_core::planner::greedy_choices(&g, &an, &req);
            let sg = gt_planner_gpu::build_subgraph_from_plan(&g, &an, m, &choices, 3);
            let mut demand = vec![0f32; sg.materials.len()];
            if let Some(&ti) = sg.mat_index.get(&m) {
                demand[ti] = *rate as f32;
            }
            let cand = gt_planner_gpu::CandidateTable::from_choices(&sg, &choices)
                .ok_or("无法构建候选表（子图不完整）")?;
            let sys = gt_planner_gpu::FlowSystem::from_candidate(&sg, &cand, &demand);

            println!("== GPU 线性求解（BiCGSTAB：物料平衡 A·x = rhs）==");
            println!(
                "材料 {} / 配方 {} / nnz {} / 迭代上限 {} / tol {}",
                sg.materials.len(),
                sg.recipes.len(),
                sys.col_idx.len(),
                iterations,
                tol
            );

            let t0 = Instant::now();
            let res: gt_planner_gpu::SolveResult = if *no_gpu {
                let x = sys.solve_cpu(*iterations, *omega);
                let residual = sys.residual(&x);
                gt_planner_gpu::SolveResult {
                    x,
                    iterations: *iterations,
                    residual,
                    converged: residual <= *tol,
                }
            } else {
                match gt_planner_gpu::GpuFlowSolver::new() {
                    Ok(solver) => {
                        eprintln!("GPU 求解器就绪: {}", solver.adapter_name);
                        solver
                            .solve_batch(std::slice::from_ref(&sys), *iterations, *tol)
                            .remove(0)
                    }
                    Err(e) => {
                        eprintln!("{}；CPU 回退", e);
                        let x = sys.solve_cpu(*iterations, *omega);
                        let residual = sys.residual(&x);
                        gt_planner_gpu::SolveResult {
                            x,
                            iterations: *iterations,
                            residual,
                            converged: residual <= *tol,
                        }
                    }
                }
            };
            let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
            let x = &res.x;
            let cpu_residual = sys.residual(x);
            println!(
                "收敛 {} / 迭代 {} / GPU残差 {:.3e} / CPU残差 {:.3e}",
                if res.converged { "是" } else { "否" },
                res.iterations,
                res.residual,
                cpu_residual
            );

            // 输出：按操作量降序
            let consumed = sys.consumed(&x);
            let mut rows: Vec<(usize, f32, f32, f32)> = (0..sys.n)
                .map(|i| (i, x[i], sys.out_qty[i], consumed[i]))
                .filter(|(_, xv, _, _)| *xv > 1e-6)
                .collect();
            rows.sort_by(|a, b| b.1.total_cmp(&a.1));
            println!();
            println!(
                "{:<45} {:>12} {:>10} {:>12}",
                "材料", "操作量/min", "产出/op", "消耗/min"
            );
            for &(i, xv, oq, cons) in rows.iter().take(*top) {
                let mid = g.material_id_str(sg.materials[i]);
                let zh = g.names.zh_or(mid, mid);
                println!(
                    "{:<45} {:>12.4} {:>10.4} {:>12.4}",
                    format!("{} ({})", mid, zh),
                    xv,
                    oq,
                    cons
                );
            }
            if rows.len() > *top {
                println!("  ... 还有 {} 行", rows.len() - top);
            }
            let score = sys.score(&x, &sg.norm_factor, 0.001);
            let max_x = x.iter().cloned().fold(0f32, f32::max);
            println!();
            println!(
                "解：{} 行非零 / 得分 {:.4} / 耗时 {:.1} ms",
                rows.len(),
                score,
                elapsed
            );
            if max_x > 1e6 {
                println!(
                    "⚠ 解发散（存在材料放大环）：最大操作量 {:.3e}；建议更小 ω 或改用 exact/beam 模式",
                    max_x
                );
            } else if !res.converged {
                println!(
                    "⚠ 未收敛（残差 {:.3e}）：固定贪心分配可能包含放大环；建议改用 exact/beam 模式",
                    cpu_residual
                );
            }
        }
        Command::Plan {
            material,
            kind,
            nbt,
            rate,
            mode,
            beam_width,
            candidates,
            max_iterations,
            mcts_iterations,
            max_ops,
            include_recycling,
            block_amplification,
            no_gpu,
            max_tier,
            objective,
            process,
            record,
            verbose,
            json,
        } => {
            let g = load(&data, names.as_ref(), chances.as_ref())?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let weights = parse_objective(objective.as_deref());
            let an = build_analysis(&g, weights);
            let mt = max_tier.as_deref().and_then(parse_max_tier);
            if max_tier.is_some() && mt.is_none() {
                eprintln!("警告：无法识别的 --max-tier（等级名或数字），忽略");
            }
            cmd_plan(
                &g,
                &an,
                m,
                *rate,
                mode,
                *max_ops,
                *beam_width,
                *candidates,
                *max_iterations,
                *mcts_iterations,
                *include_recycling,
                *block_amplification,
                *no_gpu,
                mt,
                *process,
                record.as_ref(),
                *verbose,
                json.as_ref(),
            )?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let default_level = if cli.verbose { "debug" } else { "info" };
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(default_level),
    )
    .format_timestamp_millis()
    .try_init();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}
