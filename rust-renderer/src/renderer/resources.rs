//! v0.7 phase 2 资源系统 —— u32 BitmapHandle + slot table + ABA 防护。
//!
//! ## 设计目标（决策 spec 第 3 节 + 10.4）
//!
//! - **单一 handle 类型**：业务方拿到 `u32 BitmapHandle`，不需要知道是
//!   文件 / 内存 / 外部纹理 / 视频帧 / 屏幕捕获。
//! - **显式生命周期**：每个 `*_load` / `*_create` 配 `*_destroy`；GC 不靠。
//! - **ABA 防护**：u32 = 16 位 slot index + 16 位 generation。slot 重用时
//!   generation +1，老 handle 拿过来 generation 不匹配 → `ResourceNotFound`。
//! - **零句柄保留**：`0` 始终非法，业务方可以用 0 表示「未初始化」零值。
//!   通过让 generation 起始 = 1 实现。
//! - **常量化容量**：`BITMAP_SLOT_CAPACITY` 一处定义，全局引用。
//!
//! ## 实现选择：enum SlotState
//!
//! `Vec<Option<Slot<T>>>` 看起来够，但 `Option::None` 不带 generation —— 我们需要
//! slot 被释放后**记住**它的 generation 才能在重新分配时 +1。所以 slot 内部用
//! `enum SlotState { Empty { gen }, Retired, Used { gen, value } }`。
//!
//! ## 不在范围
//!
//! - 弱引用观察（业务按句柄查 OK / NOT_FOUND 即可）
//! - 跨进程序列化（命令流话题，决策 10.5 推到 v0.8+）
//! - 自动 GC / 引用计数 —— 决策 10.7 显式释放，业务方写错就泄漏；可通过
//!   未来的 `get_resource_stats` 监控

use crate::error::{RendererError, RendererResult};

/// 决策 10.4：bitmap slot 上限。HUD overlay 通常 < 50 张并发，1024 是 20 倍 buffer。
/// 调整时只改这一处。
pub(crate) const BITMAP_SLOT_CAPACITY: usize = 1024;

/// 业务方持有的 bitmap 句柄（C ABI: u32）。
/// - bits [0..16]   slot index（max 65535，实际容量受 `BITMAP_SLOT_CAPACITY` 约束）
/// - bits [16..32]  generation counter（slot 重用时 +1）
///
/// `0` 永远非法 —— 业务方可以用 0 表示「未初始化」零值。
pub(crate) type BitmapHandle = u32;

fn split_handle(h: BitmapHandle) -> (u16, u16) {
    let index = (h & 0xFFFF) as u16;
    let generation = ((h >> 16) & 0xFFFF) as u16;
    (index, generation)
}

fn make_handle(index: u16, generation: u16) -> BitmapHandle {
    ((generation as u32) << 16) | (index as u32)
}

/// Slot 状态：空（带 generation 给重用 +1）、退休或占用（带 generation 配 value）。
enum SlotState<T> {
    Empty { generation: u16 },
    Retired,
    Used { generation: u16, value: T },
}

/// 通用 slot table。Phase 2 的 BitmapResource 是首个实例；
/// phase 4/5 的 VideoHandle / CaptureHandle 走同一套（决策：句柄统一）。
pub(crate) struct ResourceTable<T> {
    /// 容量定长 = `BITMAP_SLOT_CAPACITY`。
    /// 初始全 `Empty { generation: 1 }` —— generation 从 1 开始，保证首次分配的 handle ≠ 0。
    slots: Vec<SlotState<T>>,
    /// 回收队列（栈结构）。destroy 把 index push 进来；alloc 优先 pop 重用。
    free_list: Vec<u16>,
    /// 下一个从未分配过的 slot index。free_list 空时从这里取。
    next_fresh: u16,
}

