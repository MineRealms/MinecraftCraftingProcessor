//! gt-planner-server：axum Web API + 静态前端托管。
//!
//! 启动时加载 JEI JSON → 构建 KnowledgeGraph + Analysis，之后全部请求共享。

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use gt_planner_core::analysis::Analysis;
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::{MaterialId, MaterialKind, RecipeId};
use gt_planner_core::planner::{plan_beam, plan_tree, BeamOptions, PlanRequest};
use gt_planner_core::plan::Plan;

struct AppState {
    graph: KnowledgeGraph,
    analysis: Analysis,
    loaded_at: Instant,
}

type St = State<Arc<AppState>>;

// ---------------------------------------------------------------------------
// 通用错误响应
// ---------------------------------------------------------------------------

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

fn kind_from_str(s: &str) -> Option<MaterialKind> {
    match s {
        "item" => Some(MaterialKind::Item),
        "fluid" => Some(MaterialKind::Fluid),
        _ => None,
    }
}

fn resolve_material(
    g: &KnowledgeGraph,
    name: &str,
    kind: Option<MaterialKind>,
    nbt: Option<&str>,
) -> Result<MaterialId, ApiError> {
    let kinds: Vec<MaterialKind> = match kind {
        Some(k) => vec![k],
        None => vec![MaterialKind::Item, MaterialKind::Fluid],
    };
    for k in kinds {
        if let Some(m) = g.find_material(k, name, nbt) {
            return Ok(m);
        }
    }
    let suggestions: Vec<String> = g
        .search(name, kind, 5)
        .into_iter()
        .map(|m| g.material_id_str(m).to_string())
        .collect();
    Err(not_found(format!(
        "找不到材料 \"{}\"{}",
        name,
        if suggestions.is_empty() {
            String::new()
        } else {
            format!("；你是不是想要：{}", suggestions.join(", "))
        }
    )))
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RecipeDto {
    id: String,
    category: String,
    category_title: String,
    inputs: Vec<gt_planner_core::SlotDto>,
    outputs: Vec<gt_planner_core::SlotDto>,
}

fn recipe_dto(g: &KnowledgeGraph, rid: RecipeId) -> RecipeDto {
    let r = g.recipe(rid);
    let cat = g.category(r.category);
    RecipeDto {
        id: g.recipe_full_id(rid),
        category: cat.ty.clone(),
        category_title: cat.title.clone(),
        inputs: r.inputs.iter().map(|s| g.slot_dto(s)).collect(),
        outputs: r.outputs.iter().map(|s| g.slot_dto(s)).collect(),
    }
}

#[derive(Serialize)]
struct MaterialDetail {
    material: gt_planner_core::MaterialDto,
    unit_cost: Option<f64>,
    depth: Option<u32>,
    cyclic: bool,
    best_recipe: Option<String>,
    producer_count: usize,
    consumer_count: usize,
    producers: Vec<RecipeDto>,
    consumers: Vec<RecipeDto>,
}

#[derive(Serialize)]
struct GraphNode {
    data: GraphNodeData,
}

#[derive(Serialize)]
struct GraphNodeData {
    id: String,
    label: String,
    node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    material_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    /// 材料完整 id（如 gtceu:copper_ingot），用于点击跳转
    #[serde(skip_serializing_if = "Option::is_none")]
    mat_id: Option<String>,
}

#[derive(Serialize)]
struct GraphEdge {
    data: GraphEdgeData,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn api_stats(State(st): St) -> Json<serde_json::Value> {
    let g = &st.graph;
    Json(serde_json::json!({
        "meta": g.meta,
        "stats": g.stats,
        "analysis": {
            "build_ms": st.analysis.build_ms,
            "cost_converged": st.analysis.cost.converged,
            "load_ms": st.loaded_at.elapsed().as_secs_f64() * 1000.0,
        }
    }))
}

async fn api_categories(State(st): St) -> Json<serde_json::Value> {
    let cats: Vec<serde_json::Value> = st
        .graph
        .categories
        .iter()
        .map(|c| {
            serde_json::json!({
                "type": c.ty,
                "title": c.title,
                "recipe_count": c.recipe_count,
                "catalyst_count": c.catalysts.len(),
            })
        })
        .collect();
    Json(serde_json::json!({ "categories": cats }))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
    kind: Option<String>,
}

async fn api_search(State(st): St, Query(p): Query<SearchParams>) -> Result<Json<serde_json::Value>, ApiError> {
    let g = &st.graph;
    let limit = p.limit.unwrap_or(40).min(200);
    let kf = p.kind.as_deref().and_then(kind_from_str);
    let hits = g.search(&p.q, kf, limit);
    let items: Vec<serde_json::Value> = hits
        .iter()
        .map(|&m| {
            let dto = g.material_dto(m);
            serde_json::json!({
                "material": dto,
                "producers": g.producers[m as usize].len(),
                "consumers": g.consumers[m as usize].len(),
                "unit_cost": if st.analysis.cost.unit_cost[m as usize].is_finite() {
                    Some(st.analysis.cost.unit_cost[m as usize])
                } else { None },
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": items })))
}

#[derive(Deserialize)]
struct MaterialParams {
    kind: Option<String>,
    nbt: Option<String>,
    limit: Option<usize>,
}

async fn api_material(
    State(st): St,
    Path(id): Path<String>,
    Query(p): Query<MaterialParams>,
) -> Result<Json<MaterialDetail>, ApiError> {
    let g = &st.graph;
    let m = resolve_material(g, &id, p.kind.as_deref().and_then(kind_from_str), p.nbt.as_deref())?;
    let limit = p.limit.unwrap_or(60).min(300);

    let producers = g.producers[m as usize]
        .iter()
        .take(limit)
        .map(|&r| recipe_dto(g, r))
        .collect();
    let consumers = g.consumers[m as usize]
        .iter()
        .take(limit)
        .map(|&r| recipe_dto(g, r))
        .collect();

    let unit_cost = st.analysis.cost.unit_cost[m as usize];
    let depth = st.analysis.cost.depth[m as usize];
    Ok(Json(MaterialDetail {
        material: g.material_dto(m),
        unit_cost: unit_cost.is_finite().then_some(unit_cost),
        depth: (depth != u32::MAX).then_some(depth),
        cyclic: st.analysis.material_cyclic(m),
        best_recipe: st
            .analysis
            .cost
            .best_recipe[m as usize]
            .map(|r| g.recipe_full_id(r)),
        producer_count: g.producers[m as usize].len(),
        consumer_count: g.consumers[m as usize].len(),
        producers,
        consumers,
    }))
}

#[derive(Serialize)]
struct GraphEdgeData {
    id: String,
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Serialize)]
struct GraphView {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    truncated: bool,
    /// 因每配方输入/输出上限被隐藏的边数
    pruned_edges: usize,
    /// 起点材料节点 id
    start: String,
}

/// 添加材料节点（返回是否新增）。
fn add_material_node(
    g: &KnowledgeGraph,
    nodes: &mut Vec<GraphNode>,
    seen: &mut HashSet<String>,
    m: MaterialId,
) -> bool {
    let mid = format!("m:{}", m);
    if seen.insert(mid.clone()) {
        let info = g.material(m);
        nodes.push(GraphNode {
            data: GraphNodeData {
                id: mid,
                label: info.display.clone(),
                node_type: "material".to_string(),
                material_kind: Some(g.kind_str(info.key.kind).to_string()),
                category: None,
                count: None,
                mat_id: Some(g.material_id_str(m).to_string()),
            },
        });
        true
    } else {
        false
    }
}

/// 添加配方节点（返回是否新增）。
fn add_recipe_node(
    g: &KnowledgeGraph,
    nodes: &mut Vec<GraphNode>,
    seen: &mut HashSet<String>,
    rid: RecipeId,
) -> bool {
    let rid_s = format!("r:{}", rid);
    if seen.insert(rid_s.clone()) {
        let r = g.recipe(rid);
        let cat = g.category(r.category);
        nodes.push(GraphNode {
            data: GraphNodeData {
                id: rid_s,
                label: r.id.clone(),
                node_type: "recipe".to_string(),
                material_kind: None,
                category: Some(cat.title.clone()),
                count: Some(r.inputs.len()),
                mat_id: None,
            },
        });
        true
    } else {
        false
    }
}

fn add_graph_edge(
    edges: &mut Vec<GraphEdge>,
    seen: &mut HashSet<(String, String)>,
    source: String,
    target: String,
    label: Option<String>,
) {
    let key = (source.clone(), target.clone());
    if seen.insert(key) {
        edges.push(GraphEdge {
            data: GraphEdgeData {
                id: format!("e:{}>{}", source, target),
                source,
                target,
                label,
            },
        });
    }
}

fn qty_label(g: &KnowledgeGraph, m: MaterialId, qty: u64) -> Option<String> {
    if qty == 0 {
        return None;
    }
    match g.material(m).key.kind {
        MaterialKind::Fluid => Some(format!("{}mB", qty)),
        _ => Some(format!("x{}", qty)),
    }
}

/// 按归一数量取前 N 个主候选（返回 (保留, 被裁剪数)）。
fn top_primaries(
    g: &KnowledgeGraph,
    slots: &[gt_planner_core::Slot],
    limit: usize,
) -> (Vec<(MaterialId, u64)>, usize) {
    let mut primaries: Vec<(MaterialId, u64)> = slots.iter().filter_map(|s| s.primary()).collect();
    primaries.sort_by(|a, b| {
        gt_planner_core::util::mat_norm_qty(g, b.0, b.1)
            .total_cmp(&gt_planner_core::util::mat_norm_qty(g, a.0, a.1))
    });
    let pruned = primaries.len().saturating_sub(limit);
    primaries.truncate(limit);
    (primaries, pruned)
}

#[derive(Deserialize)]
struct GraphParams {
    material: String,
    kind: Option<String>,
    depth: Option<usize>,
    /// up（上游原料）/ down（下游用途）/ both
    direction: Option<String>,
    max_nodes: Option<usize>,
    /// 每个配方最多展示的输入槽数
    max_inputs: Option<usize>,
    /// 每个配方最多展示的输出槽数
    max_outputs: Option<usize>,
    /// 排除回收类配方（默认 true）
    exclude_recycling: Option<bool>,
}

async fn api_graph(State(st): St, Query(p): Query<GraphParams>) -> Result<Json<GraphView>, ApiError> {
    let g = &st.graph;
    let start = resolve_material(g, &p.material, p.kind.as_deref().and_then(kind_from_str), None)?;
    let depth = p.depth.unwrap_or(2).clamp(1, 6);
    let direction = p.direction.as_deref().unwrap_or("up");
    let node_limit = p.max_nodes.unwrap_or(300).clamp(10, 1500);
    let max_inputs = p.max_inputs.unwrap_or(6).clamp(1, 50);
    let max_outputs = p.max_outputs.unwrap_or(4).clamp(1, 50);
    let exclude_recycling = p.exclude_recycling.unwrap_or(true);

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<(String, String)> = HashSet::new();
    let mut truncated = false;
    let mut pruned_edges = 0usize;

    add_material_node(g, &mut nodes, &mut seen_nodes, start);

    let mut queue: VecDeque<(MaterialId, usize, i8)> = VecDeque::new();
    let mut visited: HashSet<(MaterialId, i8)> = HashSet::new();
    let want_up = direction == "up" || direction == "both";
    let want_down = direction == "down" || direction == "both";
    if want_up {
        queue.push_back((start, 0, 1));
        visited.insert((start, 1));
    }
    if want_down {
        queue.push_back((start, 0, -1));
        visited.insert((start, -1));
    }

    'bfs: while let Some((m, d, sign)) = queue.pop_front() {
        if nodes.len() >= node_limit {
            truncated = true;
            break;
        }
        if d >= depth {
            continue;
        }
        let mid = format!("m:{}", m);

        if sign > 0 {
            // ---- 上游：谁生产 m ----
            for &rid in &g.producers[m as usize] {
                if !g.is_plannable(rid) {
                    continue;
                }
                if exclude_recycling && g.is_recycling(rid) {
                    continue;
                }
                let r = g.recipe(rid);
                if r.inputs.is_empty() {
                    continue;
                }
                if nodes.len() >= node_limit {
                    truncated = true;
                    break 'bfs;
                }
                add_recipe_node(g, &mut nodes, &mut seen_nodes, rid);
                let rid_s = format!("r:{}", rid);
                let out_q = gt_planner_core::util::output_qty_of(r, m).unwrap_or(0);
                // 流向：配方 → 产物 m
                add_graph_edge(
                    &mut edges,
                    &mut seen_edges,
                    rid_s.clone(),
                    mid.clone(),
                    qty_label(g, m, out_q),
                );
                let (primaries, pruned) = top_primaries(g, &r.inputs, max_inputs);
                pruned_edges += pruned;
                for (im, iq) in primaries {
                    add_material_node(g, &mut nodes, &mut seen_nodes, im);
                    let imid = format!("m:{}", im);
                    // 流向：输入 → 配方
                    add_graph_edge(
                        &mut edges,
                        &mut seen_edges,
                        imid.clone(),
                        rid_s.clone(),
                        qty_label(g, im, iq),
                    );
                    if visited.insert((im, 1)) {
                        queue.push_back((im, d + 1, 1));
                    }
                }
            }
        } else {
            // ---- 下游：谁消耗 m ----
            for &rid in &g.consumers[m as usize] {
                if !g.is_plannable(rid) {
                    continue;
                }
                if exclude_recycling && g.is_recycling(rid) {
                    continue;
                }
                let r = g.recipe(rid);
                if r.outputs.is_empty() {
                    continue;
                }
                if nodes.len() >= node_limit {
                    truncated = true;
                    break 'bfs;
                }
                add_recipe_node(g, &mut nodes, &mut seen_nodes, rid);
                let rid_s = format!("r:{}", rid);
                // m 在该配方中的消耗量
                let mut qty_m = 0u64;
                for slot in &r.inputs {
                    for &(mm, q) in &slot.alts {
                        if mm == m {
                            qty_m = q;
                        }
                    }
                }
                add_graph_edge(
                    &mut edges,
                    &mut seen_edges,
                    mid.clone(),
                    rid_s.clone(),
                    qty_label(g, m, qty_m),
                );
                let (primaries, pruned) = top_primaries(g, &r.outputs, max_outputs);
                pruned_edges += pruned;
                for (om, oq) in primaries {
                    add_material_node(g, &mut nodes, &mut seen_nodes, om);
                    let omid = format!("m:{}", om);
                    add_graph_edge(
                        &mut edges,
                        &mut seen_edges,
                        rid_s.clone(),
                        omid.clone(),
                        qty_label(g, om, oq),
                    );
                    if visited.insert((om, -1)) {
                        queue.push_back((om, d + 1, -1));
                    }
                }
            }
        }
    }

    Ok(Json(GraphView {
        nodes,
        edges,
        truncated,
        pruned_edges,
        start: format!("m:{}", start),
    }))
}

#[derive(Deserialize)]
struct PlanReq {
    material: String,
    kind: Option<String>,
    nbt: Option<String>,
    rate: f64,
    mode: Option<String>,
    beam_width: Option<usize>,
    candidates: Option<usize>,
    max_iterations: Option<usize>,
    include_recycling: Option<bool>,
    block_amplification: Option<bool>,
}

async fn api_plan(State(st): St, Json(req): Json<PlanReq>) -> Result<Json<Plan>, ApiError> {
    if !(req.rate.is_finite() && req.rate > 0.0) {
        return Err(bad_request("rate 必须是正数"));
    }
    let mode = req.mode.unwrap_or_else(|| "tree".to_string());
    if mode != "tree" && mode != "beam" && mode != "exact" {
        return Err(bad_request("mode 只支持 tree / beam / exact"));
    }
    let state = Arc::clone(&st);
    let plan = tokio::task::spawn_blocking(move || {
        let g = &state.graph;
        let an = &state.analysis;
        let m = resolve_material(g, &req.material, req.kind.as_deref().and_then(kind_from_str), req.nbt.as_deref())?;
        let plan = match mode.as_str() {
            "beam" => {
                let opts = BeamOptions {
                    beam_width: req.beam_width.unwrap_or(8),
                    candidate_limit: req.candidates.unwrap_or(3),
                    max_iterations: req.max_iterations.unwrap_or(12),
                    sample_per_state: 24,
                    ops_penalty: 0.001,
                };
                // GPU 批量评估（不可用则回退 CPU）
                match gt_planner_gpu::GpuEvaluator::new() {
                    Ok(mut ev) => gt_planner_core::planner::plan_beam_with_evaluator(
                        g, an, m, req.rate, &opts, Some(&mut ev),
                    ),
                    Err(e) => {
                        eprintln!("{}；beam 使用 CPU 路径", e);
                        plan_beam(g, an, m, req.rate, &opts)
                    }
                }
            }
            "exact" => {
                let opts = gt_planner_core::solver::ExactOptions {
                    include_recycling: req.include_recycling.unwrap_or(false),
                    block_amplification: req.block_amplification.unwrap_or(false),
                    ..Default::default()
                };
                gt_planner_core::solver::plan_exact(g, an, m, req.rate, &opts)
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?
            }
            _ => {
                let preq = PlanRequest {
                    target: m,
                    rate_per_min: req.rate,
                    max_ops: 200_000,
                };
                plan_tree(g, an, &preq)
            }
        };
        Ok::<Plan, ApiError>(plan)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("规划任务失败: {e}")))??;
    Ok(Json(plan))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut data = std::env::var("GTP_DATA").unwrap_or_else(|_| "jei_recipes.json".to_string());
    let mut port: u16 = 8787;
    let mut web = "web".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" | "-d" => {
                i += 1;
                data = args.get(i).cloned().unwrap_or_default();
            }
            "--port" | "-p" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(8787);
            }
            "--web" => {
                i += 1;
                web = args.get(i).cloned().unwrap_or(web);
            }
            other => eprintln!("未知参数: {}", other),
        }
        i += 1;
    }

    let data_path = PathBuf::from(&data);
    if !data_path.exists() {
        eprintln!("找不到数据文件：{}", data_path.display());
        eprintln!("用法: gt-planner-server --data <jei_recipes.json> [--port 8787] [--web web]");
        std::process::exit(1);
    }

    eprintln!("加载 {} ...", data_path.display());
    let t0 = Instant::now();
    let graph = gt_planner_core::parser::load_file(&data_path)?;
    eprintln!(
        "构图完成：{} 材料 / {} 配方（{:.2}s）",
        graph.stats.material_count,
        graph.stats.recipe_count,
        t0.elapsed().as_secs_f64()
    );
    let analysis = Analysis::build(&graph);
    eprintln!(
        "分析完成：SCC + 成本 + 剪枝（{:.2}s）",
        analysis.build_ms / 1000.0
    );

    let state = Arc::new(AppState {
        graph,
        analysis,
        loaded_at: Instant::now(),
    });

    let index = format!("{}/index.html", web);
    let static_files = ServeDir::new(&web).fallback(ServeFile::new(&index));

    let app = Router::new()
        .route("/api/stats", get(api_stats))
        .route("/api/categories", get(api_categories))
        .route("/api/search", get(api_search))
        .route("/api/material/{id}", get(api_material))
        .route("/api/graph", get(api_graph))
        .route("/api/plan", post(api_plan))
        .fallback_service(static_files)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("gt-planner 已启动: http://127.0.0.1:{}", port);
    println!("前端目录: {}", web);
    axum::serve(listener, app).await?;
    Ok(())
}
