//! 单实例 IPC 协议(M3a → 2026-11-02 T06 重造):named pipe 上的版本化
//! JSON 帧,请求-响应一一对应。
//!
//! 帧格式:`u32 LE payload 长度 + JSON 单行`。纯逻辑零 IO——管道读写由
//! 壳层负责(app/src/ipc.rs),编解码在这里 TDD。
//!
//! T06 语义:
//! - 请求带 `id`(request_id);响应带同 `id` + `status`——客户端必须校验
//!   应答与请求对应,成功返回不再"证明命令执行"(旧客户端读到任意字节
//!   即 Ok,服务端从未写 ACK)。
//! - 响应分级:`queued`(已入队,UI 将处理)/`applied`(UI 已执行)/
//!   `error:<code>`(拒绝或失败)。`send-text` 成功只承诺字节写入 PTY,
//!   不承诺 shell 执行完毕。
//! - 帧长上限 `MAX_FRAME_BYTES`(1 MiB):长度前缀越界、非法 JSON、未知
//!   操作必须立即拒绝——解码器区分 `NeedMore / Complete / Invalid`,
//!   坏帧不得当"未到齐"无限累积。

use serde::{Deserialize, Serialize};

/// 单帧上限(含长度前缀)。正常请求(几 KB 内的 profile 名/粘贴文本)
/// 远低于此;越界立即拒绝,坏客户端不能借半帧把服务端缓冲撑到无界。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 客户端 → 服务端。op 语义:
/// - `activate`:聚焦既有窗口(二次启动/new-tab 默认附带)
/// - `new-tab`:开新标签;profile 缺省走默认 shell;font_size 覆盖
///   启动字号(M5a/T9 每标签字号,pt;None = 跟随全局)
/// - `new-window`:既有实例起第二个窗口
/// - `send-text`:向目标 pane 注入输入(tmux send-keys 同义)——
///   成功仅承诺字节写入 PTY
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    /// 请求标识:响应必须回带同一个 id(客户端校验对应关系)
    pub id: u64,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// #44 send-text:注入的文本(CLI 侧已做转义展开)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// #44 send-text 目标:None = 活跃标签,Some(n) = 第 n 标签,
    /// Some(-1) = Quick Terminal
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<i64>,
}

impl IpcRequest {
    pub fn new(id: u64, op: &str) -> Self {
        Self {
            id,
            op: op.into(),
            profile: None,
            font_size: None,
            text: None,
            tab: None,
        }
    }

    /// 服务端已知的操作集合(未知 op 在入队前即拒绝)。
    pub fn is_known_op(op: &str) -> bool {
        matches!(op, "activate" | "new-tab" | "new-window" | "send-text")
    }
}

/// 响应状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcStatus {
    /// 已入队(UI 将处理;服务端无法同步确认执行时的过渡态)
    Queued,
    /// UI 已执行(activate/new-tab/new-window/send-text 写入成功)
    Applied,
    /// 拒绝或失败,detail 携带错误码(见 IpcError)
    Error,
}

/// 错误码(error status 的 detail)。CLI 以非零退出 + 文案表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// 帧长超过 MAX_FRAME_BYTES
    FrameTooLarge,
    /// 长度前缀后的 JSON 不合法
    BadJson,
    /// 未知操作
    UnknownOp,
    /// send-text 目标(tab 序号/QT)不存在
    BadTarget,
    /// profile 名不存在
    BadProfile,
    /// font_size 非有限值或超合理范围
    BadFontSize,
    /// 无存活窗口可执行(进程退出中)
    NoWindow,
    /// UI 已入队但未在期限内应答——请求**可能已执行**,勿盲目重试
    Timeout,
    /// 管道不存在(无运行实例)——唯一可安全自启的形态
    NoInstance,
    /// 管道存在但占用超预算(实例活着,勿自启)
    Busy,
    /// 打开管道被拒(权限/提升边界不匹配)
    AccessDenied,
    /// 请求已发出后连接断开——**结果未知**,勿盲目重试
    Disconnected,
    /// 会话回收通道故障态(DEGRADED),暂不能建新会话
    Degraded,
    /// shell/pty 创建失败
    SpawnFailed,
    /// pty 写入失败
    PtyWrite,
    /// 输入通道饱和(队列+暂存都满),字节被整体拒收
    InputOverflow,
    /// 其余传输层失败
    Transport,
}

