use embassy_stm32::{
    Peripherals, bind_interrupts,
    can::Can,
    gpio::{Flex, Level, Output, Speed},
    peripherals,
    usart::{self, BufferedUart},
};
use static_cell::StaticCell;

use crate::swd::Swd;

// UART8 中断绑定：由缓冲串口驱动处理接收和发送事件。
bind_interrupts!(struct UartIrqs {
    UART8 => usart::BufferedInterruptHandler<peripherals::UART8>;
});

pub struct Ports {
    pub can: Can<'static>,
    pub uart: Option<BufferedUart<'static>>,
}

pub fn configure(p: Peripherals) -> (crate::board::Usb, Ports, Option<Swd>) {
    let usb = crate::board::usb(p.USB_OTG_HS, p.PA12, p.PA11);

    // 软件 SWD：PD14 对应 P16-11，作为 SWCLK；PD15 对应 P16-9，作为 SWDIO。
    // 两根引脚按 GPIO 使用，UART8 不启用占用这两脚的 CTS/RTS。
    let swd = Some(Swd::new(
        Output::new(p.PD14, Level::Low, Speed::Low),
        Flex::new(p.PD15),
    ));

    // UART8 收发缓冲区各 256 字节，StaticCell 提供中断驱动所需的长期缓冲。
    static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

    // UART8 参数：115200 baud，8 数据位，无校验，1 停止位。
    let mut uart_config = usart::Config::default();
    uart_config.baudrate = 115_200;
    uart_config.data_bits = usart::DataBits::DataBits8;
    uart_config.parity = usart::Parity::ParityNone;
    uart_config.stop_bits = usart::StopBits::STOP1;

    let uart = Some(
        BufferedUart::new(
            p.UART8,
            p.PE0, // RX: P16-15
            p.PE1, // TX: P16-17
            TX_BUF.init([0; 256]),
            RX_BUF.init([0; 256]),
            UartIrqs,
            uart_config,
        )
        .unwrap(),
    );

    let can = crate::board::can1(p.FDCAN1, p.PD0, p.PD1);
    (usb, Ports { can, uart }, swd)
}
