#![no_std]
#![no_main]

mod board;
mod can_bridge;
mod control;
mod dap_usb;
pub mod swd;
mod wiring;

use board::Usb;
use embassy_executor::Spawner;
use embassy_futures::{join::join4, select::select};
use embassy_usb::{
    Builder,
    class::cdc_acm::{CdcAcmClass, State},
};
use embedded_io_async::{Read, Write};
use panic_halt as _;
use static_cell::StaticCell;

const PACKET: usize = 64;
// Track linker-script changes so Cargo relinks the firmware.
const _: &[u8] = include_bytes!("../memory.x");

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let (driver, ports, swd) = wiring::configure(board::init());
    // Classroom-only test VID/PID, not allocated product identifiers.
    let mut config = embassy_usb::Config::new(0xC0DE, 0xCAFC);
    config.manufacturer = Some("HerKules");
    config.product = Some("DM-MC02 CMSIS-DAP Demo");
    config.max_packet_size_0 = PACKET as u8;
    static DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 64]> = StaticCell::new();
    static CTL_STATE: StaticCell<State> = StaticCell::new();
    static UART_STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        DESC.init([0; 256]),
        BOS.init([0; 128]),
        MSOS.init([0; 256]),
        CONTROL.init([0; 64]),
    );
    builder.msos_descriptor(0x0603_0000, 0x20);
    let dap = dap_usb::DapUsb::new(&mut builder);
    // 两个 CDC 分别用于 CAN/控制命令和 PA2/PA0 串口，扩展沿用原有 USB 接口。
    let control = CdcAcmClass::new(&mut builder, CTL_STATE.init(State::new()), PACKET as u16);
    let serial = CdcAcmClass::new(&mut builder, UART_STATE.init(State::new()), PACKET as u16);
    let mut device = builder.build();
    // 本次扩展入口：控制口交给桥接任务，原 USB、UART 和 DAP 继续运行。
    join4(
        device.run(),
        can_bridge::run(control, ports.can, swd),
        uart_port(serial, ports.uart),
        dap.run(),
    )
    .await;
}

async fn uart_port(usb: CdcAcmClass<'static, Usb>, uart: Option<wiring::UartPort>) {
    let Some(mut uart) = uart else {
        return;
    };
    // 仅取 USART2 的发送半部和 UART4 的接收半部，两个完整驱动始终保留。
    // 回传只来自 PA0 的真实接收；不把 PA2 发送外设的本地读回当作回环结果。
    let (mut serial_out, _unused_tx_receiver) = uart.tx.split_ref();
    let (_unused_rx_transmitter, mut serial_in) = uart.rx.split_ref();
    let (mut usb_out, mut usb_in) = usb.split();
    loop {
        usb_in.wait_connection().await;
        let from_host = async {
            let mut buf = [0; PACKET];
            while let Ok(n) = usb_in.read_packet(&mut buf).await {
                if serial_out.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        let to_host = async {
            // A short packet needs no zero-length terminator.
            let mut buf = [0; PACKET - 1];
            while let Ok(n) = serial_in.read(&mut buf).await {
                if n != 0 && usb_out.write_packet(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        select(from_host, to_host).await;
    }
}
