use embassy_stm32::{
    Peripherals, bind_interrupts, peripherals,
    can::Can,
    gpio::{Flex, Level, Output, Speed},
    usart::{self, BufferedUart},
};
use static_cell::StaticCell;

use crate::swd::Swd;

bind_interrupts!(struct UartIrqs {
    UART8 => usart::BufferedInterruptHandler<peripherals::UART8>;
});

pub struct Ports {
    pub can: Can<'static>,
    pub uart: Option<BufferedUart<'static>>,
}

pub fn configure(p: Peripherals) -> (crate::board::Usb, Ports, Option<Swd>) {
    let usb = crate::board::usb(p.USB_OTG_HS, p.PA12, p.PA11);

    // Confirmed P16 mapping: PD14=P16-11, PD15=P16-9.
    let swd = Some(Swd::new(
        Output::new(p.PD14, Level::Low, Speed::Low),
        Flex::new(p.PD15),
    ));

    // Confirmed size: 256 bytes in each direction.
    static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

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