use embassy_stm32::{
    Peripherals, bind_interrupts,
    can::Can,
    gpio::{Flex, Level, Output, Speed},
    peripherals,
    usart::{self, BufferedUart},
};
use static_cell::StaticCell;

use crate::swd::Swd;

// 两个硬件串口分别负责发送和接收，各自使用缓冲中断驱动。
bind_interrupts!(struct UartIrqs {
    USART2 => usart::BufferedInterruptHandler<peripherals::USART2>;
    UART4 => usart::BufferedInterruptHandler<peripherals::UART4>;
});

// 保留完整驱动的所有权，使 UART4 单线接收脚不会因发送半部释放而失效。
pub struct UartPort {
    pub tx: BufferedUart<'static>,
    pub rx: BufferedUart<'static>,
}

pub struct Ports {
    pub can: Can<'static>,
    pub uart: Option<UartPort>,
}

pub fn configure(p: Peripherals) -> (crate::board::Usb, Ports, Option<Swd>) {
    let usb = crate::board::usb(p.USB_OTG_HS, p.PA12, p.PA11);

    // 软件 SWD 改用 PWM 接口的信号脚：PE13 为 SWCLK，PE9 为 SWDIO。
    // 这里按 GPIO 使用，不启用 PWM，也不连接接口中间的 5V 电源脚。
    // 本板自测：PE13 接 P1-3/PA14，PE9 接 P1-4/PA13。
    let swd = Some(Swd::new(
        Output::new(p.PE13, Level::Low, Speed::Low),
        Flex::new(p.PE9),
    ));

    // PA2 和 PA0 分别是 USART2、UART4 的 TX 复用脚，不能直接当作同一串口的 TX/RX。
    // 用 USART2 从 PA2 发送；UART4 的单线模式把接收器接到 PA0，此处只接收。
    // 每个缓冲驱动都要求 TX/RX 两块缓冲；主机实际使用 TX_BUF 和 RX_BUF。
    static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static TX_UNUSED_RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static RX_UNUSED_TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

    // 两个硬件串口使用相同的 115200、8N1；接收一个字节即可唤醒转发任务。
    let mut uart_config = usart::Config::default();
    uart_config.baudrate = 115_200;
    uart_config.data_bits = usart::DataBits::DataBits8;
    uart_config.parity = usart::Parity::ParityNone;
    uart_config.stop_bits = usart::StopBits::STOP1;
    uart_config.eager_reads = Some(1);

    // PA2 只连接接收端，采用推挽发送；不把本外设的接收数据转发给电脑。
    uart_config.tx_config = usart::OutputConfig::PushPull;
    let tx = BufferedUart::new_half_duplex(
        p.USART2,
        p.PA2, // TX: P11-2，图中第 3 行最左侧 PA02 信号脚
        UartIrqs,
        TX_BUF.init([0; 256]),
        TX_UNUSED_RX_BUF.init([0; 256]),
        uart_config,
        usart::HalfDuplexReadback::NoReadback,
    )
    .unwrap();

    // UART4 初始化后只开接收，PA0 作为单线接收脚保持释放，并启用内部上拉。
    // 主程序不使用它的发送半部，避免 PA0 与外部发送端争用。
    let mut rx_config = uart_config;
    rx_config.tx_config = usart::OutputConfig::OpenDrainPullUp;
    let rx = BufferedUart::new_half_duplex(
        p.UART4,
        p.PA0, // RX: P11-1，图中第 4 行最左侧 PA00 信号脚
        UartIrqs,
        RX_UNUSED_TX_BUF.init([0; 256]),
        RX_BUF.init([0; 256]),
        rx_config,
        usart::HalfDuplexReadback::NoReadback,
    )
    .unwrap();

    let uart = Some(UartPort { tx, rx });

    let can = crate::board::can1(p.FDCAN1, p.PD0, p.PD1);
    (usb, Ports { can, uart }, swd)
}
