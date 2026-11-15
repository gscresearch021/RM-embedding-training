//! 基础控制和短回复编码；CAN 收发统一交给 can_bridge 中的任务。

use dm_mc02_cmsis_dap::protocol::{MAX_REPLY_LEN, ParseError, Reply};
use heapless::String;

use crate::swd::Swd;

pub type Response = String<MAX_REPLY_LEN>;

pub fn fixed(text: &str) -> Response {
    Response::try_from(text).expect("fixed reply fits one short USB packet")
}

pub fn encode(reply: Reply<'_>) -> Response {
    let mut out = Response::new();
    if reply.write_to(&mut out).is_err() {
        // 编码失败时丢弃整条临时缓冲，不能发送半条回复。
        return fixed("ERR FRAME\n");
    }
    out
}

pub fn error_reply(error: ParseError) -> Response {
    encode(Reply::Error(error))
}

pub fn swd_id(swd: &mut Option<Swd>) -> Response {
    let Some(swd) = swd.as_mut() else {
        return fixed("ERR SWD_NOT_CONFIGURED\n");
    };
    match swd.read_dpidr() {
        Some(id) => encode(Reply::SwdId(id)),
        None => fixed("ERR SWD\n"),
    }
}
