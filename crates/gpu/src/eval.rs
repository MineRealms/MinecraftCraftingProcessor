//! wgpu 批量候选评估器。
//!
//! 对每个候选选择表并行执行"物料平衡定点迭代"：
//! ```text
//! repeat K 次:
//!   consumed[i] = Σ_{r 消耗 i} ops[owner[r]] · qty(r→i)
//!   ops[i]      = (demand[i] + consumed[i]) / out_qty[i]   (有配方时)
//! score = Σ raw[i]·weight[i] + λ·Σ ops[i]                  (raw = 未满足缺口)
//! ```
//! 得分用于 beam 局部搜索的粗筛（top-K 再交给 CPU 完整展开精评）。

use std::collections::HashMap;

use gt_planner_core::analysis::Analysis;
use gt_planner_core::graph::KnowledgeGraph;
use gt_planner_core::model::{MaterialId, RecipeId};
use gt_planner_core::planner::BatchEvaluator;
use wgpu::util::DeviceExt;

use crate::subgraph::{build_subgraph_from_plan, CandidateTable, SubgraphData};

const SHADER: &str = r#"
struct Params {
    n_materials: u32,
    n_recipes: u32,
    n_candidates: u32,
    iterations: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> cons_offsets: array<u32>;
@group(0) @binding(2) var<storage, read> cons_recipes: array<u32>;
@group(0) @binding(3) var<storage, read> cons_qty: array<f32>;
@group(0) @binding(4) var<storage, read> demand: array<f32>;
@group(0) @binding(5) var<storage, read> norm_factor: array<f32>;
@group(0) @binding(6) var<storage, read> owner: array<u32>;
@group(0) @binding(7) var<storage, read> out_qty: array<f32>;
@group(0) @binding(8) var<storage, read_write> ops: array<f32>;
@group(0) @binding(9) var<storage, read_write> scores: array<f32>;

fn consumed_of(c: u32, i: u32) -> f32 {
    var consumed = 0.0;
    let start = cons_offsets[i];
    let end = cons_offsets[i + 1u];
    for (var k = start; k < end; k = k + 1u) {
        let r = cons_recipes[k];
        let o = owner[c * params.n_recipes + r];
        if (o != 0xffffffffu) {
            consumed = consumed + ops[c * params.n_materials + o] * cons_qty[k];
        }
    }
    return consumed;
}

@compute @workgroup_size(64)
fn iterate(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let c = gid.y;
    if (i >= params.n_materials || c >= params.n_candidates) {
        return;
    }
    let consumed = consumed_of(c, i);
    let idx = c * params.n_materials + i;
    let oq = out_qty[idx];
    if (oq > 0.0) {
        var v = (demand[i] + consumed) / oq;
        if (v > 1e12) { v = 1e12; }
        ops[idx] = v;
    } else {
        ops[idx] = 0.0;
    }
}

@compute @workgroup_size(1)
fn score(@builtin(global_invocation_id) gid: vec3<u32>) {
    let c = gid.x;
    if (c >= params.n_candidates) {
        return;
    }
    var s = 0.0;
    var diverged = false;
    for (var i = 0u; i < params.n_materials; i = i + 1u) {
        let idx = c * params.n_materials + i;
        let consumed = consumed_of(c, i);
        let produced = ops[idx] * out_qty[idx];
        let raw = max(0.0, demand[i] + consumed - produced);
        s = s + raw * norm_factor[i];
        s = s + 0.001 * ops[idx];
        if (ops[idx] > 1e11) { diverged = true; }
    }
    if (diverged) { s = s + 1e9; }
    scores[c] = s;
}
"#;

/// GPU 不可用（无适配器等）。
#[derive(Debug)]
pub struct GpuUnavailable(pub String);

impl std::fmt::Display for GpuUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GPU 不可用: {}", self.0)
    }
}

/// 评估上下文（子图 + 静态缓冲）。
struct EvalCtx {
    sg: SubgraphData,
    cons_offsets_buf: wgpu::Buffer,
    cons_recipes_buf: wgpu::Buffer,
    cons_qty_buf: wgpu::Buffer,
    demand_buf: wgpu::Buffer,
    norm_buf: wgpu::Buffer,
}

/// GPU 批量评估器。
pub struct GpuEvaluator {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    iterate_pipeline: wgpu::ComputePipeline,
    score_pipeline: wgpu::ComputePipeline,
    adapter_name: String,
    ctx: Option<EvalCtx>,
    iterations: u32,
}

