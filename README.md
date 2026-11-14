# DM-MC02 USB 调试器与 CAN 桥接

基于 STM32H723VG，提供 USB↔CAN 持续双向桥接、UART8 串口转发、GPIO SWD DPIDR 读取和 CMSIS-DAP 演示接口。CAN 使用标准 11 位数据帧、0–8 字节、1 Mbit/s。

固件已完成目标编译和链接；硬件通信尚未上板验证。CMSIS-DAP 接口只支持设备发现，不能代替外置探针烧录或执行完整调试。

## 文件

| 文件 | 用途 |
|---|---|
| `src/` | 固件源码；`main.rs` 启动 USB、桥接、UART 与 DAP |
| `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` | 构建配置与固定依赖版本 |
| `.cargo/config.toml`、`memory.x` | STM32 编译目标、烧录命令和内存布局 |
| `host_bridge.py` | 持续 CAN 收发工具 |
| `host.py` | 基础控制、SWD、CAN 轮询和 UART 回环入口 |
| `requirements-host.txt` | Python 运行依赖 |

## 编译与烧录

电脑需要 Rust/rustup、Python 3.10 或以上；Windows 还需 MSVC C++ 生成工具和 Windows SDK。Rust 工具链固定为 1.99.1，目标为 `thumbv7em-none-eabihf`，由工具链文件指定。

在仓库根目录执行：

```powershell
cargo build --locked
```

生成的 ELF 为 `target/thumbv7em-none-eabihf/debug/dm-mc02-cmsis-dap`。

首次烧录使用外置 SWD 探针，板卡另行正确供电；移除软件 SWD 回接跳线后，外置探针接 P1：SWCLK→3、SWDIO→4、GND→2。P1-1 的 3.3 V 根据探针的 VTref/供电定义连接，不并接未知电源输出。

```powershell
cargo install probe-rs-tools --locked
probe-rs list
cargo run --locked
```

`cargo run` 会通过项目 runner 烧录并启动 STM32，需有实际硬件。选择外置探针，不能选本板的 `DM-MC02 CMSIS-DAP Demo`。如果连接多个探针，用实际外置探针的 VID:PID 和序列号选择参数消除歧义后再烧录。烧录完成后 Ctrl+C 退出探针会话，板上程序继续运行。

## 接线

以下为厂家 V1.0 原理图对应脚号，接线前核对实物版本、连接器方向和线序。

| 用途 | MCU 引脚 | 板卡连接器 |
|---|---|---|
| CAN1 RX/TX | PD0 / PD1 | P2-1=CAN L，P2-2=CAN H，P2-3/4=GND |
| 软件 SWCLK | PD14 | P16-11 → 目标 SWCLK |
| 软件 SWDIO | PD15 | P16-9 → 目标 SWDIO |
| UART8 RX | PE0 | P16-15 |
| UART8 TX | PE1 | P16-17 |
| 软件接口公共地 | GND | P16-1/3/5/7 中已核对的一脚 |

软件 SWD 使用 3.3 V 逻辑，两板正确供电并共地。UART8 为 115200、8N1，无硬件流控，收发缓冲各 256 字节；回环仅短接 P16-15/17。

CAN 对端使用正常模式、Classical CAN、1 Mbit/s，并共地。总线两个物理端点各放一个 120 Ω 端接，核对本板和对端已装电阻，避免重复端接。P16-10/12 是 CAN3，不用于本方案的 CAN1。板卡需由 USB 或规定电源供电，外置探针不代表 CAN 收发器已完整供电。

## 主机运行

用已安装的 Python 建立环境并安装依赖；已有 `.venv` 可跳过创建。若 `python` 不在 PATH，使用解释器的完整路径替换它。

```powershell
python -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements-host.txt
.\.venv\Scripts\python.exe host_bridge.py --list-ports
```

板卡有两个 CDC 串口。用 PING 找控制口，另一个口为 UART8。下面的 COM7、COM8 均需替换为实际端口；同一个端口只能由一个程序占用。

```powershell
.\.venv\Scripts\python.exe host.py COM7 ping
.\.venv\Scripts\python.exe host_bridge.py COM7
```

