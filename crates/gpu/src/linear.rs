//! GPU 线性求解器：CSR 稀疏矩阵 + 松弛 Jacobi 迭代。
//!
//! 物料平衡在固定配方分配下是一个线性系统：
//!
//! ```text
//! x = rhs + D⁻¹ C x        （x = 各材料操作量）
//! ⇔ (I − D⁻¹C) x = D⁻¹ b   （A x = rhs）
//! ```
//!
//! - `D = diag(out_qty)`：所选配方对材料的产出
//! - `C[i][j]`：owner 为材料 j 的配方对材料 i 的消耗量
//! - `b`：目标需求（仅目标材料非零）
//!
//! 迭代格式（带松弛 ω）：`x ← x + ω·(rhs − A·x)`。
//! ω = 1 即标准 Jacobi（与评估器的定点迭代等价）；ω < 1 可抑制
//! 放大环的发散、ω 接近 2 可加速收敛。
//!
//! 批处理：一次 dispatch 同时求解 C 个候选（每候选一套 CSR，padding 到
//! 最大尺寸），用于 beam 的候选粗筛。

use bytemuck::{Pod, Zeroable};

use crate::subgraph::{CandidateTable, SubgraphData};

/// 单个候选的流量线性系统（CSR）。
pub struct FlowSystem {
    pub n: usize,
    pub out_qty: Vec<f32>,
    pub demand: Vec<f32>,
    pub row_ptr: Vec<u32>,
    pub col_idx: Vec<u32>,
    pub values: Vec<f32>,
    pub rhs: Vec<f32>,
}

impl FlowSystem {
    /// 从评估子图 + 候选选择表构建 A x = rhs。
    pub fn from_candidate(sg: &SubgraphData, cand: &CandidateTable, demand: &[f32]) -> Self {
        let n = sg.materials.len();
        let mut out_qty = vec![0.0f32; n];
        out_qty.copy_from_slice(&cand.out_qty);
        let mut row_ptr: Vec<u32> = Vec::with_capacity(n + 1);
        let mut col_idx: Vec<u32> = Vec::new();
        let mut values: Vec<f32> = Vec::new();
        let mut rhs = vec![0.0f32; n];
        row_ptr.push(0);

        for i in 0..n {
            // 消耗项（去重合并）
            let mut entries: Vec<(u32, f32)> = Vec::new();
            for k in sg.cons_offsets[i]..sg.cons_offsets[i + 1] {
                let r = sg.cons_recipes[k as usize];
                let j = cand.owner[r as usize];
                if j == u32::MAX {
                    continue;
                }
                entries.push((j, sg.cons_qty[k as usize]));
            }
            entries.sort_by_key(|e| e.0);
            let mut merged: Vec<(u32, f32)> = Vec::new();
            for (j, q) in entries {
                if let Some(last) = merged.last_mut() {
                    if last.0 == j {
                        last.1 += q;
                        continue;
                    }
                }
                merged.push((j, q));
            }

            let oi = out_qty[i];
            if oi > 0.0 {
                rhs[i] = demand[i] / oi;
                let mut diag = 1.0f32;
                for (j, q) in merged {
                    let coeff = -q / oi;
                    if j == i as u32 {
                        diag += coeff;
                    } else {
                        col_idx.push(j);
                        values.push(coeff);
                    }
                }
                // 对角项放在行尾
                col_idx.push(i as u32);
                values.push(diag);
            } else {
                // 无配方：恒等行（x_i = 0）
                col_idx.push(i as u32);
                values.push(1.0);
            }
            row_ptr.push(col_idx.len() as u32);
        }

        Self {
            n,
            out_qty,
            demand: demand.to_vec(),
            row_ptr,
            col_idx,
            values,
            rhs,
        }
    }

    /// CPU 侧 A·x（用于评分时反推消耗量）。
    pub fn spmv(&self, x: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; self.n];
        for i in 0..self.n {
            let mut acc = 0.0f32;
            for k in self.row_ptr[i]..self.row_ptr[i + 1] {
                acc += self.values[k as usize] * x[self.col_idx[k as usize] as usize];
            }
            y[i] = acc;
        }
        y
    }

    /// 由解 x 计算消耗量：consumed_i = (x_i − (A·x)_i) · out_i。
    pub fn consumed(&self, x: &[f32]) -> Vec<f32> {
        let ax = self.spmv(x);
        (0..self.n)
            .map(|i| (x[i] - ax[i]) * self.out_qty[i])
            .collect()
    }

    /// 评分：Σ 未满足缺口·权重 + λ·Σ x（越小越好）。
    pub fn score(&self, x: &[f32], norm_factor: &[f32], ops_penalty: f32) -> f32 {
        let ax = self.spmv(x);
        let mut s = 0.0f32;
        let mut diverged = false;
        for i in 0..self.n {
            let consumed = (x[i] - ax[i]) * self.out_qty[i];
            let produced = x[i] * self.out_qty[i];
            let raw = (self.demand[i] + consumed - produced).max(0.0);
            s += raw * norm_factor[i] + ops_penalty * x[i];
            if x[i] > 1e11 {
                diverged = true;
            }
        }
        if diverged {
            s += 1e9;
        }
        s
    }

    /// CPU 回退：同样的松弛 Jacobi 迭代。
    pub fn solve_cpu(&self, iterations: u32, omega: f32) -> Vec<f32> {
        let mut x = vec![0.0f32; self.n];
        for _ in 0..iterations.max(1) {
            let ax = self.spmv(&x);
            for i in 0..self.n {
                x[i] += omega * (self.rhs[i] - ax[i]);
            }
        }
        x
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    max_n: u32,
    max_nnz: u32,
    n_candidates: u32,
    iterations: u32,
    omega: f32,
    _pad: [f32; 3],
}

