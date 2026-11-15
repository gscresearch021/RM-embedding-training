//! 持续双向桥接：每个外设方向有唯一拥有者，任务之间只传递拥有自身数据的消息。

use core::cell::RefCell;

use dm_mc02_cmsis_dap::{
    bridge_input::SessionLine,
    bridge_state::{RxTag, Session, SessionError, SessionTag},
    protocol::{CanFrame, Command, Reply},
};
use embassy_futures::{
    join::{join, join4},
    select::{Either, select, select3},
    yield_now,
};
use embassy_stm32::can::{Can, CanRx, CanTx, Frame};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    channel::Channel,
    watch::{Receiver as WatchReceiver, Watch},
};
use embassy_time::{Duration, Instant, Timer, with_deadline, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender};
use embedded_can::{Frame as _, Id};
use static_cell::StaticCell;

use crate::{PACKET, board::Usb, control, swd::Swd};

// 缓存配置：分别限制待发请求、接收帧和控制回复，防止主机阻塞时无限占用内存。
const TX_CAPACITY: usize = 16;
const RX_CAPACITY: usize = 32;
const REPLY_CAPACITY: usize = 8;
const WAIT: Duration = Duration::from_millis(100);
const REPLY_BURST: usize = 4;

// 三个任务会等待会话变化；每个任务都有自己的 Watch 接收者。
type Changes<'a> = WatchReceiver<'a, CriticalSectionRawMutex, Session, 3>;

#[derive(Clone, Copy)]
struct TxRequest {
    tag: SessionTag,
    frame: CanFrame,
    // None 保留旧 OK QUEUED 格式；Some 则在回复中带 nonce。
    reply_session: Option<u32>,
}

struct QueuedReply {
    tag: SessionTag,
    text: control::Response,
}

#[derive(Clone, Copy)]
struct ReceivedFrame {
    tag: RxTag,
    frame: CanFrame,
}

/// 计数保存在固定内存中，可用调试器观察；不把丢帧写成成功。
#[derive(Default)]
struct Statistics {
    rx_received: u32,
    rx_dropped: u32,
    rx_unsupported: u32,
    rx_errors: u32,
    tx_submitted: u32,
    tx_full: u32,
    tx_stale: u32,
    tx_timeouts: u32,
    tx_replaced: u32,
    reply_timeouts: u32,
    usb_write_errors: u32,
    usb_write_timeouts: u32,
}

struct State {
    session: Session,
    stats: Statistics,
}