控制口预期回复 `OK DM-MC02`。桥接工具随后自动完成 HELLO 和 STREAM ON，持续显示 CAN 帧，不需要逐帧轮询。

| 交互输入 | 动作 |
|---|---|
| `123 A55A` | 发送 ID=123、两字节数据 A5 5A |
| `123 -` | 发送零字节帧 |
| `ping` | 基础控制 |
| `swd` | 读取已接目标的 SWD DPIDR |
| `stream off` / `stream on` | 停止/开启主动接收 |
| `quit` | 关闭流和端口 |

ID 按十六进制，不带 `0x`，范围 000–7FF；数据每字节两个 HEX 字符。只接收 30 秒：

```powershell
.\.venv\Scripts\python.exe host_bridge.py COM7 --receive-only --duration 30
```

退出桥接工具、释放控制口后，可单独运行原有入口：

```powershell
.\.venv\Scripts\python.exe host.py COM7 swd
.\.venv\Scripts\python.exe host.py COM7 can-send 123 A55A
.\.venv\Scripts\python.exe host.py COM7 can-recv
.\.venv\Scripts\python.exe host.py COM8 uart-loopback
```

SWD 命令需要接好目标，UART 回环命令需要短接对应 TX/RX。

## 线上协议与运行边界

消息为 ASCII，以 LF 结束，也接受 CRLF。新连接先发送一个 LF 清理残留半行，再以新生成的 8 位 HEX nonce 同步；收到同 nonce 的 HELLO 回复后才处理当前会话数据。

```text
CAN HELLO 89ABCDEF                  → OK HELLO 89ABCDEF
CAN STREAM 89ABCDEF ON              → OK STREAM 89ABCDEF ON
CAN TX 89ABCDEF 123 2 A55A           → OK QUEUED 89ABCDEF
CAN TX 89ABCDEF 123 0 -              → OK QUEUED 89ABCDEF
CAN STREAM 89ABCDEF OFF             → OK STREAM 89ABCDEF OFF
```

设备主动上报格式为 `RX 89ABCDEF 321 2 A55A`，依次是会话编号、ID、十进制长度和数据。nonce 用于区分会话，不是认证。旧 `CAN TX <ID> <HEX>` 和 `CAN RX` 仅在 STREAM OFF 时可用；流模式下返回 `ERR STREAMING`。

软件 TX/RX/回复队列容量为 16/32/8，单行输入 96 字节，回复最多 63 字节。TX 队列满返回 `ERR BUSY`；RX 队列满丢新帧并计数；CAN 提交、USB 写和回复入队各受 100 ms 限制。CAN 硬件发送使用 FIFO 模式，避免优先级模式同步取消旧帧的等待。

非法字段返回 `ERR SYNTAX`、`ERR FRAME`、`ERR SESSION` 或 `ERR ENCODING`，超长行丢到下一 LF 后返回 `ERR LINE_TOO_LONG`。控制器提交超时返回 `ERR CAN TX`。`OK QUEUED` 只表示已提交控制器，不保证总线 ACK 或对端收到。

DTR 关闭、端点错误、USB 写入或回复入队超时会废弃旧会话，清理未完成行和软件队列。主机超时或出错后关闭端口，重新运行工具生成新 nonce，不自动重发 TX。已交给 USB FIFO 或 CAN 控制器的数据不能撤回；旧 nonce 的消息由主机丢弃。旧脚本的兼容格式不提供同样的会话隔离保证。

## 来源

- [课程模板](https://github.com/HKUAstar/embedded-training-template)：保留板级初始化、UART、SWD DPIDR 和 DAP 发现基础，新增持续桥接、会话管理和主机工具。
- [DM-MC02 V1.0 原理图](https://github.com/dmBots/DM-MC02/blob/master/drawings/2d/schematics/CtrBoard-H7_V1.0-240124.pdf)：接线依据。
- Embassy STM32/USB 0.6.0、sync 0.8.0：包元数据声明 `MIT OR Apache-2.0`；pyserial 3.5：BSD。版本以 Cargo.lock 和 requirements-host.txt 为准。
