//! GPU 线性求解器：CSR 稀疏矩阵 + **BiCGSTAB**（回退：松弛 Jacobi）。
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
//! 求解器：
//! - **BiCGSTAB**：A 一般非对称（循环流），用双共轭梯度稳定化方法；
//!   每个候选一个 workgroup（256 线程），迭代内做 SpMV + 归约点积；
//!   带收敛判定（残差 < tol）与 breakdown 保护。
//! - **松弛 Jacobi**：`x ← x + ω·(rhs − A·x)`，作为 CPU 回退/对照。
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

    /// CPU 侧 A·x。
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

    /// 评分：Σ 未满足缺口·权重 + λ·Σ x（越小越好）。非有限解加罚。
    pub fn score(&self, x: &[f32], norm_factor: &[f32], ops_penalty: f32) -> f32 {
        let ax = self.spmv(x);
        let mut s = 0.0f32;
        let mut diverged = false;
        for i in 0..self.n {
            let consumed = (x[i] - ax[i]) * self.out_qty[i];
            let produced = x[i] * self.out_qty[i];
            let raw = (self.demand[i] + consumed - produced).max(0.0);
            s += raw * norm_factor[i] + ops_penalty * x[i];
            if !x[i].is_finite() || x[i] > 1e11 {
                diverged = true;
            }
        }
        if diverged {
            s += 1e9;
        }
        s
    }

    /// 残差范数 ‖rhs − A·x‖₂（用于收敛判定/报告）。
    pub fn residual(&self, x: &[f32]) -> f32 {
        let ax = self.spmv(x);
        let mut s = 0.0f32;
        for i in 0..self.n {
            let d = self.rhs[i] - ax[i];
            s += d * d;
        }
        s.sqrt()
    }

    /// CPU 回退：松弛 Jacobi（ω = 1 即标准 Jacobi）。
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

/// 单个系统的求解结果。
#[derive(Debug, Clone)]
pub struct SolveResult {
    pub x: Vec<f32>,
    pub iterations: u32,
    pub residual: f32,
    pub converged: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    max_n: u32,
    max_nnz: u32,
    n_candidates: u32,
    max_iters: u32,
    tol: f32,
    _pad: [f32; 3],
}

