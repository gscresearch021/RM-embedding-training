//! 与外设无关的会话状态机。
//!
//! generation 区分不同连接/HELLO；stream_generation 区分不同订阅阶段。
//! 队列中的消息携带这两个标记，发送前再校验，过期消息就不会进入新会话。
//! 本模块只计算状态，不清理队列，也不执行 USB/CAN 操作。

/// 一条控制回复或 CAN 发送请求所属的会话。
/// nonce 为 None 时代表尚未执行 HELLO 的兼容模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTag {
    pub generation: u64,
    pub nonce: Option<u32>,
}

/// 接收帧还需要标记订阅阶段，防止 STREAM 切换后重放旧帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxTag {
    pub session: SessionTag,
    pub stream_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    Disconnected,
    Nonce,
}

/// 可以复制的状态快照，适合通过 Watch 广播给多个任务。
/// 实际共享时，调用者应在短临界区内修改状态，不能持锁等待外设。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    pub connected: bool,
    pub generation: u64,
    pub nonce: Option<u32>,
    pub streaming: bool,
    pub stream_generation: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub const fn new() -> Self {
        Self {
            connected: false,
            generation: 0,
            nonce: None,
            streaming: false,
            stream_generation: 0,
        }
    }

    /// DTR/端点连接状态真正改变时才更新代号；重复通知不会破坏当前会话。
    pub fn set_connected(&mut self, connected: bool) -> bool {
        if self.connected == connected {
            return false;
        }
        self.connected = connected;
        self.restart();
        true
    }

    /// 每次 HELLO 都开启一个新协议会话，即使主机重复使用同一个 nonce。
    pub fn hello(&mut self, nonce: u32) -> Result<SessionTag, SessionError> {
        if !self.connected {
            return Err(SessionError::Disconnected);
        }
        self.restart();
        self.nonce = Some(nonce);
        // connected 已在上面验证，因此这里一定能构造当前会话标记。
        Ok(SessionTag {
            generation: self.generation,
            nonce: self.nonce,
        })
    }

    /// 未连接时没有有效标记；已连接但未 HELLO 时可运行旧格式命令。
    pub fn tag(&self) -> Option<SessionTag> {
        if !self.connected {
            return None;
        }
        Some(SessionTag {
            generation: self.generation,
            nonce: self.nonce,
        })
    }

    /// 新格式命令必须先完成 HELLO，并带上与当前会话相同的 nonce。
    /// nonce 只是区分会话，不是密码或安全认证。
    pub fn authorize(&self, nonce: u32) -> Result<SessionTag, SessionError> {
        if !self.connected {
            return Err(SessionError::Disconnected);
        }
        if self.nonce != Some(nonce) {
            return Err(SessionError::Nonce);
        }
        Ok(SessionTag {
            generation: self.generation,
            nonce: self.nonce,
        })
    }

    /// 每次有效 STREAM 命令都让旧 RX 标记失效；不取消同会话的 TX/控制回复。
    pub fn set_stream(&mut self, nonce: u32, enabled: bool) -> Result<SessionTag, SessionError> {
        // ? 在校验失败时提前返回，因此错误 nonce 不会改变任何状态。
        let tag = self.authorize(nonce)?;
        self.stream_generation = self.stream_generation.wrapping_add(1);
        self.streaming = enabled;
        Ok(tag)
    }

    /// 只接受当前连接、当前 generation 和当前 nonce 的消息。
    pub fn accepts(&self, tag: SessionTag) -> bool {
        self.connected && self.generation == tag.generation && self.nonce == tag.nonce
    }

    /// STREAM OFF 时也可以给帧打标记，供兼容的 CAN RX 轮询命令使用。
    pub fn rx_tag(&self) -> Option<RxTag> {
        Some(RxTag {
            session: self.tag()?,
            stream_generation: self.stream_generation,
        })
    }

    pub fn accepts_rx(&self, tag: RxTag) -> bool {
        self.accepts(tag.session) && self.stream_generation == tag.stream_generation
    }

    /// 结束发生故障的协议会话；过期任务的故障不能破坏后来建立的新会话。
    /// 这里保留物理连接状态，主机可通过新的 HELLO 再同步。
    pub fn invalidate(&mut self, tag: SessionTag) -> bool {
        if !self.accepts(tag) {
            return false;
        }
        self.restart();
        true
    }

    // 会话重置：使旧队列标记失效，清 nonce 并关闭流；不撤回已交给硬件的数据。
    fn restart(&mut self) {
        // wrapping_add 明确选择计数回绕，避免整数溢出导致 panic。
        // 在代号完整回绕前，旧标记不会重合；它不是无限长或加密标识。
        self.generation = self.generation.wrapping_add(1);
        self.stream_generation = self.stream_generation.wrapping_add(1);
        self.nonce = None;
        self.streaming = false;
        // 标记失效只约束软件队列；已交给 USB FIFO/CAN 控制器的字节或帧不能撤回。
        // 新主机仍须先等待匹配 nonce 的 HELLO 回复，并丢弃此前的输入。
    }
}
