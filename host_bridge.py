#!/usr/bin/env python3
"""DM-MC02 CAN/USB 持续桥接工具；无需逐帧发送 CAN RX。"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import math
import queue
import re
import secrets
import sys
import threading
import time
from collections.abc import Callable
from typing import Protocol


MAX_LINE_BYTES = 96
READ_CHUNK_BYTES = 64
INPUT_QUEUE_SIZE = 16
HEX = re.compile(r"[0-9a-fA-F]+\Z", re.ASCII)
NONCE = re.compile(r"[0-9a-fA-F]{8}\Z", re.ASCII)
DECIMAL = re.compile(r"[0-9]+\Z", re.ASCII)


class BridgeError(Exception):
    """本次会话无法继续；调用者应关闭端口，重新连接时重新生成 nonce。"""


class SerialPort(Protocol):
    """只规定用到的方法，真实串口和离线测试的假串口都可以满足这个接口。"""

    def read(self, size: int) -> bytes: ...
    def write(self, data: bytes) -> int: ...
    def close(self) -> None: ...


@dataclass(frozen=True)
class CanFrame:
    identifier: int
    data: bytes


@dataclass(frozen=True)
class Message:
    kind: str
    session: str | None = None
    frame: CanFrame | None = None
    enabled: bool | None = None
    text: str = ""


@dataclass(frozen=True)
class LineError:
    reason: str


class LineDecoder:
    """逐字节组行；半行跨 read 保留，超长后丢弃到 LF，不会无限增长。"""

    def __init__(self) -> None:
        self._buffer = bytearray()
        self._discarding = False

    def reset(self) -> None:
        self._buffer.clear()
        self._discarding = False

    def feed(self, data: bytes) -> list[str | LineError]:
        events: list[str | LineError] = []
        for byte in data:
            if byte == 10:  # ASCII LF，只有它才结束一条消息。
                if self._discarding:
                    events.append(LineError("收到超长行，已丢弃到 LF"))
                else:
                    raw = bytes(self._buffer)
                    if raw.endswith(b"\r"):
                        raw = raw[:-1]  # 只移除 CRLF 中紧挨 LF 的 CR。
                    try:
                        events.append(raw.decode("ascii"))
                    except UnicodeDecodeError:
                        events.append(LineError("收到非 ASCII 行，已丢弃"))
                self.reset()
            elif self._discarding:
                continue
            elif len(self._buffer) == MAX_LINE_BYTES:
                self._discarding = True
            else:
                self._buffer.append(byte)
        return events


def parse_frame(identifier: str, length: str, payload: str) -> CanFrame:
    """校验标准 11 位 ID、十进制长度和 HEX，禁止用截断掩盖长度不符。"""
    if not HEX.fullmatch(identifier) or int(identifier, 16) > 0x7FF:
        raise ValueError("CAN ID 必须是 000～7FF 的十六进制数，不带 0x")
    if not DECIMAL.fullmatch(length) or int(length) > 8:
        raise ValueError("Classical CAN 数据长度必须为 0～8")
    count = int(length)
    if count == 0:
        if payload != "-":
            raise ValueError("零字节数据必须写为 -")
        data = b""
    else:
        if len(payload) != count * 2 or not HEX.fullmatch(payload):
            raise ValueError("HEX 字符数必须恰好等于数据长度的两倍")
        data = bytes.fromhex(payload)
    return CanFrame(int(identifier, 16), data)


def parse_message(line: str) -> Message | None:
    """把设备消息转换成有类型的值；未知消息不当作任意命令的成功回复。"""
    if not line.isascii() or any(ord(char) < 32 or ord(char) == 127 for char in line):
        raise ValueError("消息中含控制字符或非 ASCII 字符")
    words = line.split()
    if words[:1] == ["ERR"]:
        return Message("error", text=line)
    if len(words) == 3 and words[:2] == ["OK", "HELLO"] and NONCE.fullmatch(words[2]):
        return Message("hello", session=words[2].upper(), text=line)
    if len(words) == 4 and words[:2] == ["OK", "STREAM"] and NONCE.fullmatch(words[2]):
        if words[3] not in ("ON", "OFF"):
            raise ValueError("STREAM 回复必须包含 ON 或 OFF")
        return Message("stream", session=words[2].upper(), enabled=words[3] == "ON", text=line)
    if len(words) == 3 and words[:2] == ["OK", "QUEUED"] and NONCE.fullmatch(words[2]):
        return Message("queued", session=words[2].upper(), text=line)
    if len(words) == 5 and words[0] == "RX" and NONCE.fullmatch(words[1]):
        return Message("rx", session=words[1].upper(), frame=parse_frame(*words[2:]), text=line)
    if words == ["OK", "DM-MC02"]:
        return Message("pong", text=line)
    if len(words) == 3 and words[:2] == ["OK", "DPIDR"] and NONCE.fullmatch(words[2]):
        return Message("swd", text=line)
    return None


@dataclass
class Pending:
    kind: str
    deadline: float
    enabled: bool | None = None


# 主机端：命令回复和主动 RX 共用一个读取入口，键盘线程不访问串口。
class BridgeClient:
    """串口只在 poll_once 中读取；所有命令最多保留一个待回复请求。"""

    def __init__(
        self,
        port: SerialPort,
        *,
        session: str | None = None,
        timeout: float = 3.0,
        on_frame: Callable[[CanFrame], None] | None = None,
        on_notice: Callable[[str], None] | None = None,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        value = secrets.token_hex(4) if session is None else session
        if not NONCE.fullmatch(value):
            raise ValueError("session 必须是 8 位 HEX")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout 必须是正的有限秒数")
        self.port = port
        self.session = value.upper()
        self.timeout = timeout
        self.on_frame = on_frame or (lambda frame: None)
        self.on_notice = on_notice or (lambda text: None)
        self.clock = clock
        self.decoder = LineDecoder()
        self.synchronized = False
        self.streaming = False
        self.failed = False
        self.pending: Pending | None = None
        self._reply: Message | None = None

    def _fail(self, reason: str) -> None:
        self.failed = True
        self.synchronized = False
        self.pending = None
        raise BridgeError(reason)

    def _check_timeout(self) -> None:
        if self.pending is not None and self.clock() >= self.pending.deadline:
            self._fail("等待设备回复超时；结果可能不确定，本工具不会自动重发，请重新连接")

    def _accept(self, message: Message) -> None:
        if not self.synchronized:
            # HELLO 是新旧会话的边界；之前的帧、错误和旧 nonce 回复全部忽略。
            if message.kind != "hello" or message.session != self.session:
                return
            if self.pending is None or self.pending.kind != "hello":
                return
            self.synchronized = True
        elif message.session is not None and message.session != self.session:
            return

        if message.kind == "rx":
            if self.streaming and message.frame is not None:
                self.on_frame(message.frame)
            return
        if message.kind == "error":
            # ERR 没有 nonce，只有同步后的当前请求可以接收它；失败后结束会话。
            if self.pending is not None:
                self._fail(f"设备拒绝请求：{message.text}；会话已结束，不自动重发")
            return
        if self.pending is None or message.kind != self.pending.kind:
            return
        if message.kind == "stream" and message.enabled != self.pending.enabled:
            return
        if message.kind == "stream":
            self.streaming = bool(message.enabled)
        self._reply = message
        self.pending = None

    def poll_once(self) -> None:
        if self.failed:
            raise BridgeError("会话已经结束，请关闭端口后重新连接")
        self._check_timeout()
        try:
            # 唯一的串口读取位置。pyserial 的 read timeout 将单次等待限制在 50 ms。
            data = self.port.read(READ_CHUNK_BYTES)
        except OSError as error:
            self._fail(f"串口读取失败：{error}")
        self._check_timeout()
        for event in self.decoder.feed(data):
            if isinstance(event, LineError):
                self.on_notice(event.reason)
                continue
            try:
                message = parse_message(event)
            except ValueError as error:
                self.on_notice(f"忽略格式无效的消息：{error}")
                continue
            if message is not None:
                self._accept(message)

    # 请求处理：只允许一个待回复命令，等待期间继续接收 RX；超时不重发 TX。
    def _request(self, command: str, kind: str, *, enabled: bool | None = None,
                 timeout: float | None = None, prefix: bytes = b"") -> Message:
        if self.failed:
            raise BridgeError("会话已经结束，请重新连接")
        if self.pending is not None:
            raise BridgeError("已有一个请求等待回复，不能同时发出第二个请求")
        if kind != "hello" and not self.synchronized:
            raise BridgeError("必须先完成 HELLO 同步")
        self._reply = None
        wait = self.timeout if timeout is None else timeout
        self.pending = Pending(kind, self.clock() + wait, enabled)
        raw = prefix + (command + "\n").encode("ascii")
        try:
            written = self.port.write(raw)
        except OSError as error:
            self._fail(f"串口写入失败：{error}；不自动重发")
        if written != len(raw):
            self._fail("串口只写入部分请求，发送结果不确定；不自动重发")
        while self._reply is None:
            self.poll_once()
        return self._reply

    def connect(self) -> Message:
        if self.synchronized:
            raise BridgeError("当前连接已经同步；重连必须建立新的客户端和 nonce")
        self.decoder.reset()
        # 前导 LF 结束设备可能残留的半行，随后用新 nonce 建立明确同步点。
        return self._request(f"CAN HELLO {self.session}", "hello", prefix=b"\n")

    def stream(self, enabled: bool, *, timeout: float | None = None) -> Message:
        mode = "ON" if enabled else "OFF"
        return self._request(f"CAN STREAM {self.session} {mode}", "stream",
                             enabled=enabled, timeout=timeout)

    def send_can(self, identifier: str, payload: str) -> Message:
        # 交互输入省略长度；这里根据 HEX 计算，再由共同的校验函数检查完整性。
        count = 0 if payload == "-" else len(payload) // 2
        frame = parse_frame(identifier, str(count), payload)
        encoded = frame.data.hex().upper() if frame.data else "-"
        return self._request(
            f"CAN TX {self.session} {frame.identifier:03X} {len(frame.data)} {encoded}",
            "queued",
        )

    def ping(self) -> Message:
        return self._request("PING", "pong")

    def swd(self) -> Message:
        return self._request("SWD ID", "swd")

    def close(self) -> None:
        try:
            if self.synchronized and not self.failed and self.streaming:
                # 关闭前尝试停止流，但等待有界；无法得到回复也要继续关闭端口。
                try:
                    self.stream(False, timeout=0.5)
                except BridgeError as error:
                    self.on_notice(str(error))
        finally:
            try:
                if hasattr(self.port, "dtr"):
                    self.port.dtr = False
            except OSError as error:
                self.on_notice(f"关闭 DTR 失败：{error}")
            finally:
                self.port.close()
                self.synchronized = False
                self.streaming = False
                self.decoder.reset()


def print_frame(frame: CanFrame) -> None:
    payload = frame.data.hex().upper() if frame.data else "-"
    print(f"RX ID={frame.identifier:03X} LEN={len(frame.data)} DATA={payload}", flush=True)


def read_input(commands: queue.Queue[str], finished: threading.Event) -> None:
    """输入线程只处理键盘；不会读取或写入串口，也不会阻止主程序退出。"""
    try:
        while True:
            try:
                line = input("> ")
            except EOFError:
                return
            try:
                commands.put(line, timeout=0.1)
            except queue.Full:
                print("输入队列已满，本行未发送；请等待当前请求完成。", file=sys.stderr)
    finally:
        finished.set()


def run(client: BridgeClient, *, receive_only: bool, duration: float | None) -> None:
    print(client.connect().text, flush=True)
    print(client.stream(True).text, flush=True)
    print("已开启持续接收。OK QUEUED 仅表示已提交控制器，不代表总线 ACK。", flush=True)
    commands: queue.Queue[str] = queue.Queue(maxsize=INPUT_QUEUE_SIZE)
    finished = threading.Event()
    if not receive_only:
        print("输入：123 A55A、123 -、ping、swd、stream on、stream off 或 quit。", flush=True)
        threading.Thread(target=read_input, args=(commands, finished), daemon=True).start()
    deadline = None if duration is None else time.monotonic() + duration
    while deadline is None or time.monotonic() < deadline:
        if not receive_only:
            try:
                line = commands.get_nowait().strip()
            except queue.Empty:
                if finished.is_set():
                    return
            else:
                words = line.split()
                if words in (["quit"], ["exit"]):
                    return
                if not words:
                    continue
                try:
                    if words == ["ping"]:
                        reply = client.ping()
                    elif words == ["swd"]:
                        reply = client.swd()
                    elif words in (["stream", "on"], ["stream", "off"]):
                        reply = client.stream(words[1] == "on")
                    elif len(words) == 2:
                        reply = client.send_can(*words)
                    else:
                        raise ValueError("输入格式：ID HEX，或 ping/swd/stream on/stream off/quit")
                    print(reply.text, flush=True)
                except ValueError as error:
                    # 本地输入不合法时尚未发出请求，可以修改后重输。
                    print(f"输入未发送：{error}", file=sys.stderr, flush=True)
        client.poll_once()


def main(argv: list[str] | None = None) -> int:
    # Windows 管道/日志也统一输出 UTF-8，避免中文帮助信息被按系统代码页编码。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", nargs="?", help="控制 CDC 串口，例如 COM7（另一个 CDC 是 UART）")
    parser.add_argument("--list-ports", action="store_true", help="列出电脑当前串口")
    parser.add_argument("--receive-only", action="store_true", help="只持续接收，不启动键盘输入线程")
    parser.add_argument("--duration", type=float, help="完成同步并开启流后运行的秒数；省略时持续运行")
    parser.add_argument("--timeout", type=float, default=3.0, help="每条命令的回复超时秒数，默认 3")
    args = parser.parse_args(argv)
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout 必须是正的有限秒数")
    if args.duration is not None and (not math.isfinite(args.duration) or args.duration < 0):
        parser.error("--duration 必须是非负的有限秒数")
    if not args.list_ports and not args.port:
        parser.error("请提供控制串口，或使用 --list-ports")
    # 延迟导入 pyserial：--help 和纯协议测试不需要安装串口包。
    try:
        import serial
        from serial.tools import list_ports
    except ImportError:
        print("缺少 pyserial：请使用项目 .venv，并安装 requirements-host.txt。", file=sys.stderr)
        return 2
    if args.list_ports:
        ports = list(list_ports.comports())
        if not ports:
            print("未发现串口；当前无硬件时属于预期情况。")
        for port in ports:
            print(f"{port.device}: {port.description}")
        return 0
    client = None
    try:
        # 默认 DTR=True；设备据此识别主机已打开控制口。读写都采用有界超时。
        port = serial.Serial(args.port, 115200, timeout=0.05, write_timeout=1.0)
        client = BridgeClient(port, timeout=args.timeout, on_frame=print_frame,
                              on_notice=lambda text: print(text, file=sys.stderr, flush=True))
        run(client, receive_only=args.receive_only, duration=args.duration)
        return 0
    except KeyboardInterrupt:
        print("\n正在关闭桥接会话。", flush=True)
        return 0
    except (OSError, BridgeError) as error:
        print(f"桥接停止：{error}", file=sys.stderr, flush=True)
        return 1
    finally:
        if client is not None:
            try:
                client.close()
            except OSError as error:
                print(f"关闭串口失败：{error}", file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