struct Shared {
    state: Mutex<CriticalSectionRawMutex, RefCell<State>>,
    changes: Watch<CriticalSectionRawMutex, Session, 3>,
    tx: Channel<CriticalSectionRawMutex, TxRequest, TX_CAPACITY>,
    rx: Channel<CriticalSectionRawMutex, ReceivedFrame, RX_CAPACITY>,
    replies: Channel<CriticalSectionRawMutex, QueuedReply, REPLY_CAPACITY>,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State {
                session: Session::new(),
                stats: Statistics::default(),
            })),
            changes: Watch::new_with(Session::new()),
            tx: Channel::new(),
            rx: Channel::new(),
            replies: Channel::new(),
        }
    }

    fn state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        // 临界区只执行短内存操作。这里没有 await，也不等待 USB/CAN。
        self.state.lock(|state| f(&mut state.borrow_mut()))
    }

    fn session(&self) -> Session {
        self.state(|state| state.session)
    }

    fn clear_rx(&self) {
        let discarded = self.rx.len() as u32;
        self.rx.clear();
        self.state(|state| {
            state.stats.rx_dropped = state.stats.rx_dropped.saturating_add(discarded);
        });
    }

    fn clear_queues(&self) {
        let discarded = self.tx.len() as u32;
        self.tx.clear();
        self.replies.clear();
        self.clear_rx();
        self.state(|state| {
            state.stats.tx_stale = state.stats.tx_stale.saturating_add(discarded);
        });
    }

    fn publish(&self) {
        // 本模块的更新、清队列和发布之间没有 await，均在同一执行器上顺序完成。
        // 中断不调用这些方法，因此不会出现旧快照覆盖新快照。
        self.changes.sender().send(self.session());
    }

    fn set_connected(&self, connected: bool) {
        if self.state(|state| state.session.set_connected(connected)) {
            self.clear_queues();
            self.publish();
        }
    }

    fn hello(&self, nonce: u32) -> Result<SessionTag, SessionError> {
        let tag = self.state(|state| state.session.hello(nonce))?;
        self.clear_queues();
        self.publish();
        Ok(tag)
    }

    fn set_stream(&self, nonce: u32, enabled: bool) -> Result<SessionTag, SessionError> {
        let tag = self.state(|state| state.session.set_stream(nonce, enabled))?;
        // 每次订阅/取消订阅都废弃已有 RX，不能把旧模式的缓存改贴新标签。
        self.clear_rx();
        self.publish();
        Ok(tag)
    }

    fn invalidate(&self, tag: SessionTag) {
        if self.state(|state| state.session.invalidate(tag)) {
            self.clear_queues();
            self.publish();
        }
    }

    async fn reply(&self, tag: SessionTag, text: control::Response) {
        if !self.session().accepts(tag) {
            return;
        }
        if with_timeout(WAIT, self.replies.send(QueuedReply { tag, text }))
            .await
            .is_err()
        {
            self.state(|state| {
                state.stats.reply_timeouts = state.stats.reply_timeouts.saturating_add(1);
            });
            // 回复队列阻塞超过上限：结束协议会话，之后必须重新 HELLO 同步。
            self.invalidate(tag);
        }
    }

    async fn enqueue_tx(&self, request: TxRequest) {
        if self.tx.try_send(request).is_err() {
            self.state(|state| {
                state.stats.tx_full = state.stats.tx_full.saturating_add(1);
            });
            self.reply(request.tag, control::fixed("ERR BUSY\n")).await;
        }
    }
}

/// 桥接入口：将 CAN 和 USB 按收发方向拆开，再运行五个异步任务。
/// 每个外设方向只有一个拥有者，任务间通过队列传递数据。
pub async fn run(usb: CdcAcmClass<'static, Usb>, can: Can<'static>, swd: Option<Swd>) {
    static SHARED: StaticCell<Shared> = StaticCell::new();
    let shared = SHARED.init(Shared::new());
    let (usb_tx, usb_rx, control_changed) = usb.split_with_control();
    let (can_tx, can_rx, _properties) = can.split();
    let rx_changes = shared.changes.receiver().expect("three Watch receivers");
    let tx_changes = shared.changes.receiver().expect("three Watch receivers");
    let can_changes = shared.changes.receiver().expect("three Watch receivers");

    join(
        session_task(control_changed, shared),
        join4(
            host_rx_task(usb_rx, swd, shared, rx_changes),
            host_tx_task(usb_tx, shared, tx_changes),
            can_tx_task(can_tx, shared, can_changes),
            can_rx_task(can_rx, shared),
        ),
    )
    .await;
}

// 会话监测：DTR 变化后更新连接状态，通知等待中的收发任务。
async fn session_task(control: ControlChanged<'static>, shared: &Shared) {
    shared.set_connected(control.dtr());
    loop {
        control.control_changed().await;
        // DTR 表示主机应用打开了控制串口。每个任务通过 Watch 收到状态变化。
        shared.set_connected(control.dtr());
    }
}

