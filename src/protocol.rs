//! CAN 与主机通信的纯协议层：类型、逐字节组行、命令解析和回复编码。
//!
//! 本模块只依赖 core，不读写 USB/CAN，不校验会话是否仍有效。
//! HELLO、STREAM 和带会话编号的 TX 能被解析，不等于对应桥接任务已经运行。

use core::fmt::{self, Write};

/// 一行最多保存 96 字节，不包含 LF；若使用 CRLF，CR 占一个字节。
pub const MAX_LINE_LEN: usize = 96;
/// 回复最多 63 字节，低于当前 USB 端点的 64 字节包长。
pub const MAX_REPLY_LEN: usize = 63;

// derive 让编译器生成调试打印、复制和比较实现，便于传递值和验证结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Syntax,
    Frame,
    Session,
    LineTooLong,
    Encoding,
}

impl ParseError {
    /// 返回固定错误行；&'static str 表示它引用程序中已有的字符串常量。
    pub const fn reply(self) -> &'static str {
        match self {
            Self::Syntax => "ERR SYNTAX\n",
            Self::Frame => "ERR FRAME\n",
            Self::Session => "ERR SESSION\n",
            Self::LineTooLong => "ERR LINE_TOO_LONG\n",
            Self::Encoding => "ERR ENCODING\n",
        }
    }
}

/// 标准 Classical CAN 数据帧。len 指出 data 中前几个字节有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanFrame {
    pub id: u16,
    pub len: u8,
    pub data: [u8; 8],
}

impl CanFrame {
    pub fn new(id: u16, payload: &[u8]) -> Result<Self, ParseError> {
        if id > 0x7FF || payload.len() > 8 {
            return Err(ParseError::Frame);
        }
        let mut data = [0; 8];
        // 复制实际数据；未使用的位置保留为零，不借用临时接收缓冲。
        data[..payload.len()].copy_from_slice(payload);
        Ok(Self {
            id,
            len: payload.len() as u8,
            data,
        })
    }

    pub fn payload(&self) -> Result<&[u8], ParseError> {
        // 字段是 pub，调用者也能手动构造值，所以切片前再次检查边界。
        if self.id > 0x7FF || self.len > 8 {
            return Err(ParseError::Frame);
        }
        Ok(&self.data[..usize::from(self.len)])
    }
}

/// 带会话标记的发送请求，保留你已经建立的四个字段。
/// 请求拥有全部数据，后续可放入队列，不会引用被下一包覆盖的文本缓冲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanTxRequest {
    pub session: u32,
    pub id: u16,
    pub len: u8,
    pub data: [u8; 8],
}

impl CanTxRequest {
    /// 保留最初的 Option 接口；这里只接受带 nonce 和显式长度的新 TX 格式。
    pub fn parse(command: &str) -> Option<Self> {
        // .ok()? 把解析失败转换成 None；成功后只提取 CanTx 这个枚举分支。
        match Command::parse(command).ok()? {
            Command::CanTx(request) => Some(request),
            _ => None,
        }
    }

    pub fn frame(&self) -> Result<CanFrame, ParseError> {
        let frame = CanFrame {
            id: self.id,
            len: self.len,
            data: self.data,
        };
        // ? 表示：若校验失败，立即将错误交给调用者。
        frame.payload()?;
        Ok(frame)
    }
}

/// enum 表示命令只能是这些分支之一；各分支保存自己需要的数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Ping,
    SwdId,
    Hello { session: u32 },
    Stream { session: u32, enabled: bool },
    CanTx(CanTxRequest),
    LegacyCanTx(CanFrame),
    CanRx,
}

impl Command {
    /// 输入是一条完整命令；也允许调用者传入带结尾 LF 或 CRLF 的完整行。
    pub fn parse(command: &str) -> Result<Self, ParseError> {
        let line = command.strip_suffix('\n').unwrap_or(command);
        if line.len() > MAX_LINE_LEN {
            return Err(ParseError::LineTooLong);
        }
        let line = if command.ends_with('\n') {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        if !line.is_ascii() {
            return Err(ParseError::Encoding);
        }
        // 完整行中不能再藏换行或其他控制字符；允许空格和 TAB 分隔字段。
        if line.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(ParseError::Syntax);
        }

        let mut words = line.split_ascii_whitespace();
        let command = match (words.next(), words.next()) {
            (Some("PING"), None) => Self::Ping,
            (Some("SWD"), Some("ID")) => Self::SwdId,
            (Some("CAN"), Some("RX")) => Self::CanRx,
            (Some("CAN"), Some("HELLO")) => {
                let session = parse_session(words.next().ok_or(ParseError::Syntax)?)?;
                Self::Hello { session }
            }
            (Some("CAN"), Some("STREAM")) => {
                let session = parse_session(words.next().ok_or(ParseError::Syntax)?)?;
                let enabled = match words.next() {
                    Some("ON") => true,
                    Some("OFF") => false,
                    _ => return Err(ParseError::Syntax),
                };
                Self::Stream { session, enabled }
            }
            (Some("CAN"), Some("TX")) => {
                let first = words.next().ok_or(ParseError::Syntax)?;
                let second = words.next().ok_or(ParseError::Syntax)?;
                // 只剩 ID、HEX 两个字段的是旧格式；有其他字段就按新格式校验。
                match words.next() {
                    None => Self::LegacyCanTx(parse_frame(first, None, second)?),
                    Some(length) => {
                        let payload = words.next().ok_or(ParseError::Syntax)?;
                        let session = parse_session(first)?;
                        let frame = parse_frame(second, Some(length), payload)?;
                        Self::CanTx(CanTxRequest {
                            session,
                            id: frame.id,
                            len: frame.len,
                            data: frame.data,
                        })
                    }
                }
            }
            _ => return Err(ParseError::Syntax),
        };

        // 已经解析出命令后，仍要拒绝多余字段，不能悄悄忽略输入尾部。
        if words.next().is_some() {
            return Err(ParseError::Syntax);
        }
        Ok(command)
    }
}

