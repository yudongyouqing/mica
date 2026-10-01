//! 单实例 IPC 协议(M3a,spec §4):named pipe 上的 JSON 帧。
//!
//! 帧格式:`u32 LE payload 长度 + JSON 单行`。纯逻辑零 IO——管道读写由
//! 壳层负责(app/src/ipc.rs),编解码在这里 TDD。

use serde::{Deserialize, Serialize};

/// 客户端 → 服务端。op 语义:
/// - `activate`:聚焦既有窗口(二次启动/new-tab 默认附带)
/// - `new-tab`:开新标签;profile 缺省走默认 shell
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcMessage {
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl IpcMessage {
    pub fn activate() -> Self {
        Self {
            op: "activate".into(),
            profile: None,
        }
    }

    pub fn new_tab(profile: Option<String>) -> Self {
        Self {
            op: "new-tab".into(),
            profile,
        }
    }
}

/// 编码帧:4 字节 LE 长度 + JSON。
pub fn encode(msg: &IpcMessage) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("IpcMessage 序列化不失败");
    let len = (json.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len);
    out.extend_from_slice(&json);
    out
}

/// 解码完整帧(长度前缀 + payload)。长度不符/JSON 坏 → None(客户端重发
/// 或放弃,协议无重试语义)。
pub fn decode(buf: &[u8]) -> Option<IpcMessage> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let payload = buf.get(4..4 + len)?;
    serde_json::from_slice(payload).ok()
}

/// 读帧辅助:从流缓冲头部取一帧,返回 (消息, 消费字节数);不足一帧 → None
/// 且消费 0(调用方继续累积)。服务端读循环用。
pub fn take_frame(buf: &[u8]) -> Option<(IpcMessage, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let msg = decode(&buf[..4 + len])?;
    Some((msg, 4 + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_both_ops() {
        let msgs = [
            IpcMessage::activate(),
            IpcMessage::new_tab(None),
            IpcMessage::new_tab(Some("Ubuntu".into())),
        ];
        for m in &msgs {
            assert_eq!(decode(&encode(m)).as_ref(), Some(m));
        }
    }

    #[test]
    fn profile_omitted_when_none() {
        let enc = encode(&IpcMessage::activate());
        let json = &enc[4..];
        assert!(
            !json
                .windows(b"\"profile\"".len())
                .any(|w| w == b"\"profile\""),
            "None 不上线路减少字节:{json:?}"
        );
    }

    #[test]
    fn bad_frames_rejected() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[1, 2, 3]), None, "长度头不足");
        assert_eq!(decode(&[0xff, 0, 0, 0, 1]), None, "长度超界");
        let mut garbage = (4u32).to_le_bytes().to_vec();
        garbage.extend_from_slice(b"not json");
        assert_eq!(decode(&garbage), None);
    }

    #[test]
    fn take_frame_waits_for_complete_payload() {
        let full = encode(&IpcMessage::new_tab(Some("cmd".into())));
        // 半帧:不可取
        assert_eq!(take_frame(&full[..full.len() - 1]), None);
        // 全帧:可取且消费正确
        let (msg, used) = take_frame(&full).unwrap();
        assert_eq!(msg.profile.as_deref(), Some("cmd"));
        assert_eq!(used, full.len());
    }
}