const SHADER: &str = r#"
struct Params {
    max_n: u32,
    max_nnz: u32,
    n_candidates: u32,
    max_iters: u32,
    tol: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> row_ptr: array<u32>;
@group(0) @binding(2) var<storage, read> col_idx: array<u32>;
@group(0) @binding(3) var<storage, read> values: array<f32>;
// 8 个打包向量：0=x 1=r 2=r_hat 3=p 4=v 5=s 6=t 7=rhs
@group(0) @binding(4) var<storage, read_write> vecs: array<f32>;
// 每候选 4 个标量：[iterations, residual, converged, -]
@group(0) @binding(5) var<storage, read_write> out_info: array<f32>;
// 每候选的解向量
@group(0) @binding(6) var<storage, read_write> x_out: array<f32>;

const WG: u32 = 256u;
const VX: u32 = 0u;
const VR: u32 = 1u;
const VRH: u32 = 2u;
const VP: u32 = 3u;
const VV: u32 = 4u;
const VS: u32 = 5u;
const VT: u32 = 6u;
const VB: u32 = 7u;

var<workgroup> sh: array<f32, 256>;
var<workgroup> scal: array<f32, 8>;

fn vidx(v: u32, c: u32, i: u32) -> u32 {
    return (v * params.n_candidates + c) * params.max_n + i;
}

// 点积归约：结果对所有线程可见（通过 sh[0]）
fn dot_reduce(c: u32, a: u32, b: u32, tid: u32) -> f32 {
    var partial = 0.0;
    for (var i = tid; i < params.max_n; i = i + WG) {
        partial = partial + vecs[vidx(a, c, i)] * vecs[vidx(b, c, i)];
    }
    sh[tid] = partial;
    workgroupBarrier();
    var s = WG / 2u;
    loop {
        if (s == 0u) { break; }
        if (tid < s) {
            sh[tid] = sh[tid] + sh[tid + s];
        }
        workgroupBarrier();
        s = s / 2u;
    }
    let res = sh[0];
    workgroupBarrier();
    return res;
}

// 残差范数（平方）→ 存 scal[3]
fn residual_norm(c: u32, tid: u32) {
    let rn = dot_reduce(c, VR, VR, tid);
    if (tid == 0u) {
        scal[3] = sqrt(rn);
    }
    workgroupBarrier();
}

// v = A * src
fn spmv(c: u32, src: u32, dst: u32, tid: u32) {
    let rp_base = c * (params.max_n + 1u);
    let nnz_base = c * params.max_nnz;
    for (var i = tid; i < params.max_n; i = i + WG) {
        let start = row_ptr[rp_base + i];
        let end = row_ptr[rp_base + i + 1u];
        var acc = 0.0;
        for (var k = start; k < end; k = k + 1u) {
            acc = acc + values[nnz_base + k] * vecs[vidx(src, c, col_idx[nnz_base + k])];
        }
        vecs[vidx(dst, c, i)] = acc;
    }
}

@compute @workgroup_size(256)
fn bicgstab(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let c = wid.x;
    if (c >= params.n_candidates) {
        return;
    }
    let tid = lid.x;
    let n = params.max_n;

    // 初始化：x=0, r=rhs, r_hat=rhs, p=0, v=0, s=0, t=0
    for (var i = tid; i < n; i = i + WG) {
        let b = vecs[vidx(VB, c, i)];
        vecs[vidx(VX, c, i)] = 0.0;
        vecs[vidx(VR, c, i)] = b;
        vecs[vidx(VRH, c, i)] = b;
        vecs[vidx(VP, c, i)] = 0.0;
        vecs[vidx(VV, c, i)] = 0.0;
        vecs[vidx(VS, c, i)] = 0.0;
        vecs[vidx(VT, c, i)] = 0.0;
    }
    if (tid == 0u) {
        scal[0] = 1.0; // rho
        scal[1] = 1.0; // alpha
        scal[2] = 1.0; // omega
        scal[3] = 1e30; // residual
    }
    workgroupBarrier();

    var iters = 0u;
    var converged = false;

    // 初始残差
    residual_norm(c, tid);
    if (scal[3] < params.tol) {
        converged = true;
    }

    for (var it = 0u; it < params.max_iters; it = it + 1u) {
        if (converged) { break; }
        iters = it + 1u;

        // rho_new = <r_hat, r>
        let rho_new = dot_reduce(c, VRH, VR, tid);
        if (abs(rho_new) < 1e-30) { break; }
        let beta = (rho_new / scal[0]) * (scal[1] / scal[2]);

        // p = r + beta*(p - omega*v)
        for (var i = tid; i < n; i = i + WG) {
            vecs[vidx(VP, c, i)] = vecs[vidx(VR, c, i)]
                + beta * (vecs[vidx(VP, c, i)] - scal[2] * vecs[vidx(VV, c, i)]);
        }
        workgroupBarrier();

        // v = A p
        spmv(c, VP, VV, tid);
        workgroupBarrier();

        let rv = dot_reduce(c, VRH, VV, tid);
        if (abs(rv) < 1e-30) { break; }
        let alpha = rho_new / rv;

        // s = r - alpha*v ; x += alpha*p
        for (var i = tid; i < n; i = i + WG) {
            vecs[vidx(VS, c, i)] = vecs[vidx(VR, c, i)] - alpha * vecs[vidx(VV, c, i)];
            vecs[vidx(VX, c, i)] = vecs[vidx(VX, c, i)] + alpha * vecs[vidx(VP, c, i)];
        }
        workgroupBarrier();

        // 检查 ||s||
        let s_norm2 = dot_reduce(c, VS, VS, tid);
        if (sqrt(s_norm2) < params.tol) {
            if (tid == 0u) { scal[3] = sqrt(s_norm2); }
            workgroupBarrier();
            converged = true;
            break;
        }

        // t = A s
        spmv(c, VS, VT, tid);
        workgroupBarrier();

        let tt = dot_reduce(c, VT, VT, tid);
        let ts = dot_reduce(c, VT, VS, tid);
        if (abs(tt) < 1e-30) { break; }
        let omega = ts / tt;

        // x += omega*s ; r = s - omega*t
        for (var i = tid; i < n; i = i + WG) {
            vecs[vidx(VX, c, i)] = vecs[vidx(VX, c, i)] + omega * vecs[vidx(VS, c, i)];
            vecs[vidx(VR, c, i)] = vecs[vidx(VS, c, i)] - omega * vecs[vidx(VT, c, i)];
        }
        if (tid == 0u) {
            scal[0] = rho_new;
            scal[1] = alpha;
            scal[2] = omega;
        }
        workgroupBarrier();

        // 收敛判定
        residual_norm(c, tid);
        if (scal[3] < params.tol) {
            converged = true;
        }
    }

    // 输出解向量
    for (var i = tid; i < n; i = i + WG) {
        x_out[c * params.max_n + i] = vecs[vidx(VX, c, i)];
    }
    // 输出前做真实残差校验：r_true = rhs − A·x
    // （BiCGSTAB 可能"假收敛"：中间某步 ||s|| 恰好很小但并非解，
    //   尤其是谱半径 > 1 的发散系统；必须以真实残差为准）
    spmv(c, VX, VT, tid);
    workgroupBarrier();
    for (var i = tid; i < n; i = i + WG) {
        vecs[vidx(VS, c, i)] = vecs[vidx(VB, c, i)] - vecs[vidx(VT, c, i)];
    }
    workgroupBarrier();
    let true_r2 = dot_reduce(c, VS, VS, tid);
    let true_r = sqrt(true_r2);
    if (tid == 0u) {
        let ok = true_r < params.tol;
        out_info[c * 4u + 0u] = f32(iters);
        out_info[c * 4u + 1u] = true_r;
        out_info[c * 4u + 2u] = select(0.0, 1.0, ok);
    }
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

/// GPU 流量线性求解器（BiCGSTAB 批量）。
pub struct GpuFlowSolver {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    pub adapter_name: String,
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

        let adapter_name = adapter.get_info().name;
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
        let entry = |binding: u32, uniform: bool, read_only: bool| wgpu::BindGroupLayoutEntry {
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
            label: Some("gtp-linear-layout"),
            entries: &[
                entry(0, true, true),
                entry(1, false, true),
                entry(2, false, true),
                entry(3, false, true),
                entry(4, false, false),
                entry(5, false, false),
                entry(6, false, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gtp-linear-pl"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("bicgstab"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("bicgstab"),
            compilation_options: Default::default(),
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            layout,
            adapter_name,
        })
    }

    /// 批量求解（BiCGSTAB）。返回每个系统的解与收敛信息。
    pub fn solve_batch(
        &self,
        systems: &[FlowSystem],
        max_iters: u32,
        tol: f32,
    ) -> Vec<SolveResult> {
        if systems.is_empty() {
            return Vec::new();
        }
        use wgpu::util::DeviceExt;
        let c = systems.len();
        let max_n = systems.iter().map(|s| s.n).max().unwrap_or(0);
        let max_nnz = systems.iter().map(|s| s.col_idx.len()).max().unwrap_or(0).max(1);
        let iters = max_iters.max(1);

        let mut row_ptr = vec![0u32; c * (max_n + 1)];
        let mut col_idx = vec![0u32; c * max_nnz];
        let mut values = vec![0f32; c * max_nnz];
        let mut vecs = vec![0f32; 8 * c * max_n];
        for (ci, s) in systems.iter().enumerate() {
            let rp_base = ci * (max_n + 1);
            let nnz_base = ci * max_nnz;
            let nnz = s.col_idx.len();
            for i in 0..=s.n {
                row_ptr[rp_base + i] = s.row_ptr[i];
            }
            for i in (s.n + 1)..=max_n {
                row_ptr[rp_base + i] = nnz as u32;
            }
            col_idx[nnz_base..nnz_base + nnz].copy_from_slice(&s.col_idx);
            values[nnz_base..nnz_base + nnz].copy_from_slice(&s.values);
            // rhs 打包为向量 7
            let vb_base = (7 * c + ci) * max_n;
            vecs[vb_base..vb_base + s.n].copy_from_slice(&s.rhs);
        }

        let params = Params {
            max_n: max_n as u32,
            max_nnz: max_nnz as u32,
            n_candidates: c as u32,
            max_iters: iters,
            tol,
            _pad: [0.0; 3],
        };

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
        let vecs_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vecs"),
                contents: bytemuck::cast_slice(&vecs),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let params_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let info_size = ((c * 4) as u64 * 4).max(16);
        let info_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("out_info"),
            size: info_size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&info_buf, 0, &vec![0u8; info_size as usize]);
        let x_size = ((c * max_n) as u64 * 4).max(4);
        let x_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("x_out"),
            size: x_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let info_staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("info-staging"),
            size: info_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let x_staging = self.device.create_buffer(&wgpu::BufferDescriptor {
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
                    resource: vecs_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: info_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
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
                label: Some("bicgstab-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(c as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&info_buf, 0, &info_staging, 0, info_size);
        encoder.copy_buffer_to_buffer(&x_buf, 0, &x_staging, 0, x_size);
        self.queue.submit(Some(encoder.finish()));

        info_staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        x_staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());

        let info_data = info_staging.slice(..).get_mapped_range();
        let info: &[f32] = bytemuck::cast_slice(&info_data);
        let x_data = x_staging.slice(..).get_mapped_range();
        let x_all: &[f32] = bytemuck::cast_slice(&x_data);

        let mut out = Vec::with_capacity(c);
        for (ci, s) in systems.iter().enumerate() {
            let base = ci * max_n;
            let x = x_all[base..base + s.n].to_vec();
            let it = info[ci * 4] as u32;
            let residual = info[ci * 4 + 1];
            let converged = info[ci * 4 + 2] > 0.5;
            if std::env::var_os("GTP_DEBUG_LINEAR").is_some() {
                eprintln!(
                    "linear[{}]: info=[{:.3}, {:.3e}, {:.3}, {:.3}] iters={} residual={:.3e} converged={}",
                    ci, info[ci * 4], info[ci * 4 + 1], info[ci * 4 + 2], info[ci * 4 + 3],
                    it, residual, converged
                );
            }
            out.push(SolveResult {
                x,
                iterations: it,
                residual,
                converged,
            });
        }
        drop(info_data);
        drop(x_data);
        info_staging.unmap();
        x_staging.unmap();
        let converged = out.iter().filter(|r| r.converged).count();
        let iters: u32 = out.iter().map(|r| r.iterations).sum();
        log::debug!(
            "bicgstab batch: {} 系统 / 收敛 {} / 迭代合计 {}",
            out.len(),
            converged,
            iters
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_system() -> FlowSystem {
        // 链：ore(原料) → a → b（每步 1:1），目标 b 60/min
        // 材料 0=ore(无配方), 1=a, 2=b
        // 物料平衡（A x = rhs）：
        //   行 0：x0 = 0                （ore 无配方）
        //   行 1：x1 − x2 = 0            （a 的产出被 b 消耗）
        //   行 2：x2 = 60                （b 的产出满足目标）
        let n = 3;
        let mut row_ptr = vec![0u32];
        let mut col_idx = Vec::new();
        let mut values = Vec::new();
        // 行 0：x0
        col_idx.push(0);
        values.push(1.0);
        row_ptr.push(1);
        // 行 1：x1 - x2
        col_idx.push(2);
        values.push(-1.0);
        col_idx.push(1);
        values.push(1.0);
        row_ptr.push(3);
        // 行 2：x2
        col_idx.push(2);
        values.push(1.0);
        row_ptr.push(4);
        FlowSystem {
            n,
            out_qty: vec![0.0, 1.0, 1.0],
            demand: vec![0.0, 0.0, 60.0],
            row_ptr,
            col_idx,
            values,
            rhs: vec![0.0, 0.0, 60.0],
        }
    }

    #[test]
    fn jacobi_cpu_solves_chain() {
        let sys = chain_system();
        let x = sys.solve_cpu(500, 1.0);
        assert!((x[1] - 60.0).abs() < 1e-3, "x1 = {}", x[1]);
        assert!((x[2] - 60.0).abs() < 1e-3, "x2 = {}", x[2]);
    }

    #[test]
    fn spmv_matches_expected() {
        let sys = chain_system();
        let x = vec![0.0, 60.0, 60.0];
        let y = sys.spmv(&x);
        assert!((y[0] - 0.0).abs() < 1e-6);
        assert!((y[1] - 0.0).abs() < 1e-6); // x1 - x2
        assert!((y[2] - 60.0).abs() < 1e-6); // x2
    }

    #[test]
    fn consumed_recovered() {
        let sys = chain_system();
        let x = vec![0.0, 60.0, 60.0];
        let c = sys.consumed(&x);
        assert!((c[1] - 60.0).abs() < 1e-6); // a 被 b 消耗 60
        assert!((c[2] - 0.0).abs() < 1e-6); // b 无消耗
    }
}