fn parse_session(text: &str) -> Result<u32, ParseError> {
    // 会话编号严格为 8 个 HEX 字符，不接受加号或 0x 前缀。
    if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ParseError::Session);
    }
    u32::from_str_radix(text, 16).map_err(|_| ParseError::Session)
}

fn parse_frame(id: &str, length: Option<&str>, payload: &str) -> Result<CanFrame, ParseError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ParseError::Frame);
    }
    let id = u16::from_str_radix(id, 16).map_err(|_| ParseError::Frame)?;
    if id > 0x7FF {
        return Err(ParseError::Frame);
    }

    // 长度以十进制书写；旧格式没有这个字段，就由 HEX 字符数推导。
    let explicit_len = match length {
        Some(text) => {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ParseError::Frame);
            }
            let len = text.parse::<u8>().map_err(|_| ParseError::Frame)?;
            if len > 8 {
                return Err(ParseError::Frame);
            }
            Some(usize::from(len))
        }
        None => None,
    };
    let hex = if payload == "-" { "" } else { payload };
    if hex.len() > 16 || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ParseError::Frame);
    }
    let len = hex.len() / 2;
    if explicit_len.is_some_and(|expected| expected != len) {
        return Err(ParseError::Frame);
    }
    let mut data = [0; 8];
    // 前面已确认全部是 ASCII，每次取两个字符，才能安全按字节索引字符串。
    for (i, slot) in data[..len].iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ParseError::Frame)?;
    }
    CanFrame::new(id, &data[..len])
}

/// 逐字节组行。USB 包的边界不影响这里的状态，只有 LF 才结束一条命令。
pub struct Line {
    bytes: [u8; MAX_LINE_LEN],
    len: usize,
    discarding: bool,
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}

impl Line {
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_LINE_LEN],
            len: 0,
            discarding: false,
        }
    }

    /// 结束会话时清掉半行和溢出状态，避免重连后拼接上一次的命令。
    pub fn reset(&mut self) {
        self.len = 0;
        self.discarding = false;
    }

    /// None：还没收完；Some(Ok(...))：完整行；Some(Err(...))：本行无效。
    pub fn push(&mut self, byte: u8) -> Option<Result<&str, ParseError>> {
        if byte == b'\n' {
            // take 取走旧长度并将长度归零，为下一行做准备。
            let n = core::mem::take(&mut self.len);
            if core::mem::take(&mut self.discarding) {
                return Some(Err(ParseError::LineTooLong));
            }
            let bytes = &self.bytes[..n];
            // CRLF 只去掉紧挨着 LF 的一个 CR；其他 CR 留给命令校验拒绝。
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            if !bytes.is_ascii() {
                return Some(Err(ParseError::Encoding));
            }
            return Some(core::str::from_utf8(bytes).map_err(|_| ParseError::Encoding));
        }
        if self.discarding {
            // 超长后一直吞掉数据，不能把尾部的 PING 等文字当成新命令。
            return None;
        }
        if self.len == self.bytes.len() {
            // 第 97 个非 LF 字节才溢出；恰好 96 字节的一行仍然有效。
            self.discarding = true;
            return None;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        None
    }
}

/// 回复只描述要编码的内容；创建 Hello/Stream 回复本身不会改变运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply<'a> {
    Pong,
    SwdId(u32),
    Hello(u32),
    Stream {
        session: u32,
        enabled: bool,
    },
    Queued {
        session: Option<u32>,
    },
    Received {
        session: Option<u32>,
        frame: &'a CanFrame,
    },
    Empty,
    Error(ParseError),
}

impl Reply<'_> {
    /// 输出到实现 Write 的缓冲；固件用固定容量 String，电脑测试用普通 String。
    /// 写失败会返回 fmt::Error，调用者必须丢弃不完整缓冲，不能发送半条回复。
    pub fn write_to(&self, out: &mut impl Write) -> fmt::Result {
        match *self {
            Self::Pong => out.write_str("OK DM-MC02\n"),
            Self::SwdId(id) => writeln!(out, "OK DPIDR {id:08X}"),
            Self::Hello(session) => writeln!(out, "OK HELLO {session:08X}"),
            Self::Stream { session, enabled } => {
                let mode = if enabled { "ON" } else { "OFF" };
                writeln!(out, "OK STREAM {session:08X} {mode}")
            }
            Self::Queued {
                session: Some(session),
            } => writeln!(out, "OK QUEUED {session:08X}"),
            Self::Queued { session: None } => out.write_str("OK QUEUED\n"),
            Self::Received { session, frame } => {
                // 在输出前校验 public 字段，防止无效 len 导致切片越界。
                let payload = frame.payload().map_err(|_| fmt::Error)?;
                match session {
                    Some(session) => {
                        write!(out, "RX {session:08X} {:03X} {} ", frame.id, frame.len)?
                    }
                    None => write!(out, "OK RX {:03X} ", frame.id)?,
                }
                if payload.is_empty() {
                    out.write_char('-')?;
                }
                for byte in payload {
                    write!(out, "{byte:02X}")?;
                }
                out.write_char('\n')
            }
            Self::Empty => out.write_str("EMPTY\n"),
            Self::Error(error) => out.write_str(error.reply()),
        }
    }
}