impl IpcError {
    pub fn code(self) -> &'static str {
        match self {
            IpcError::FrameTooLarge => "frame_too_large",
            IpcError::BadJson => "bad_json",
            IpcError::UnknownOp => "unknown_op",
            IpcError::BadTarget => "bad_target",
            IpcError::BadProfile => "bad_profile",
            IpcError::BadFontSize => "bad_font_size",
            IpcError::NoWindow => "no_window",
            IpcError::Timeout => "timeout",
            IpcError::NoInstance => "no_instance",
            IpcError::Busy => "busy",
            IpcError::AccessDenied => "access_denied",
            IpcError::Disconnected => "disconnected",
            IpcError::Degraded => "degraded",
            IpcError::SpawnFailed => "spawn_failed",
            IpcError::PtyWrite => "pty_write",
            IpcError::InputOverflow => "input_overflow",
            IpcError::Transport => "transport",
        }
    }
}

/// 服务端 → 客户端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    /// 与请求同 id;客户端必须校验
    pub id: u64,
    pub status: IpcStatus,
    /// error 时为 IpcError::code;成功时无
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl IpcResponse {
    pub fn queued(id: u64) -> Self {
        Self {
            id,
            status: IpcStatus::Queued,
            detail: None,
        }
    }
    pub fn applied(id: u64) -> Self {
        Self {
            id,
            status: IpcStatus::Applied,
            detail: None,
        }
    }
    pub fn error(id: u64, e: IpcError) -> Self {
        Self {
            id,
            status: IpcStatus::Error,
            detail: Some(e.code().into()),
        }
    }
}

