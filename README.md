# DM-MC02 USB 调试器作业

## 完成内容

1. 配置软件 SWD 和 UART8。软件 SWD 使用 PD14、PD15，UART8 使用 PE0、PE1，收发缓冲区各 256 字节。
2. 完成 CAN 与 USB 主机的持续双向桥接。主机请求经过解析、会话校验和队列进入 CAN 发送路径；CAN 接收帧经队列主动上报主机。
3. 将 USB 接收、USB 发送、CAN 接收、CAN 发送和会话监测分别放入异步任务。保留原有 UART 转发、SWD DPIDR 读取和 DAP 发现接口。
4. 完成分包、粘包、超长输入、队列满、断连和超时处理。通过会话编号区分重连前后的数据，流模式下无需反复发送 `CAN RX`。
5. 新增 `host_bridge.py`，用于持续收帧和交互发送。主机端只保留一个串口读取入口、一个待回复请求，发送超时不自动重发。

主要修改位置：`wiring.rs` 为引脚和 UART 配置，`can_bridge.rs` 为桥接任务，`protocol.rs` 为协议解析，`bridge_state.rs`、`bridge_input.rs` 为会话和半行管理；`main.rs` 接入运行入口，`host_bridge.py` 提供主机端操作。`board.rs` 将 CAN 硬件发送改为 FIFO 模式，时钟和位时序沿用原配置。

## 验收参数

### 板卡与接口

| 项目 | 参数 |
|---|---|
| MCU / 编译目标 | STM32H723VG / `thumbv7em-none-eabihf` |
| Rust / Cargo | 1.99.1，依赖版本见 `Cargo.lock` |
| Python 依赖 | pyserial 3.5，见 `requirements-host.txt` |
| USB 标识 | VID=`0xC0DE`，PID=`0xCAFC`，产品名 `DM-MC02 CMSIS-DAP Demo` |
| USB 接口 / 包长 | 两个 CDC 串口、一个 DAP 演示接口 / 64 字节 |
| 软件 SWCLK | PD14，P16-11 |
| 软件 SWDIO | PD15，P16-9 |
| UART8 RX / TX | PE0，P16-15 / PE1，P16-17 |
| UART8 格式 | 115200 baud，8 数据位，无校验，1 停止位，无硬件流控 |
| UART8 缓冲区 | TX、RX 各 256 字节 |
| CAN1 RX / TX | PD0 / PD1 |
| CAN1 接口 | P2-1=CAN L，P2-2=CAN H，P2-3/4=GND |
| 软件接口地线 | P16-1、3、5、7 中已核对的一脚 |
| 本板硬件 SWD 接口 | P1-1=3.3 V，P1-2=GND，P1-3=SWCLK，P1-4=SWDIO |


### CAN 与缓存

| 项目 | 参数 |
|---|---|
| CAN 类型 | Classical CAN，标准 11 位数据帧，ID=`000`–`7FF`，数据长度 0–8 字节 |
| CAN 内核时钟 / 位率 | 80 MHz / 1 Mbit/s |
| 预分频 / 时间段 1 / 时间段 2 / SJW | 1 / 59 tq / 20 tq / 20 tq |
| 采样点 / 硬件发送模式 | 75% / FIFO |
| CAN 对端 / 端接 | 正常模式、1 Mbit/s；总线两端各 120 Ω，核对板上已装端接 |
| 软件 TX / RX / 回复队列 | 16 请求 / 32 帧 / 8 条 |
| 输入行 / 回复上限 | 96 字节 / 63 字节；输入上限不含 LF，CRLF 中的 CR 占一个字节 |
| CAN 提交 / USB 写入 / 回复入队等待 | 各 100 ms |
| 旧格式 CAN RX 等待 | 100 ms |
| 默认流模式 | OFF；主机完成 HELLO 后显式开启 STREAM |
| USB 发送调度 | 最多连续发送 4 条控制回复后，有接收帧时上报一帧 |


### 主机端

| 项目 | 参数 |
|---|---|
| 控制串口 | 实际枚举的控制 CDC 端口；另一个 CDC 用于 UART8 |
| 串口配置 | 115200 baud；CAN 总线位率另按上表配置 |
| 单次读取 / 写入超时 | 50 ms / 1 s |
| 命令回复超时 | 默认 3 s，可用 `--timeout` 指定 |
| 退出时停止流的等待 | 最多 0.5 s |
| 待回复请求 / 键盘输入队列 | 1 个 / 16 行 |
| 会话编号 | 每次连接随机生成的 8 位 HEX nonce，重连重新同步 |

## 验收接口与状态

消息使用 ASCII，以 LF 结束，接受 CRLF。以下以会话编号 `89ABCDEF` 为例，实际编号由主机工具生成。

| 请求或消息 | 回复或含义 |
|---|---|
| `PING` | `OK DM-MC02` |
| `SWD ID` | `OK DPIDR <8位HEX>`，需要连接实际目标 |
| `CAN HELLO 89ABCDEF` | `OK HELLO 89ABCDEF` |
| `CAN STREAM 89ABCDEF ON` / `OFF` | `OK STREAM 89ABCDEF ON` / `OFF` |
| `CAN TX 89ABCDEF 123 2 A55A` | `OK QUEUED 89ABCDEF` |
| `CAN TX 89ABCDEF 123 0 -` | 零字节数据帧 |
| `RX 89ABCDEF 321 2 A55A` | 主动上报：会话编号、ID、十进制长度、HEX 数据 |