const SHADER: &str = r#"
struct Params {
    max_n: u32,
    max_nnz: u32,
    n_candidates: u32,
    iterations: u32,
    omega: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> row_ptr: array<u32>;
@group(0) @binding(2) var<storage, read> col_idx: array<u32>;
@group(0) @binding(3) var<storage, read> values: array<f32>;
@group(0) @binding(4) var<storage, read> rhs: array<f32>;
@group(0) @binding(5) var<storage, read_write> x: array<f32>;

// x ← x + ω·(rhs − A·x)   （每线程一行）
@compute @workgroup_size(64)
fn jacobi(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let c = gid.y;
    if (i >= params.max_n || c >= params.n_candidates) {
        return;
    }
    let rp_base = c * (params.max_n + 1u);
    let start = row_ptr[rp_base + i];
    let end = row_ptr[rp_base + i + 1u];
    let x_base = c * params.max_n;
    let nnz_base = c * params.max_nnz;
    var ax = 0.0;
    for (var k = start; k < end; k = k + 1u) {
        ax = ax + values[nnz_base + k] * x[x_base + col_idx[nnz_base + k]];
    }
    let idx = x_base + i;
    x[idx] = x[idx] + params.omega * (rhs[x_base + i] - ax);
}
"#;

/// GPU 不可用。
#[derive(Debug)]
pub struct GpuUnavailable(pub String);

impl std::fmt::Display for GpuUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GPU 不可用: {}", self.0)
    }
}

/// GPU 流量线性求解器。
pub struct GpuFlowSolver {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl GpuFlowSolver {
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
        .map_err(|e| GpuUnavailable(format!("找不到 wgpu 适配器: {e}")))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gtp-linear"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| GpuUnavailable(format!("创建设备失败: {e}")))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gtp-linear"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let entry = |binding: u32, uniform: bool, read_write: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: if uniform {
                    wgpu::BufferBindingType::Uniform
                } else {
                    wgpu::BufferBindingType::Storage {
                        read_only: !read_write,
                    }
                },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gtp-linear-layout"),
            entries: &[
                entry(0, true, false),
                entry(1, false, false),
                entry(2, false, false),
                entry(3, false, false),
                entry(4, false, false),
                entry(5, false, true),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gtp-linear-pl"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("jacobi"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("jacobi"),
            compilation_options: Default::default(),
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            layout,
        })
    }

    /// 批量求解：返回每个系统的解向量 x。
    pub fn solve_batch(
        &self,
        systems: &[FlowSystem],
        iterations: u32,
        omega: f32,
    ) -> Vec<Vec<f32>> {
        if systems.is_empty() {
            return Vec::new();
        }
        let c = systems.len();
        let max_n = systems.iter().map(|s| s.n).max().unwrap_or(0);
        let max_nnz = systems.iter().map(|s| s.col_idx.len()).max().unwrap_or(0).max(1);
        let iters = iterations.max(1);

        // 打包（padding 到最大尺寸）
        let mut row_ptr = vec![0u32; c * (max_n + 1)];
        let mut col_idx = vec![0u32; c * max_nnz];
        let mut values = vec![0f32; c * max_nnz];
        let mut rhs = vec![0f32; c * max_n];
        for (ci, s) in systems.iter().enumerate() {
            let rp_base = ci * (max_n + 1);
            let nnz_base = ci * max_nnz;
            let x_base = ci * max_n;
            // 行指针：超出 n 的行填空（= nnz），保证空区间
            let nnz = s.col_idx.len();
            for i in 0..=s.n {
                row_ptr[rp_base + i] = s.row_ptr[i];
            }
            for i in (s.n + 1)..=max_n {
                row_ptr[rp_base + i] = nnz as u32;
            }
            col_idx[nnz_base..nnz_base + nnz].copy_from_slice(&s.col_idx);
            values[nnz_base..nnz_base + nnz].copy_from_slice(&s.values);
            rhs[x_base..x_base + s.n].copy_from_slice(&s.rhs);
        }

        let params = Params {
            max_n: max_n as u32,
            max_nnz: max_nnz as u32,
            n_candidates: c as u32,
            iterations: iters,
            omega,
            _pad: [0.0; 3],
        };

        use wgpu::util::DeviceExt;
        let row_ptr_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("row_ptr"),
                contents: bytemuck::cast_slice(&row_ptr),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let col_idx_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("col_idx"),
                contents: bytemuck::cast_slice(&col_idx),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let values_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("values"),
                contents: bytemuck::cast_slice(&values),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let rhs_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("rhs"),
                contents: bytemuck::cast_slice(&rhs),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let params_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let x_size = ((c * max_n * 4) as u64).max(4);
        let x_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("x"),
            size: x_size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&x_buf, 0, &vec![0u8; x_size as usize]);
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("x-staging"),
            size: x_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("linear"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: row_ptr_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: col_idx_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: values_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: rhs_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: x_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("linear-encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("jacobi-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let wg_x = (max_n as u32).div_ceil(64);
            for _ in 0..iters {
                pass.dispatch_workgroups(wg_x, c as u32, 1);
            }
        }
        encoder.copy_buffer_to_buffer(&x_buf, 0, &staging, 0, x_size);
        self.queue.submit(Some(encoder.finish()));

        staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = staging.slice(..).get_mapped_range();
        let flat: &[f32] = bytemuck::cast_slice(&data);
        let mut out = Vec::with_capacity(c);
        for (ci, s) in systems.iter().enumerate() {
            let base = ci * max_n;
            out.push(flat[base..base + s.n].to_vec());
        }
        drop(data);
        staging.unmap();
        out
    }
}