/// #44 CLI 转义展开(`mica send` 默认模式,--literal 跳过):
/// 反斜杠+r/n 输出 CR(回车),反斜杠+e 输出 ESC,双反斜杠输出反斜杠;
/// 未知转义原样保留,其余 UTF-8 原样。纯函数零 IO,在此 TDD。
pub fn unescape_send(text: &str) -> Vec<u8> {
    let bs = '\\';
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != bs {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') | Some('n') => out.push(b'\r'),
            Some('e') => out.push(0x1B),
            Some('\\') => out.push(bs as u8),
            // 未知转义:反斜杠原样保留 + 该字符本身
            Some(other) => {
                out.push(bs as u8);
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(bs as u8), // 尾随孤立反斜杠
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 帧编解码
// ---------------------------------------------------------------------------

/// 增量解码状态:半帧继续累积,坏帧立即拒绝,不与"未到齐"混淆。
#[derive(Debug, Clone, PartialEq)]
pub enum FrameDecode<T> {
    /// 不足一帧(消费 0 字节,调用方继续累积)
    NeedMore,
    /// 完整一帧(T + 消费字节数)
    Complete(T, usize),
    /// 立即拒绝(超长/坏 JSON/坏长度头):调用方应回复错误并断开本连接
    Invalid(IpcError),
}

/// 帧编码:4 字节 LE 长度 + JSON。
pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("IPC 序列化不失败(消息类型受控)");
    let len = (json.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len);
    out.extend_from_slice(&json);
    out
}

/// 帧解码(通用:请求/响应共用一个壳)。
pub fn decode_frame<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> FrameDecode<T> {
    if buf.len() < 4 {
        return FrameDecode::NeedMore;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return FrameDecode::Invalid(IpcError::FrameTooLarge);
    }
    if buf.len() < 4 + len {
        return FrameDecode::NeedMore;
    }
    match serde_json::from_slice(&buf[4..4 + len]) {
        Ok(msg) => FrameDecode::Complete(msg, 4 + len),
        Err(_) => FrameDecode::Invalid(IpcError::BadJson),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_roundtrip_with_id() {
        let mut req = IpcRequest::new(42, "new-tab");
        req.profile = Some("cmd".into());
        req.font_size = Some(18.0);
        let enc = encode_frame(&req);
        match decode_frame::<IpcRequest>(&enc) {
            FrameDecode::Complete(got, used) => {
                assert_eq!(got, req);
                assert_eq!(used, enc.len());
            }
            other => panic!("应完整解码,得 {other:?}"),
        }
        let resp = IpcResponse::error(42, IpcError::BadTarget);
        match decode_frame::<IpcResponse>(&encode_frame(&resp)) {
            FrameDecode::Complete(got, _) => {
                assert_eq!(got.id, 42, "响应必须回带请求 id");
                assert_eq!(got.status, IpcStatus::Error);
                assert_eq!(got.detail.as_deref(), Some("bad_target"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// 老客户端兼容:缺 id 的请求不通过解码(协议版本化:新服务端拒绝
    /// 无 id 请求为 bad_json——id 缺失不是 JSON 错而是字段错,serde 会拒;
    /// 旧字段其余照常)。
    #[test]
    fn legacy_fields_still_parse_but_id_required() {
        let payload = br#"{"id":1,"op":"new-tab","profile":"cmd"}"#;
        let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(payload);
        match decode_frame::<IpcRequest>(&framed) {
            FrameDecode::Complete(m, _) => {
                assert_eq!(m.profile.as_deref(), Some("cmd"));
                assert_eq!(m.font_size, None, "缺省字段兜 None");
            }
            other => panic!("{other:?}"),
        }
        // 无 id:拒绝(不静默给 0)
        let bad = br#"{"op":"activate"}"#;
        let mut framed = (bad.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(bad);
        assert!(matches!(
            decode_frame::<IpcRequest>(&framed),
            FrameDecode::Invalid(IpcError::BadJson)
        ));
    }

    /// 半帧:NeedMore 且消费 0(继续累积),不误判坏帧。
    #[test]
    fn partial_frame_is_need_more() {
        let full = encode_frame(&IpcRequest::new(7, "activate"));
        assert_eq!(
            decode_frame::<IpcRequest>(&full[..3]),
            FrameDecode::NeedMore
        );
        assert_eq!(
            decode_frame::<IpcRequest>(&full[..full.len() - 1]),
            FrameDecode::NeedMore
        );
    }

    /// 超长:立即 Invalid,不累积。
    #[test]
    fn overlong_frame_rejected_immediately() {
        let mut buf = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(b"{\"id\":1,\"op\":\"activate\"}");
        assert_eq!(
            decode_frame::<IpcRequest>(&buf),
            FrameDecode::Invalid(IpcError::FrameTooLarge)
        );
        // 恰好在限内(长度头本身不超):走 NeedMore(载荷未到齐)
        let mut ok = (8u32).to_le_bytes().to_vec();
        ok.extend_from_slice(b"1234"); // 只到一半
        assert!(matches!(
            decode_frame::<IpcRequest>(&ok),
            FrameDecode::NeedMore
        ));
    }

    /// 坏 JSON:立即 Invalid。
    #[test]
    fn bad_json_rejected_immediately() {
        let mut buf = (8u32).to_le_bytes().to_vec();
        buf.extend_from_slice(b"not json");
        assert!(matches!(
            decode_frame::<IpcRequest>(&buf),
            FrameDecode::Invalid(IpcError::BadJson)
        ));
    }

    /// 未知操作在入队前拒绝。
    #[test]
    fn unknown_op_is_known_set() {
        assert!(IpcRequest::is_known_op("activate"));
        assert!(IpcRequest::is_known_op("new-tab"));
        assert!(IpcRequest::is_known_op("new-window"));
        assert!(IpcRequest::is_known_op("send-text"));
        assert!(!IpcRequest::is_known_op("rm-rf"));
    }

    /// #44:send-text 转义展开契约
    #[test]
    fn send_text_unescape() {
        let bs = '\\';
        let hi_cr = format!("hi{bs}r");
        let m = IpcRequest::new(1, "send-text");
        let _ = m;
        assert_eq!(unescape_send("a\\rb\\ec"), b"a\rb\x1bc".to_vec());
        assert_eq!(unescape_send("C:\\\\path"), b"C:\\path".to_vec());
        assert_eq!(unescape_send("w\\q"), b"w\\q".to_vec());
        assert_eq!(unescape_send("end\\"), b"end\\".to_vec());
        let mut want = "中文".as_bytes().to_vec();
        want.push(b'\r');
        assert_eq!(unescape_send("中文\\r"), want);
        assert_eq!(unescape_send(&hi_cr), b"hi\r".to_vec());
    }
}
