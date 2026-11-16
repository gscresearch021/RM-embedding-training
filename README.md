# DM-MC02 USB 调试器作业

## SWD GPIO 配置

在 `src/wiring.rs` 中，选用 PWM 接口的 PE13 作为 SWCLK，以初始低电平的 `Output` 输出时钟；PE9 作为 SWDIO，通过 `Flex` 切换输入和输出。两根引脚均按 3.3 V GPIO 使用，并交给 `Swd::new` 完成 DPIDR 读取。

## UART 配置

串口使用 PWM 接口的 PA2 和 PA0。USART2 从 PA2 发送，UART4 通过单线模式从 PA0 接收，参数为 115200 baud、8 数据位、无校验、1 停止位。

在 `src/wiring.rs` 中绑定 USART2、UART4 的缓冲串口中断，并通过 `StaticCell` 分配四块各 256 字节的静态缓冲区。主机发送使用 USART2 的 TX 缓冲，主机接收使用 UART4 的 RX 缓冲。`UartPort` 保存两个驱动，`src/main.rs` 使用 `split_ref` 取得对应的发送和接收半部，完成 USB 与串口之间的字节转发。

SWD 使用 PE13、PE9，串口使用 PA2、PA0，四路信号分别占用独立的 MCU 引脚。

## 引脚表

| 信号 | MCU 引脚 | 接插件信号脚 | 本次实测接法 |
|---|---|---|---|
| SWCLK | PE13 | P11-4，PWM 第 1 行信号脚 | 接本板 P1-3 / PA14 / SWCLK |
| SWDIO | PE9 | P11-3，PWM 第 2 行信号脚 | 接本板 P1-4 / PA13 / SWDIO |
| UART TX | PA2 | P11-2，PWM 第 3 行信号脚 | 接 P11-1 / PA0，形成回环 |
| UART RX | PA0 | P11-1，PWM 第 4 行信号脚 | 接 P11-2 / PA2，形成回环 |

PWM 行序按管脚图从上往下排列，均使用信号列。SWD 采用同板自回接，UART 用一根杜邦线短接 PA2 与 PA0。接插件脚号依据 [DM-MC02 V1.0 原理图](https://github.com/dmBots/DM-MC02/blob/master/drawings/2d/schematics/CtrBoard-H7_V1.0-240124.pdf)。

## 实测记录

板卡为 DM-MC02，MCU 为 STM32H723VG，编译目标为 `thumbv7em-none-eabihf`。使用 Horco CMSIS-DAP 外置探针烧录。

| 项目 | 执行命令 | 实际结果 |
|---|---|---|
| 编译 | `cargo build --locked` | `Finished dev profile [optimized + debuginfo]` |
| 烧录 | `cargo run --locked` | 最近一次 Erasing、Programming 均达到 100%；Programming 为 135.00 KiB，`Finished in 11.37s` |
| 外置探针识别 | `probe-rs list` | `Horco CMSIS-DAP -- ef1a:74e5-0:7844331832 (CMSIS-DAP)` |
| 板上演示探针发现 | `probe-rs list` | `DM-MC02 CMSIS-DAP Demo -- c0de:cafc-0: (CMSIS-DAP)` |
| 最后一次串口枚举 | `.\.venv\Scripts\python.exe host_bridge.py --list-ports` | `COM5: USB 串行设备 (COM5)`；`COM6: USB 串行设备 (COM6)` |

SWD 自测记录使用控制口 COM7，连续两次读取 DPIDR，随后执行 PING：

```text
.\.venv\Scripts\python.exe host.py COM7 swd
OK DPIDR 6BA02477
.\.venv\Scripts\python.exe host.py COM7 swd
OK DPIDR 6BA02477
.\.venv\Scripts\python.exe host.py COM7 ping
OK DM-MC02
```

最后一次烧录并重新连接后，控制口为 COM5，UART 口为 COM6，实际结果为：

```text
.\.venv\Scripts\python.exe host.py COM5 ping
OK DM-MC02
.\.venv\Scripts\python.exe host.py COM6 uart-loopback
OK UART loopback
```

## 用实测确认的配置

将 PA2 与 PA0 短接后，`host.py` 发送 `HerKules UART\n`，并逐字节核对返回的 14 字节，结果为 `OK UART loopback`。这项实测确认了 USART2/PA2 发送、UART4/PA0 单线接收的组合配置和回环接线能够完成串口收发。