impl<T> ResourceTable<T> {
    pub(crate) fn new() -> Self {
        let slots: Vec<SlotState<T>> = (0..BITMAP_SLOT_CAPACITY)
            .map(|_| SlotState::Empty { generation: 1 })
            .collect();
        Self {
            slots,
            free_list: Vec::with_capacity(64),
            next_fresh: 0,
        }
    }

    /// 占一个 slot 放入 value，返回 handle。
    /// 满 → `ResourceLimit`。
    pub(crate) fn insert(&mut self, value: T) -> RendererResult<BitmapHandle> {
        // 1) 优先复用 free_list
        while let Some(idx) = self.free_list.pop() {
            let slot = &mut self.slots[idx as usize];
            let new_gen = match slot {
                SlotState::Empty { generation } => *generation,
                SlotState::Retired => continue,
                SlotState::Used { .. } => {
                    // 防御：free_list 里的 slot 必须是 Empty。bug 走到这里数据已乱。
                    debug_assert!(false, "free_list contained a Used slot");
                    return Err(RendererError::ResourceLimit);
                }
            };
            *slot = SlotState::Used {
                generation: new_gen,
                value,
            };
            return Ok(make_handle(idx, new_gen));
        }

        // 2) 新 slot：从 next_fresh 取
        if (self.next_fresh as usize) >= BITMAP_SLOT_CAPACITY {
            return Err(RendererError::ResourceLimit);
        }
        let idx = self.next_fresh;
        self.next_fresh += 1;
        // 初始 generation = 1（new() 设的）—— 直接换 Used 保留同 generation。
        let gen_ = match &self.slots[idx as usize] {
            SlotState::Empty { generation } => *generation,
            SlotState::Retired | SlotState::Used { .. } => {
                debug_assert!(false, "next_fresh pointed at a non-empty slot");
                return Err(RendererError::ResourceLimit);
            }
        };
        self.slots[idx as usize] = SlotState::Used {
            generation: gen_,
            value,
        };
        Ok(make_handle(idx, gen_))
    }

    /// 按 handle 拿 &mut。失败 → `ResourceNotFound`。
    pub(crate) fn get_mut(&mut self, h: BitmapHandle) -> RendererResult<&mut T> {
        if h == 0 {
            return Err(RendererError::ResourceNotFound);
        }
        let (idx, gen_) = split_handle(h);
        let slot = self
            .slots
            .get_mut(idx as usize)
            .ok_or(RendererError::ResourceNotFound)?;
        match slot {
            SlotState::Used {
                generation, value, ..
            } if *generation == gen_ => Ok(value),
            _ => Err(RendererError::ResourceNotFound),
        }
    }

    /// 按 handle 拿 &T。
    pub(crate) fn get(&self, h: BitmapHandle) -> RendererResult<&T> {
        if h == 0 {
            return Err(RendererError::ResourceNotFound);
        }
        let (idx, gen_) = split_handle(h);
        let slot = self
            .slots
            .get(idx as usize)
            .ok_or(RendererError::ResourceNotFound)?;
        match slot {
            SlotState::Used {
                generation, value, ..
            } if *generation == gen_ => Ok(value),
            _ => Err(RendererError::ResourceNotFound),
        }
    }

    /// 按 handle 释放。已释放或失效视为 `ResourceNotFound`。
    /// 释放后 slot 未达 generation 上限则进 free_list；达到上限则退休（防 ABA）。
    pub(crate) fn remove(&mut self, h: BitmapHandle) -> RendererResult<T> {
        if h == 0 {
            return Err(RendererError::ResourceNotFound);
        }
        let (idx, gen_) = split_handle(h);
        let slot = self
            .slots
            .get_mut(idx as usize)
            .ok_or(RendererError::ResourceNotFound)?;
        match slot {
            SlotState::Used { generation, .. } if *generation == gen_ => {
                // 不能直接 take —— SlotState::Used 不是 Option。先 swap 出去。
                let taken = std::mem::replace(slot, SlotState::Empty { generation: 0 });
                let (taken_gen, value) = match taken {
                    SlotState::Used { generation, value } => (generation, value),
                    _ => unreachable!(),
                };
                if taken_gen == u16::MAX {
                    *slot = SlotState::Retired;
                } else {
                    *slot = SlotState::Empty {
                        generation: taken_gen + 1,
                    };
                    self.free_list.push(idx);
                }
                Ok(value)
            }
            _ => Err(RendererError::ResourceNotFound),
        }
    }