// 主机接收：组行、解析和校验会话，CAN 请求入队后由发送任务处理。
async fn host_rx_task(
    mut usb: Receiver<'static, Usb>,
    mut swd: Option<Swd>,
    shared: &Shared,
    mut changes: Changes<'_>,
) {
    let mut packet = [0; PACKET];
    let mut line = SessionLine::new();
    loop {
        usb.wait_connection().await;
        shared.set_connected(usb.dtr());
        line.reset();
        loop {
            yield_now().await;
            let before = changes.get().await;
            // get 会把通知标为已读，所以每次也要核对缓冲所属的实际 generation。
            line.observe(before);
            match select(changes.changed(), usb.read_packet(&mut packet)).await {
                Either::First(after) => {
                    line.observe(after);
                }
                Either::Second(Err(_)) => {
                    shared.set_connected(false);
                    line.reset();
                    break;
                }
                Either::Second(Ok(n)) => {
                    let Some(mut tag) = before.tag() else {
                        line.reset();
                        continue;
                    };
                    for &byte in &packet[..n] {
                        let current = shared.session();
                        if !usb.dtr() || !current.accepts(tag) {
                            line.reset();
                            break;
                        }
                        let Some(command) = line.push(current, byte) else {
                            continue;
                        };
                        // 先完成纯解析得到 owned 命令，再 await，避免借用行缓冲跨任务传递。
                        let expected = match command {
                            Ok(command) => {
                                dispatch(command, tag, &mut swd, shared, &mut changes).await
                            }
                            Err(error) => {
                                shared.reply(tag, control::error_reply(error)).await;
                                Some(tag)
                            }
                        };
                        let Some(expected) = expected else {
                            line.reset();
                            break;
                        };
                        if !shared.session().accepts(expected) {
                            line.reset();
                            break;
                        }
                        // HELLO 可在本包中创建新 generation；后续完整命令仍可继续解析。
                        tag = expected;
                        line.adopt(expected);
                    }
                }
            }
        }
    }
}

// 命令分派：HELLO 建立会话，STREAM 切换上报模式，TX 入队，基础命令保留。
async fn dispatch(
    command: Command,
    tag: SessionTag,
    swd: &mut Option<Swd>,
    shared: &Shared,
    changes: &mut Changes<'_>,
) -> Option<SessionTag> {
    match command {
        Command::Ping => shared.reply(tag, control::encode(Reply::Pong)).await,
        Command::SwdId => shared.reply(tag, control::swd_id(swd)).await,
        Command::Hello { session } => {
            let fresh = shared.hello(session).ok()?;
            shared
                .reply(fresh, control::encode(Reply::Hello(session)))
                .await;
            return Some(fresh);
        }
        Command::Stream { session, enabled } => {
            let text = match shared.set_stream(session, enabled) {
                Ok(_) => control::encode(Reply::Stream { session, enabled }),
                Err(_) => control::fixed("ERR SESSION\n"),
            };
            shared.reply(tag, text).await;
        }
        Command::CanTx(request) => {
            let text = match shared.session().authorize(request.session) {
                Ok(_) => match request.frame() {
                    Ok(frame) => {
                        shared
                            .enqueue_tx(TxRequest {
                                tag,
                                frame,
                                reply_session: Some(request.session),
                            })
                            .await;
                        return Some(tag);
                    }
                    Err(error) => control::error_reply(error),
                },
                Err(_) => control::fixed("ERR SESSION\n"),
            };
            shared.reply(tag, text).await;
        }
        Command::LegacyCanTx(frame) => {
            if shared.session().streaming {
                shared.reply(tag, control::fixed("ERR STREAMING\n")).await;
            } else {
                shared
                    .enqueue_tx(TxRequest {
                        tag,
                        frame,
                        reply_session: None,
                    })
                    .await;
            }
        }
        Command::CanRx => {
            if shared.session().streaming {
                shared.reply(tag, control::fixed("ERR STREAMING\n")).await;
            } else {
                let response = poll_one_frame(tag, shared, changes).await?;
                shared.reply(tag, response).await;
            }
        }
    }
    Some(tag)
}