impl GpuEvaluator {
    /// 创建评估器（不绑定具体方案；`prepare` 时构建子图）。
    pub fn new() -> Result<Self, GpuUnavailable> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .or_else(|_| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: true,
            }))
        })
        .map_err(|e| GpuUnavailable(format!("找不到 wgpu 适配器（含软件回退）: {e}")))?;

        let adapter_name = adapter.get_info().name;
        // 需要 ≥10 个存储缓冲绑定：请求适配器实际支持的 limits
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gt-planner-gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| GpuUnavailable(format!("创建设备失败: {e}")))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gtp-eval"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        // 显式绑定布局（0 = uniform，1..=7 只读存储，8..=9 读写存储）
        let entry = |binding: u32, read_only: bool, uniform: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: if uniform {
                    wgpu::BufferBindingType::Uniform
                } else {
                    wgpu::BufferBindingType::Storage { read_only }
                },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gtp-eval-layout"),
            entries: &[
                entry(0, false, true),
                entry(1, true, false),
                entry(2, true, false),
                entry(3, true, false),
                entry(4, true, false),
                entry(5, true, false),
                entry(6, true, false),
                entry(7, true, false),
                entry(8, false, false),
                entry(9, false, false),
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gtp-eval-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let iterate_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("iterate"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("iterate"),
            compilation_options: Default::default(),
            cache: None,
        });
        let score_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("score"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("score"),
            compilation_options: Default::default(),
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            layout,
            iterate_pipeline,
            score_pipeline,
            adapter_name,
            ctx: None,
            iterations: 48,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// 设置定点迭代次数。
    pub fn set_iterations(&mut self, iterations: u32) {
        self.iterations = iterations.clamp(1, 512);
    }

    /// 用基线方案构建评估子图并上传静态缓冲。
    pub fn prepare_with_plan(
        &mut self,
        g: &KnowledgeGraph,
        an: &Analysis,
        target: MaterialId,
        rate_per_min: f64,
        choices: &HashMap<MaterialId, RecipeId>,
    ) {
        let sg = build_subgraph_from_plan(g, an, target, choices, 3);
        let demand: Vec<f32> = (0..sg.materials.len())
            .map(|i| {
                if sg.materials[i] == target {
                    rate_per_min as f32
                } else {
                    0.0
                }
            })
            .collect();

        let cons_offsets_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cons_offsets"),
                contents: bytemuck::cast_slice(&sg.cons_offsets),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let cons_recipes_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cons_recipes"),
                contents: bytemuck::cast_slice(&sg.cons_recipes),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let cons_qty_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cons_qty"),
                contents: bytemuck::cast_slice(&sg.cons_qty),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let demand_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("demand"),
                contents: bytemuck::cast_slice(&demand),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let norm_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("norm"),
                contents: bytemuck::cast_slice(&sg.norm_factor),
                usage: wgpu::BufferUsages::STORAGE,
            });

        self.ctx = Some(EvalCtx {
            sg,
            cons_offsets_buf,
            cons_recipes_buf,
            cons_qty_buf,
            demand_buf,
            norm_buf,
        });
    }

    fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    /// 评估一批候选（返回得分）。
    fn evaluate_batch(&self, ctx: &EvalCtx, tables: &[&CandidateTable]) -> Vec<f64> {
        let n = ctx.sg.materials.len();
        let r = ctx.sg.recipes.len();
        let c = tables.len();
        if c == 0 {
            return Vec::new();
        }

        let mut owner: Vec<u32> = Vec::with_capacity(c * r);
        let mut out_qty: Vec<f32> = Vec::with_capacity(c * n);
        for t in tables {
            owner.extend_from_slice(&t.owner);
            out_qty.extend_from_slice(&t.out_qty);
        }

        let owner_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("owner"),
                contents: bytemuck::cast_slice(&owner),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let out_qty_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("out_qty"),
                contents: bytemuck::cast_slice(&out_qty),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let ops_size = ((c * n * 4) as u64).max(4);
        let ops_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ops"),
            size: ops_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&ops_buf, 0, &vec![0u8; ops_size as usize]);

        let scores_size = ((c * 4) as u64).max(4);
        let scores_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scores"),
            size: scores_size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: scores_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let params: [u32; 4] = [n as u32, r as u32, c as u32, self.iterations];
        let params_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let layout = self.bind_group_layout();
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("eval"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: ctx.cons_offsets_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: ctx.cons_recipes_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: ctx.cons_qty_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: ctx.demand_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: ctx.norm_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: owner_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: out_qty_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: ops_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: scores_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("eval-encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("iterate-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.iterate_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let wg_x = (n as u32).div_ceil(64);
            for _ in 0..self.iterations {
                pass.dispatch_workgroups(wg_x, c as u32, 1);
            }
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("score-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.score_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(c as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&scores_buf, 0, &staging, 0, scores_size);
        self.queue.submit(Some(encoder.finish()));

        staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = staging.slice(..).get_mapped_range();
        let scores: Vec<f64> = bytemuck::cast_slice::<u8, f32>(&data)
            .iter()
            .take(c)
            .map(|&v| v as f64)
            .collect();
        drop(data);
        staging.unmap();
        scores
    }
}

impl BatchEvaluator for GpuEvaluator {
    fn prepare(
        &mut self,
        g: &KnowledgeGraph,
        an: &Analysis,
        target: MaterialId,
        rate_per_min: f64,
        choices: &HashMap<MaterialId, RecipeId>,
    ) {
        self.prepare_with_plan(g, an, target, rate_per_min, choices);
    }

    fn evaluate(&self, choices: &[HashMap<MaterialId, RecipeId>]) -> Vec<f64> {
        const BATCH: usize = 512;
        let Some(ctx) = self.ctx.as_ref() else {
            return vec![f64::INFINITY; choices.len()];
        };

        let mut out: Vec<f64> = vec![f64::INFINITY; choices.len()];
        let mut valid: Vec<(usize, CandidateTable)> = Vec::with_capacity(choices.len());
        for (i, ch) in choices.iter().enumerate() {
            if let Some(t) = CandidateTable::from_choices(&ctx.sg, ch) {
                valid.push((i, t));
            }
        }
        if std::env::var_os("GTP_DEBUG_GPU").is_some() {
            eprintln!(
                "gpu evaluate: subgraph {} materials / {} recipes, candidates {} valid {}",
                ctx.sg.materials.len(),
                ctx.sg.recipes.len(),
                choices.len(),
                valid.len()
            );
        }

        for chunk in valid.chunks(BATCH) {
            let refs: Vec<&CandidateTable> = chunk.iter().map(|(_, t)| t).collect();
            let scores = self.evaluate_batch(ctx, &refs);
            for ((idx, _), s) in chunk.iter().zip(scores) {
                out[*idx] = s;
            }
        }
        out
    }

    fn name(&self) -> String {
        format!("wgpu({})", self.adapter_name)
    }
}