    /// 当前占用 slot 数。stats / 调试用。
    #[allow(dead_code)]
    pub(crate) fn allocated_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, SlotState::Used { .. }))
            .count()
    }
}

// ---------- 单元测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_remove_basic() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        let h = t.insert(42).unwrap();
        assert_ne!(h, 0, "handle must never be 0");
        assert_eq!(*t.get(h).unwrap(), 42);
        let v = t.remove(h).unwrap();
        assert_eq!(v, 42);
        assert!(matches!(t.get(h), Err(RendererError::ResourceNotFound)));
    }

    #[test]
    fn aba_protection_after_remove_old_handle_invalid() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        let h1 = t.insert(1).unwrap();
        t.remove(h1).unwrap();
        // 重新分配同 slot —— 新 handle generation 必然不同
        let h2 = t.insert(2).unwrap();
        assert_ne!(h1, h2, "ABA: reused slot must produce different handle");
        // 老句柄查询应失败
        assert!(matches!(t.get(h1), Err(RendererError::ResourceNotFound)));
        // 新句柄查询应成功
        assert_eq!(*t.get(h2).unwrap(), 2);
    }

    #[test]
    fn zero_handle_always_invalid() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        assert!(matches!(t.get(0), Err(RendererError::ResourceNotFound)));
        assert!(matches!(t.get_mut(0), Err(RendererError::ResourceNotFound)));
        assert!(matches!(t.remove(0), Err(RendererError::ResourceNotFound)));
    }

    #[test]
    fn capacity_exhaustion_returns_resource_limit() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        for i in 0..BITMAP_SLOT_CAPACITY {
            let h = t.insert(i as i32).expect("should fit");
            assert_ne!(h, 0);
        }
        // 第 1025 个应失败
        assert!(matches!(t.insert(9999), Err(RendererError::ResourceLimit)));
        assert_eq!(t.allocated_count(), BITMAP_SLOT_CAPACITY);
    }

    #[test]
    fn free_list_reuse_works() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        let h1 = t.insert(1).unwrap();
        let h2 = t.insert(2).unwrap();
        t.remove(h1).unwrap();
        // 容量是固定 1024 —— 重用 free_list 而不是占新 slot
        let h3 = t.insert(3).unwrap();
        assert_eq!(*t.get(h3).unwrap(), 3);
        // h2 仍然有效
        assert_eq!(*t.get(h2).unwrap(), 2);
        // h1 应失效
        assert!(matches!(t.get(h1), Err(RendererError::ResourceNotFound)));
    }

    #[test]
    fn double_remove_is_safe() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        let h = t.insert(1).unwrap();
        assert_eq!(t.remove(h).unwrap(), 1);
        assert!(matches!(t.remove(h), Err(RendererError::ResourceNotFound)));
    }

    #[test]
    fn max_generation_slot_is_retired_instead_of_wrapping() {
        let mut t: ResourceTable<i32> = ResourceTable::new();
        t.slots[0] = SlotState::Used {
            generation: u16::MAX,
            value: 7,
        };
        t.next_fresh = 1;

        let old_handle = make_handle(0, u16::MAX);
        assert_eq!(t.remove(old_handle).unwrap(), 7);
        assert!(matches!(
            t.get(old_handle),
            Err(RendererError::ResourceNotFound)
        ));

        let new_handle = t.insert(8).unwrap();
        let (new_index, new_generation) = split_handle(new_handle);
        assert_eq!(new_index, 1);
        assert_eq!(new_generation, 1);
        assert_eq!(*t.get(new_handle).unwrap(), 8);
    }
}