// 兼容旧 CAN RX：只在流关闭时从软件队列取帧，不另开硬件接收者。
async fn poll_one_frame(
    tag: SessionTag,
    shared: &Shared,
    changes: &mut Changes<'_>,
) -> Option<control::Response> {
    let deadline = Instant::now() + WAIT;
    let errors_before = shared.state(|state| state.stats.rx_errors);
    loop {
        let current = changes.get().await;
        if !current.accepts(tag) {
            return None;
        }
        match select(
            changes.changed(),
            with_deadline(deadline, shared.rx.receive()),
        )
        .await
        {
            Either::First(_) => continue,
            Either::Second(Ok(frame)) => {
                if shared.session().accepts_rx(frame.tag) {
                    return Some(control::encode(Reply::Received {
                        session: None,
                        frame: &frame.frame,
                    }));
                }
            }
            Either::Second(Err(_)) => {
                return Some(
                    if shared.state(|state| state.stats.rx_errors) != errors_before {
                        control::fixed("ERR CAN RX\n")
                    } else {
                        control::encode(Reply::Empty)
                    },
                );
            }
        }
    }
}

// CAN 发送：提交前再次检查会话，提交成功后才生成 OK QUEUED。
async fn can_tx_task(mut can: CanTx<'static>, shared: &Shared, mut changes: Changes<'_>) {
    loop {
        yield_now().await;
        let request = shared.tx.receive().await;
        let payload = match request.frame.payload() {
            Ok(payload) => payload,
            Err(error) => {
                shared.reply(request.tag, control::error_reply(error)).await;
                continue;
            }
        };
        let frame = match Frame::new_standard(request.frame.id, payload) {
            Ok(frame) => frame,
            Err(_) => {
                shared
                    .reply(request.tag, control::fixed("ERR FRAME\n"))
                    .await;
                continue;
            }
        };
        let deadline = Instant::now() + WAIT;
        loop {
            let current = changes.get().await;
            if !current.accepts(request.tag) {
                shared.state(|state| state.stats.tx_stale = state.stats.tx_stale.saturating_add(1));
                break;
            }
            match select(
                changes.changed(),
                with_deadline(deadline, can.write(&frame)),
            )
            .await
            {
                // 状态变化会取消尚未提交的 write；同会话仅切换流模式时可在原期限内继续等。
                Either::First(_) => continue,
                Either::Second(result) => {
                    let text = match result {
                        Ok(None) => {
                            shared.state(|state| {
                                state.stats.tx_submitted =
                                    state.stats.tx_submitted.saturating_add(1)
                            });
                            control::encode(Reply::Queued {
                                session: request.reply_session,
                            })
                        }
                        Ok(Some(_replaced)) => {
                            // FIFO 模式正常不会替换；仍记录异常，不能静默忽略丢失的旧帧。
                            shared.state(|state| {
                                state.stats.tx_replaced = state.stats.tx_replaced.saturating_add(1)
                            });
                            control::fixed("ERR CAN REPLACED\n")
                        }
                        Err(_) => {
                            shared.state(|state| {
                                state.stats.tx_timeouts = state.stats.tx_timeouts.saturating_add(1)
                            });
                            control::fixed("ERR CAN TX\n")
                        }
                    };
                    shared.reply(request.tag, text).await;
                    break;
                }
            }
        }
    }
}

// CAN 接收：持续排空硬件接收通道，只接收标准 Classical CAN 数据帧。
async fn can_rx_task(mut can: CanRx<'static>, shared: &Shared) {
    loop {
        // 即使硬件一直有数据，也显式让其他任务获得一次调度机会。
        yield_now().await;
        let envelope = match can.read().await {
            Ok(envelope) => envelope,
            Err(_) => {
                shared
                    .state(|state| state.stats.rx_errors = state.stats.rx_errors.saturating_add(1));
                // 持续总线错误也要让出执行器，不能在立即返回的错误上忙循环。
                Timer::after_millis(1).await;
                continue;
            }
        };
        let frame = envelope.frame;
        let Id::Standard(id) = *frame.id() else {
            shared.state(|state| {
                state.stats.rx_unsupported = state.stats.rx_unsupported.saturating_add(1)
            });
            continue;
        };
        if frame.is_remote_frame() || frame.header().fdcan() {
            shared.state(|state| {
                state.stats.rx_unsupported = state.stats.rx_unsupported.saturating_add(1)
            });
            continue;
        }
        let frame = match CanFrame::new(id.as_raw(), frame.data()) {
            Ok(frame) => frame,
            Err(_) => {
                shared.state(|state| {
                    state.stats.rx_unsupported = state.stats.rx_unsupported.saturating_add(1)
                });
                continue;
            }
        };
        shared.state(|state| state.stats.rx_received = state.stats.rx_received.saturating_add(1));
        let Some(tag) = shared.session().rx_tag() else {
            shared.state(|state| state.stats.rx_dropped = state.stats.rx_dropped.saturating_add(1));
            continue;
        };
        if shared.rx.try_send(ReceivedFrame { tag, frame }).is_err() {
            // RX 满时保留已有帧，丢弃新帧并计数；继续排空硬件接收通道。
            shared.state(|state| state.stats.rx_dropped = state.stats.rx_dropped.saturating_add(1));
        }
    }
}

