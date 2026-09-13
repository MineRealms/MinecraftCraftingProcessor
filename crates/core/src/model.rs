use serde::Serialize;

/// 材料 / 配方的稠密索引。
pub type MaterialId = u32;
pub type RecipeId = u32;
pub type CategoryId = u32;

/// 原料大类。`Other` 携带驻留后的类型字符串 id（本数据集未出现，兜底用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaterialKind {
    Item,
    Fluid,
    Other(u32),
}

impl MaterialKind {
    pub fn is_item(self) -> bool {
        matches!(self, MaterialKind::Item)
    }
    pub fn is_fluid(self) -> bool {
        matches!(self, MaterialKind::Fluid)
    }
}

/// 材料主键：类型 + 驻留 id + 可选驻留 NBT。
///
/// 关键语义：**同 id 不同 NBT 是不同材料**（带材质的工具等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaterialKey {
    pub kind: MaterialKind,
    pub id: u32,
    pub nbt: Option<u32>,
}

/// 材料事实记录。
#[derive(Debug, Clone)]
pub struct MaterialInfo {
    pub key: MaterialKey,
    /// 可读名（短 id，下划线转空格；含 NBT 时追加标记）。
    pub display: String,
}

/// 配方槽：`alts` 是多选一（OR）候选，`(材料, 数量)`。
/// 数量语义：物品 = 个；流体 = mB。
#[derive(Debug, Clone)]
pub struct Slot {
    pub name: String,
    pub alts: Vec<(MaterialId, u64)>,
}

impl Slot {
    /// 主候选（解析顺序第一个），用于快速数量估算。
    pub fn primary(&self) -> Option<(MaterialId, u64)> {
        self.alts.first().copied()
    }
}

/// 配方节点（Knowledge IR）。
#[derive(Debug, Clone)]
pub struct RecipeNode {
    pub category: CategoryId,
    /// JEI 导出的配方 id（分类内唯一）；缺失时退化为 `<type>#<序号>`。
    pub id: String,
    pub inputs: Vec<Slot>,
    pub outputs: Vec<Slot>,
    /// GT 配方数据（仅 GT 配方有；原版合成等为 None）。
    pub gt: Option<GtRecipeInfo>,
}

/// 能量方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnergyIo {
    /// 耗电
    In,
    /// 发电
    Out,
}

/// GT 配方需求块（GTCEu API 导出）。
#[derive(Debug, Clone)]
pub struct GtRecipeInfo {
    pub recipe_type: Option<String>,
    /// 配方时长（tick，20 = 1 秒）
    pub duration_ticks: u32,
    pub parallels: u32,
    /// EU/t（每 tick 能量）
    pub eut: f64,
    pub amperage: f64,
    pub energy_io: Option<EnergyIo>,
    /// 等级名（LV/MV/…）
    pub tier: Option<String>,
    /// 等级序号（便于数值比较）
    pub tier_index: Option<u8>,
    /// 最低电压
    pub voltage: f64,
    /// eut × A
    pub total_eu_t: f64,
    /// total_eu_t × duration
    pub total_eu: f64,
}

impl GtRecipeInfo {
    /// 是否消耗能量（燃料类配方无能量字段）。
    pub fn consumes_energy(&self) -> bool {
        self.energy_io == Some(EnergyIo::In)
    }
    pub fn generates_energy(&self) -> bool {
        self.energy_io == Some(EnergyIo::Out)
    }
}

impl RecipeNode {
    /// 主输出（第一个输出槽的第一个候选）。
    pub fn primary_output(&self) -> Option<(MaterialId, u64)> {
        self.outputs.first().and_then(|s| s.primary())
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.outputs.is_empty()
    }
}

/// 分类的机器催化剂（能执行该分类的机器）。
#[derive(Debug, Clone)]
pub struct Catalyst {
    pub material: MaterialId,
    pub count: u64,
}

/// 分类（JEI RecipeType）事实记录。
#[derive(Debug, Clone)]
pub struct CategoryInfo {
    pub ty: String,
    pub title: String,
    pub recipe_class: Option<String>,
    pub catalysts: Vec<Catalyst>,
    pub recipe_count: usize,
}

// ---------------------------------------------------------------------------
// DTO：面向前端 / API 的序列化结构（内部类型不直接序列化）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MaterialDto {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbt: Option<String>,
    /// 回退名（短 id）
    pub display: String,
    /// 中文名（来自 jei_names.json；缺失回退 display）
    pub display_zh: String,
    /// 英文名（来自 jei_names.json；缺失回退 display）
    pub display_en: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngredientDto {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub display: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotDto {
    pub name: String,
    pub ingredients: Vec<IngredientDto>,
}
