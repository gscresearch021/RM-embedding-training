//! 会话感知的逐字节命令输入。
//!
//! Watch 只广播最新状态，快速变化可能被合并，get() 也会把更新标记为已读。
//! 因此不能只靠 changed() 事件清理半行；每次收到字节都要比较实际 generation。

use crate::{
    bridge_state::{Session, SessionTag},
    protocol::{Command, Line, ParseError},
};

/// 将行缓冲与它所属的会话代号放在一起，防止跨连接续接半条命令。
pub struct SessionLine {
    line: Line,
    generation: Option<u64>,
}

impl Default for SessionLine {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionLine {
    pub const fn new() -> Self {
        Self {
            line: Line::new(),
            generation: None,
        }
    }

    pub fn reset(&mut self) {
        self.line.reset();
        self.generation = None;
    }

    /// 比较最新快照；无需假定自己一定收到过状态变化通知。
    pub fn observe(&mut self, session: Session) {
        let generation = if session.connected {
            Some(session.generation)
        } else {
            None
        };
        if !session.connected || self.generation != generation {
            // 同时清理半行和超长丢弃状态，让新会话从干净边界开始。
            self.line.reset();
        }
        self.generation = generation;
    }

    /// None 表示未收完或未连接；完整行会立即解析成拥有自身数据的 Command。
    pub fn push(&mut self, session: Session, byte: u8) -> Option<Result<Command, ParseError>> {
        self.observe(session);
        if !session.connected {
            return None;
        }
        // and_then 只在组行成功时继续解析，组行错误则原样向调用者传递。
        // 返回值不借用 Line，后续等待队列时不会引用将被覆盖的文本缓冲。
        self.line
            .push(byte)
            .map(|line| line.and_then(Command::parse))
    }

    /// 唯一接收者自己执行 HELLO 成功后，接纳这次主动产生的新 generation。
    /// 调用前必须已结束上一行；这里只更新归属，不清掉同包后续命令的缓冲。
    pub fn adopt(&mut self, tag: SessionTag) {
        self.generation = Some(tag.generation);
    }
}
