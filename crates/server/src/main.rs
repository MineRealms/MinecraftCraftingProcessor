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
}

#[derive(Serialize)]
struct GraphEdge {
    data: GraphEdgeData,
}

#[derive(Serialize)]
struct GraphEdgeData {
    id: String,
    source: String,
    target: String,
}

#[derive(Serialize)]
struct GraphView {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    truncated: bool,
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

#[derive(Deserialize)]
struct GraphParams {
    material: String,
    kind: Option<String>,
    depth: Option<usize>,
    limit: Option<usize>,
}

async fn api_graph(State(st): St, Query(p): Query<GraphParams>) -> Result<Json<GraphView>, ApiError> {
    let g = &st.graph;
    let start = resolve_material(g, &p.material, p.kind.as_deref().and_then(kind_from_str), None)?;
    let depth = p.depth.unwrap_or(2).min(5);
    let node_limit = p.limit.unwrap_or(400).min(2000);

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    let mut truncated = false;

    // BFS 上游（生产者方向）
    let mut queue: VecDeque<(MaterialId, usize)> = VecDeque::new();
    queue.push_back((start, 0));
    let mut visited: HashSet<MaterialId> = HashSet::new();
    visited.insert(start);

    while let Some((m, d)) = queue.pop_front() {
        let mid = format!("m:{}", m);
        if seen_nodes.insert(mid.clone()) {
            let info = g.material(m);
            nodes.push(GraphNode {
                data: GraphNodeData {
                    id: mid.clone(),
                    label: info.display.clone(),
                    node_type: "material".to_string(),
                    material_kind: Some(g.kind_str(info.key.kind).to_string()),
                    category: None,
                    count: None,
                },
            });
        }
        if nodes.len() >= node_limit {
            truncated = true;
            break;
        }
        if d >= depth {
            continue;
        }
        for &rid in &g.producers[m as usize] {
            if !g.is_plannable(rid) {
                continue;
            }
            let r = g.recipe(rid);
            if r.inputs.is_empty() {
                continue;
            }
            let rid_s = format!("r:{}", rid);
            if seen_nodes.insert(rid_s.clone()) {
                let cat = g.category(r.category);
                nodes.push(GraphNode {
                    data: GraphNodeData {
                        id: rid_s.clone(),
                        label: r.id.clone(),
                        node_type: "recipe".to_string(),
                        material_kind: None,
                        category: Some(cat.title.clone()),
                        count: Some(r.inputs.len()),
                    },
                });
            }
            // 材料 → 配方
            let eid = format!("e:{}>{}", mid, rid_s);
            if seen_edges.insert(eid.clone()) {
                edges.push(GraphEdge {
                    data: GraphEdgeData {
                        id: eid,
                        source: mid.clone(),
                        target: rid_s.clone(),
                    },
                });
            }
            // 配方 → 输入材料
            for slot in &r.inputs {
                let Some((im, _)) = slot.primary() else {
                    continue;
                };
                let imid = format!("m:{}", im);
                if seen_nodes.insert(imid.clone()) {
                    let info = g.material(im);
                    nodes.push(GraphNode {
                        data: GraphNodeData {
                            id: imid.clone(),
                            label: info.display.clone(),
                            node_type: "material".to_string(),
                            material_kind: Some(g.kind_str(info.key.kind).to_string()),
                            category: None,
                            count: None,
                        },
                    });
                }
                let eid = format!("e:{}>{}", rid_s, imid);
                if seen_edges.insert(eid.clone()) {
                    edges.push(GraphEdge {
                        data: GraphEdgeData {
                            id: eid,
                            source: rid_s.clone(),
                            target: imid.clone(),
                        },
                    });
                }
                if visited.insert(im) {
                    queue.push_back((im, d + 1));
                }
            }
        }
    }

    Ok(Json(GraphView {
        nodes,
        edges,
        truncated,
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
}

async fn api_plan(State(st): St, Json(req): Json<PlanReq>) -> Result<Json<Plan>, ApiError> {
    if !(req.rate.is_finite() && req.rate > 0.0) {
        return Err(bad_request("rate 必须是正数"));
    }
    let mode = req.mode.unwrap_or_else(|| "tree".to_string());
    if mode != "tree" && mode != "beam" {
        return Err(bad_request("mode 只支持 tree / beam"));
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
                plan_beam(g, an, m, req.rate, &opts)
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
