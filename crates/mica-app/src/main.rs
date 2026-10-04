// GUI 子系统(1.0.7 终极根因修复):mica.exe 的 PE 头此前是 Console(3)
// ——双击启动时 Windows 把它当 console 程序交给 defterm 委托的终端
// 宿主(WT 弹出并显示我们的 stderr,"wt 总是跳出来"的机制本体)。
// release 用 GUI(2),debug 保留 Console 便于 cargo run 看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    std::process::exit(mica_app::handle_cli());
    #[cfg(not(windows))]
    eprintln!("mica-app only runs on Windows; this is a stub build.");
}
