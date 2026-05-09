pub fn emit(level: i32, msg: &str) {
    eprintln!("[L{}] {}", level, msg);
}
