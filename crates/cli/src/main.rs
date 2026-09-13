use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::MaterialKind;

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

    println!();
    println!("== 配方数 Top {} 分类 ==", top);
    let mut cats: Vec<usize> = (0..g.categories.len()).collect();
    cats.sort_by_key(|&i| std::cmp::Reverse(g.categories[i].recipe_count));
    for &i in cats.iter().take(top) {
        let c = &g.categories[i];
        println!(
            "{:>7}  {:<45} {}",
            fmt_num(c.recipe_count),
            c.ty,
            c.title
        );
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
