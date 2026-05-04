//! 日志：通过 C 回调吐回宿主进程 + 同步 last_error 缓存
//!
//! 设计：
//! - 异步路径：用 `AtomicPtr` 存放函数指针，注册/调用都是线程安全的；
//!   未注册时 emit 直接 short-circuit，零开销。
//! - 同步路径：任何 ERROR 级别（>=4）的 emit 同时更新 `LAST_ERROR`，
//!   宿主可通过 `renderer_last_error_string` 拉取 —— 在 reverse P/Invoke
//!   marshalling 不可靠的环境（UWP/.NET Native）下作为可靠诊断通道。

use std::ffi::CString;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

use crate::ffi::LogCallbackFn;

/// 全局回调槽。`null` 表示未注册。
static CALLBACK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// 最近一条 ERROR 级别的日志文本。线程安全。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 注册回调。传入 `None` 等价于注销。线程安全。
pub(crate) fn set_callback(cb: Option<LogCallbackFn>) {
    let raw = match cb {
        Some(f) => f as *mut (),
        None => ptr::null_mut(),
    };
    CALLBACK.store(raw, Ordering::Release);
}

/// 主动写一条日志。线程安全。
///
/// - 任何 ERROR 级别 (>= 4) 都覆盖更新 `LAST_ERROR`，宿主可通过
///   `renderer_last_error_string` 拉取。**最后一条 ERROR 留下来**，所以调用链中
///   越深、越具体的 emit 应当越靠后；外层包装错误（比如 `renderer_create failed`
///   汇总）应当用 INFO/WARN 级别（< 4），避免把内层详细诊断（attach 失败 +
///   IID 列表）盖掉。
/// - 回调未注册时 callback 路径 short-circuit，但 LAST_ERROR 仍然按规则写入。
#[allow(dead_code)]
pub(crate) fn emit(level: i32, msg: &str) {
    // ERROR 级别总是覆盖（深度优先：最后一条 = 最具体的诊断）
    if level >= 4 {
        if let Ok(mut g) = LAST_ERROR.lock() {
            *g = Some(msg.to_string());
        }
    }

    // 异步 callback 路径
    let raw = CALLBACK.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    let Ok(cstring) = CString::new(msg) else {
        return;
    };
    let cb: LogCallbackFn = unsafe { std::mem::transmute(raw) };
    unsafe {
        cb(level, cstring.as_ptr());
    }
}

/// 清空 `LAST_ERROR`。在新一次诊断会话开始前调用，避免上次残留干扰本次诊断。
/// 典型用法：在 `renderer_create` 入口先 clear。
pub(crate) fn clear_last_error() {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = None;
    }
}

/// 取最近一条错误的 clone。返回 `None` 表示从未发生过 ERROR。
pub(crate) fn last_error_string() -> Option<String> {
    LAST_ERROR.lock().ok().and_then(|g| g.clone())
}

