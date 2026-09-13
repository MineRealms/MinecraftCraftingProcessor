use std::collections::HashMap;

/// 字符串驻留表：把重复出现的字符串（材料 id、NBT、类型名）压成 u32。
///
/// 数据集实测有 50 万+ 次原料出现，但唯一 id 只有数万个；
/// 驻留后 `MaterialKey` 可以做到 12 字节且 Copy。
#[derive(Default)]
pub struct Interner {
    map: HashMap<String, u32>,
    vec: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 驻留字符串，返回稳定 id。
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.vec.len() as u32;
        self.vec.push(s.to_owned());
        self.map.insert(s.to_owned(), id);
        id
    }

    /// 查找已驻留字符串的 id（不插入）。
    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.map.get(s).copied()
    }

    /// 按 id 取字符串。调用方需保证 id 来自本驻留表。
    pub fn get(&self, id: u32) -> &str {
        &self.vec[id as usize]
    }

    pub fn len(&self) -> usize {
        self.vec.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_roundtrip() {
        let mut it = Interner::new();
        let a = it.intern("gtceu:iron");
        let b = it.intern("gtceu:copper");
        let a2 = it.intern("gtceu:iron");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(it.get(a), "gtceu:iron");
        assert_eq!(it.lookup("gtceu:copper"), Some(b));
        assert_eq!(it.lookup("nope"), None);
        assert_eq!(it.len(), 2);
    }
}
