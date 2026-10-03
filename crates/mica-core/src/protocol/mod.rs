//! 现代终端协议的自有增量(M4,主 spec §9):alacritty_terminal 不含的
//! 输入/旁路协议在此。M4a:kitty keyboard 编码器;bracketed paste 状态
//! 由上游 TermMode 承载,消费方经 Surface 读取。

pub mod kitty;
pub mod sidecar;
