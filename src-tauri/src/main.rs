// 桌面入口：macOS/Windows 上通过它启动
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    canlow_next_lib::run();
}