enum Output {
    Reply(QueuedReply),
    Frame(ReceivedFrame),
}

fn next_output(shared: &Shared, streaming: bool, burst: &mut usize) -> Option<Output> {
    // && let 把布尔条件和模式匹配接在一起：条件成立且队列有帧时才进入。
    if streaming
        && *burst >= REPLY_BURST
        && let Ok(frame) = shared.rx.try_receive()
    {
        *burst = 0;
        return Some(Output::Frame(frame));
    }
    if let Ok(reply) = shared.replies.try_receive() {
        *burst = burst.saturating_add(1);
        return Some(Output::Reply(reply));
    }
    if streaming && let Ok(frame) = shared.rx.try_receive() {
        *burst = 0;
        return Some(Output::Frame(frame));
    }
    None
}

// USB 发送：统一输出控制回复和主动 RX，写入前检查会话和订阅代号。
async fn host_tx_task(mut usb: Sender<'static, Usb>, shared: &Shared, mut changes: Changes<'_>) {
    let mut burst = 0;
    let mut phase = None;
    loop {
        yield_now().await;
        let current = changes.get().await;
        let current_phase = (current.generation, current.stream_generation);
        if phase != Some(current_phase) {
            // 新连接/新订阅先发控制确认，不继承上一次的连续回复计数。
            burst = 0;
            phase = Some(current_phase);
        }
        if !current.connected {
            changes.changed().await;
            continue;
        }
        let Some(output) = next_output(shared, current.streaming, &mut burst) else {
            if current.streaming {
                // 只等待“就绪”而不取走消息，避免 select 的另一分支获胜时丢掉队列数据。
                select3(
                    changes.changed(),
                    shared.replies.ready_to_receive(),
                    shared.rx.ready_to_receive(),
                )
                .await;
            } else {
                select(changes.changed(), shared.replies.ready_to_receive()).await;
            }
            continue;
        };
        let (tag, rx_tag, text) = match output {
            Output::Reply(reply) => (reply.tag, None, reply.text),
            Output::Frame(frame) => (
                frame.tag.session,
                Some(frame.tag),
                control::encode(Reply::Received {
                    session: frame.tag.session.nonce,
                    frame: &frame.frame,
                }),
            ),
        };
        let deadline = Instant::now() + WAIT;
        loop {
            let current = changes.get().await;
            if !current.accepts(tag)
                || rx_tag.is_some_and(|tag| !current.streaming || !current.accepts_rx(tag))
            {
                break;
            }
            match select(
                changes.changed(),
                with_deadline(deadline, usb.write_packet(text.as_bytes())),
            )
            .await
            {
                Either::First(_) => continue,
                Either::Second(Ok(Ok(()))) => break,
                Either::Second(Ok(Err(_))) => {
                    shared.state(|state| {
                        state.stats.usb_write_errors =
                            state.stats.usb_write_errors.saturating_add(1)
                    });
                    shared.invalidate(tag);
                    break;
                }
                Either::Second(Err(_)) => {
                    shared.state(|state| {
                        state.stats.usb_write_timeouts =
                            state.stats.usb_write_timeouts.saturating_add(1)
                    });
                    shared.invalidate(tag);
                    break;
                }
            }
        }
    }
}
