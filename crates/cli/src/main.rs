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
    /// 生产计划：每分钟造 N 个目标产物
    Plan {
        /// 材料 id
        material: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        nbt: Option<String>,
        /// 目标速率（物品=个/分，流体=mB/分）
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
        /// 模式：tree（确定性展开）/ beam（局部搜索）/ exact（LP 精确求解）
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

fn load(data: &PathBuf) -> Result<KnowledgeGraph, Box<dyn std::error::Error>> {
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
    let g = gt_planner_core::parser::load_file(data)?;
    eprintln!("解析 + 构图完成，用时 {:.2}s", t0.elapsed().as_secs_f64());
    Ok(g)
}

fn build_analysis(g: &KnowledgeGraph) -> Analysis {
    eprintln!("构建 Search IR（SCC / 成本 / 剪枝）...");
    let t0 = Instant::now();
    let an = Analysis::build(g);
    eprintln!(
        "分析完成：{} 轮迭代，收敛={}，用时 {:.2}s",
        an.cost.iterations,
        an.cost.converged,
        t0.elapsed().as_secs_f64()
    );
    an
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
        println!(
            "{:<8} {:<45} {:>6} {:>6}  {}{}",
            kind_s,
            full,
            g.producers[id as usize].len(),
            g.consumers[id as usize].len(),
            info.display,
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
    println!("名称            {}", info.display);
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
    println!(
        "材料 {} ({})，名称 {}",
        g.material_id_str(m),
        g.kind_str(info.key.kind),
        info.display
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
    include_recycling: bool,
    block_amplification: bool,
    no_gpu: bool,
    verbose: bool,
    json_out: Option<&PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let plan = match mode {
        "tree" => {
            let req = PlanRequest {
                target: m,
                rate_per_min: rate,
                max_ops,
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
                ..Default::default()
            };
            gt_planner_core::solver::plan_exact(g, an, m, rate, &opts)
                .map_err(|e| format!("exact 求解失败: {e}"))?
        }
        other => {
            return Err(format!("不支持的模式 \"{}\"（tree / beam / exact）", other).into());
        }
    };

    println!("== 生产计划 ==");
    println!(
        "目标        {} ({}) x {}/min",
        plan.target.display,
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
        println!(
            "  {:>10} op/min  [{}] {}",
            fmt_rate(p.ops_per_min),
            p.category_title,
            p.recipe
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

    if let Some(path) = json_out {
        let text = serde_json::to_string_pretty(&plan)?;
        std::fs::write(path, text)?;
        println!();
        println!("计划 JSON 已写入 {}", path.display());
    }

    Ok(())
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Stats { top } => {
            let g = load(&resolve_data(cli.data))?;
            cmd_stats(&g, *top);
        }
        Command::Find { query, limit, kind } => {
            let g = load(&resolve_data(cli.data))?;
            cmd_find(&g, query, *limit, kind.as_deref());
        }
        Command::Info {
            material,
            kind,
            nbt,
            limit,
        } => {
            let g = load(&resolve_data(cli.data))?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let an = build_analysis(&g);
            cmd_info(&g, &an, m, *limit);
        }
        Command::Recipes {
            material,
            kind,
            nbt,
            limit,
        } => {
            let g = load(&resolve_data(cli.data))?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            cmd_recipes(&g, m, *limit);
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
            max_ops,
            include_recycling,
            block_amplification,
            no_gpu,
            verbose,
            json,
        } => {
            let g = load(&resolve_data(cli.data))?;
            let m = resolve_material(
                &g,
                material,
                kind.as_deref().and_then(kind_from_str),
                nbt.as_deref(),
            )?;
            let an = build_analysis(&g);
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
                *include_recycling,
                *block_amplification,
                *no_gpu,
                *verbose,
                json.as_ref(),
            )?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}
