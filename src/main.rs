#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]
#![allow(incomplete_features)]
#![feature(impl_trait_in_assoc_type)]
#![feature(type_alias_impl_trait)]

use assign_resources::assign_resources;
use core::panic::PanicInfo;
//use cortex_m::asm;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{self, UART0, USB};
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_rp::uart::{Config as UartConfig, InterruptHandler as UARTInterruptHandler, Uart};
use embassy_rp::usb::{Driver, InterruptHandler as USBInterruptHandler};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::{Config as UsbConfig, UsbDevice};
use embassy_usb_logger::with_class;
use static_cell::StaticCell;

bind_interrupts!(struct UartIrqs {
    UART0_IRQ => UARTInterruptHandler<UART0>;
});

bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => USBInterruptHandler<USB>;
});

assign_resources! {
    uart: UartResources{
        peripheral: UART0,
        tx: PIN_0,
        tx_dma: DMA_CH0,
        rx: PIN_1,
        rx_dma: DMA_CH1,
    }
    spi: SpiResources{
        peripheral: SPI0,
        clk: PIN_2,
        mosi: PIN_3,
        mosi_dma: DMA_CH2,
        miso: PIN_4,
        miso_dma: DMA_CH3,
        cs: PIN_5,
        led: PIN_25,
    }
}

// According to Serial Flasher Protocol Specification - version 1
const S_ACK: u8 = 0x06;
const S_NAK: u8 = 0x15;

enum SerprogCommand {
    Nop = 0x00,        // No operation
    QIface = 0x01,     // Query interface version
    QCmdMap = 0x02,    // Query supported commands bitmap
    QPgmName = 0x03,   // Query programmer name
    QSerBuf = 0x04,    // Query Serial Buffer Size
    QBustype = 0x05,   // Query supported bustypes
    QChipSize = 0x06,  // Query supported chipsize (2^n format)
    QOpBuf = 0x07,     // Query operation buffer size
    QWrNMaxLen = 0x08, // Query Write to opbuf: Write-N maximum length
    RByte = 0x09,      // Read a single byte
    RNBytes = 0x0A,    // Read n bytes
    OInit = 0x0B,      // Initialize operation buffer
    OWriteB = 0x0C,    // Write opbuf: Write byte with address
    OWriteN = 0x0D,    // Write to opbuf: Write-N
    ODelay = 0x0E,     // Write opbuf: udelay
    OExec = 0x0F,      // Execute operation buffer
    SyncNop = 0x10,    // Special no-operation that returns NAK+ACK
    QRdNMaxLen = 0x11, // Query read-n maximum length
    SBustype = 0x12,   // Set used bustype(s)
    OSpiOp = 0x13,     // Perform SPI operation
    SSpiFreq = 0x14,   // Set SPI clock frequency
    SPinState = 0x15,  // Enable/disable output drivers
    SSpiCs = 0x16,     // Select Chip Select to use
}

impl From<u8> for SerprogCommand {
    fn from(value: u8) -> Self {
        match value {
            0x00 => SerprogCommand::Nop,
            0x01 => SerprogCommand::QIface,
            0x02 => SerprogCommand::QCmdMap,
            0x03 => SerprogCommand::QPgmName,
            0x04 => SerprogCommand::QSerBuf,
            0x05 => SerprogCommand::QBustype,
            0x06 => SerprogCommand::QChipSize,
            0x07 => SerprogCommand::QOpBuf,
            0x08 => SerprogCommand::QWrNMaxLen,
            0x09 => SerprogCommand::RByte,
            0x0A => SerprogCommand::RNBytes,
            0x0B => SerprogCommand::OInit,
            0x0C => SerprogCommand::OWriteB,
            0x0D => SerprogCommand::OWriteN,
            0x0E => SerprogCommand::ODelay,
            0x0F => SerprogCommand::OExec,
            0x10 => SerprogCommand::SyncNop,
            0x11 => SerprogCommand::QRdNMaxLen,
            0x12 => SerprogCommand::SBustype,
            0x13 => SerprogCommand::OSpiOp,
            0x14 => SerprogCommand::SSpiFreq,
            0x15 => SerprogCommand::SPinState,
            0x16 => SerprogCommand::SSpiCs,
            _ => SerprogCommand::Nop,
        }
    }
}

const CMDMAP: u32 = (1 << SerprogCommand::Nop as u32)
    | (1 << SerprogCommand::QIface as u32)
    | (1 << SerprogCommand::QCmdMap as u32)
    | (1 << SerprogCommand::QPgmName as u32)
    | (1 << SerprogCommand::QSerBuf as u32)
    | (1 << SerprogCommand::QWrNMaxLen as u32)
    | (1 << SerprogCommand::QBustype as u32)
    | (1 << SerprogCommand::SyncNop as u32)
    | (1 << SerprogCommand::QRdNMaxLen as u32)
    | (1 << SerprogCommand::OSpiOp as u32)
    | (1 << SerprogCommand::SBustype as u32)
    | (1 << SerprogCommand::SSpiFreq as u32)
    | (1 << SerprogCommand::SPinState as u32);

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let r = split_resources!(p);
    let driver = Driver::new(p.USB, UsbIrqs);

    let config = {
        let mut config = UsbConfig::new(0x1ced, 0xc0fe);
        config.manufacturer = Some("9elements");
        config.product = Some("Picoprog");
        config.serial_number = Some("OSFC2024");
        config.max_power = 100;
        config.max_packet_size_0 = 64;

        // Required for windows compatibility.
        // https://developer.nordicsemi.com/nRF_Connect_SDK/doc/1.9.1/kconfig/CONFIG_CDC_ACM_IAD.html#help
        config.device_class = 0xEF;
        config.device_sub_class = 0x02;
        config.device_protocol = 0x01;
        config.composite_with_iads = true;
        config
    };

    let mut builder = {
        static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

        let builder = embassy_usb::Builder::new(
            driver,
            config,
            CONFIG_DESCRIPTOR.init([0; 256]),
            BOS_DESCRIPTOR.init([0; 256]),
            &mut [], // no msos descriptors
            CONTROL_BUF.init([0; 64]),
        );
        builder
    };

    let logger_class = {
        static STATE: StaticCell<State> = StaticCell::new();
        let state = STATE.init(State::new());
        CdcAcmClass::new(&mut builder, state, 64)
    };

    let uart_class = {
        static STATE: StaticCell<State> = StaticCell::new();
        let state = STATE.init(State::new());
        CdcAcmClass::new(&mut builder, state, 64)
    };

    let serprog_class = {
        static STATE: StaticCell<State> = StaticCell::new();
        let state = STATE.init(State::new());
        CdcAcmClass::new(&mut builder, state, 64)
    };

    let usb = builder.build();
    // We can't really recover here so just unwrap
    spawner.spawn(usb_task(usb)).unwrap();
    spawner.spawn(logger_task(logger_class)).unwrap();
    spawner.spawn(uart_task(uart_class, r.uart)).unwrap();
    spawner.spawn(serprog_task(serprog_class, r.spi)).unwrap();

    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
}

type CustomUsbDriver = Driver<'static, USB>;
type CustomUsbDevice = UsbDevice<'static, CustomUsbDriver>;

#[embassy_executor::task]
async fn usb_task(mut usb: CustomUsbDevice) -> ! {
    usb.run().await
}

#[embassy_executor::task]
async fn logger_task(class: CdcAcmClass<'static, CustomUsbDriver>) {
    with_class!(1024, log::LevelFilter::Info, class).await
}

#[embassy_executor::task]
async fn uart_task(class: CdcAcmClass<'static, CustomUsbDriver>, r: UartResources) {
    let config = UartConfig::default(); // TODO: make this configurable

    // TODO: Buffers are weird...
    let mut usb_tx_buf = [0; 64];
    let mut usb_rx_buf = [0; 64];
    let mut uart_tx_buf = [0; 1];
    let mut uart_rx_buf = [0; 1];

    let uart = Uart::new(
        r.peripheral,
        r.tx,
        r.rx,
        UartIrqs,
        r.tx_dma,
        r.rx_dma,
        config,
    );
    let (mut tx, mut rx) = uart.split();
    let (mut sender, mut receiver) = class.split();

    let tx_future = async {
        loop {
            receiver.wait_connection().await;
            if let Err(e) = receiver.read_packet(&mut usb_tx_buf).await {
                log::error!("Error reading packet: {:?}", e);
                continue;
            }
            let len = usb_tx_buf.iter().position(|&x| x == 0).unwrap_or(64);
            let mut offset = 0;
            while offset < len {
                let chunk_len = (len - offset).min(1);
                if let Some(slice) = usb_tx_buf.get(offset..offset + chunk_len) {
                    uart_tx_buf[..chunk_len].copy_from_slice(slice);
                    log::debug!(
                        "USB to UART - USB buffer (len: {}): {:?}, UART buffer (len: {}): {:?}",
                        slice.len(),
                        slice,
                        chunk_len,
                        &uart_tx_buf[..chunk_len]
                    );
                    if let Err(e) = tx.write(&uart_tx_buf[..chunk_len]).await {
                        log::error!("Error writing to UART: {:?}", e);
                        break;
                    }
                }
                offset += chunk_len;
            }
        }
    };

    let rx_future = async {
        loop {
            sender.wait_connection().await;
            if let Err(e) = rx.read(&mut uart_rx_buf).await {
                log::error!("Error reading from UART: {:?}", e);
                continue;
            }
            let len = uart_rx_buf.iter().position(|&x| x == 0).unwrap_or(1);
            if let Some(slice) = uart_rx_buf.get(..len) {
                usb_rx_buf[..len].copy_from_slice(slice);
                log::debug!(
                    "UART to USB - UART buffer (len: {}): {:?}, USB buffer (len: {}): {:?}",
                    slice.len(),
                    slice,
                    len,
                    &usb_rx_buf[..len]
                );
                if let Err(e) = sender.write_packet(&usb_rx_buf[..len]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
        }
    };

    join(tx_future, rx_future).await;
}

#[embassy_executor::task]
async fn serprog_task(mut class: CdcAcmClass<'static, CustomUsbDriver>, r: SpiResources) -> ! {
    let mut config = SpiConfig::default(); // TODO: make this configurable
    config.frequency = 12_000_000; // 12 MHz

    let mut spi = Spi::new(
        r.peripheral,
        r.clk,
        r.mosi,
        r.miso,
        r.mosi_dma,
        r.miso_dma,
        config,
    );
    let mut cs = Output::new(r.cs, Level::High);
    let mut led = Output::new(r.led, Level::Low);
    let mut buf = [0; 64];

    const MAX_BUFFER_SIZE: usize = 2048;

    loop {
        class.wait_connection().await;
        if let Err(e) = class.read_packet(&mut buf).await {
            log::error!("Error reading packet: {:?}", e);
            continue;
        }
        match SerprogCommand::from(buf[0]) {
            SerprogCommand::Nop => {
                log::debug!("Received Nop CMD");
                if let Err(e) = class.write_packet(&[S_ACK]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QIface => {
                log::debug!("Received QIface CMD");
                if let Err(e) = class.write_packet(&[S_ACK, 0x01, 0x00]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QCmdMap => {
                log::debug!("Received QCmdMap CMD");
                let cmdmap_bytes = CMDMAP.to_le_bytes();
                let mut packet = [0; 33];
                packet[0] = S_ACK;
                packet[1] = cmdmap_bytes[0];
                packet[2] = cmdmap_bytes[1];
                packet[3] = cmdmap_bytes[2];
                packet[4] = cmdmap_bytes[3];
                if let Err(e) = class.write_packet(&packet).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QPgmName => {
                log::debug!("Received QPgmName CMD");
                if let Err(e) = class
                    .write_packet(&[
                        S_ACK, b'P', b'i', b'c', b'o', b'p', b'r', b'o', b'g', 0, 0, 0, 0, 0, 0, 0,
                        0,
                    ])
                    .await
                {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QSerBuf => {
                log::debug!("Received QSerBuf CMD");
                if let Err(e) = class.write_packet(&[S_ACK, 0xFF, 0xFF]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QWrNMaxLen => {
                log::debug!("Received QRdNMaxLen CMD");
                let size_24 = MAX_BUFFER_SIZE.to_le_bytes();

                if let Err(e) = class
                    .write_packet(&[S_ACK, size_24[0], size_24[1], size_24[2]])
                    .await
                {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QBustype => {
                log::debug!("Received QBustype CMD");
                if let Err(e) = class.write_packet(&[S_ACK, 0x08]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::SyncNop => {
                log::debug!("Received SyncNop CMD");
                if let Err(e) = class.write_packet(&[S_NAK, S_ACK]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::QRdNMaxLen => {
                log::debug!("Received QRdNMaxLen CMD");
                let size_24 = MAX_BUFFER_SIZE.to_le_bytes();

                if let Err(e) = class
                    .write_packet(&[S_ACK, size_24[0], size_24[1], size_24[2]])
                    .await
                {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::SBustype => {
                log::debug!("Received SBustype CMD");
                if let Err(e) = class.read_packet(&mut buf).await {
                    log::error!("Error reading packet: {:?}", e);
                    continue;
                }
                if buf[0] == 0x08 {
                    log::debug!("Received SBustype 'SPI'");
                    if let Err(e) = class.write_packet(&[S_ACK]).await {
                        log::error!("Error writing packet: {:?}", e);
                    }
                } else {
                    log::debug!("Received unknown SBustype");
                    if let Err(e) = class.write_packet(&[S_NAK]).await {
                        log::error!("Error writing packet: {:?}", e);
                    }
                }
            }
            SerprogCommand::OSpiOp => {
                log::debug!("Received OSpiOp CMD");
                if let Err(e) = class.read_packet(&mut buf).await {
                    log::error!("Error reading packet: {:?}", e);
                    continue;
                }
                let op_slen = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]);
                let op_rlen = u32::from_le_bytes([buf[3], buf[4], buf[5], 0]);

                let mut sdata = [0_u8; MAX_BUFFER_SIZE];
                let mut rdata = [0_u8; MAX_BUFFER_SIZE];

                // Copy initial chunk from buf starting at byte position 6 (sdata)
                let initial_chunk_size = core::cmp::min(64 - 6, op_slen as usize);
                sdata[..initial_chunk_size].copy_from_slice(&buf[6..6 + initial_chunk_size]);
                let mut bytes_read = initial_chunk_size;

                // Read the remaining sdata in chunks
                while bytes_read < op_slen as usize {
                    let chunk_size = core::cmp::min(64, op_slen as usize - bytes_read);
                    if let Err(e) = class.read_packet(&mut buf[..chunk_size]).await {
                        log::error!("Error reading packet: {:?}", e);
                        continue;
                    }
                    sdata[bytes_read..bytes_read + chunk_size].copy_from_slice(&buf[..chunk_size]);
                    bytes_read += chunk_size;
                }

                log::debug!(
                    "Starting SPI transfer, sdata: {:?}, rdata: {:?}",
                    &sdata[..op_slen as usize],
                    &rdata[..op_rlen as usize]
                );

                // This call is blocking according to the SPI HAL
                if let Err(e) = spi.flush() {
                    log::error!("Error flushing SPI: {:?}", e);
                }

                cs.set_low();
                match spi.write(&sdata[..op_slen as usize]).await {
                    Ok(_) => {
                        log::debug!("SPI write successful");
                        log::debug!("Sent data (sdata): {:?}", &sdata[..op_slen as usize]);
                        match spi.read(&mut rdata[..op_rlen as usize]).await {
                            Ok(_) => {
                                log::debug!("SPI read successful");
                                log::debug!(
                                    "Received data (rdata): {:?}",
                                    &rdata[..op_rlen as usize]
                                );
                                if let Err(e) = class.write_packet(&[S_ACK]).await {
                                    log::error!("Error writing packet: {:?}", e);
                                }
                                // Send the full rdata in chunks
                                let mut bytes_written = 0;
                                while bytes_written < op_rlen as usize {
                                    let chunk_size =
                                        core::cmp::min(64, op_rlen as usize - bytes_written);
                                    if let Err(e) = class
                                        .write_packet(
                                            &rdata[bytes_written..bytes_written + chunk_size],
                                        )
                                        .await
                                    {
                                        log::error!("Error writing rdata: {:?}", e);
                                    }
                                    bytes_written += chunk_size;
                                }
                            }
                            Err(e) => {
                                log::error!("SPI read error: {:?}", e);
                                if let Err(e) = class.write_packet(&[S_NAK]).await {
                                    log::error!("Error writing NAK: {:?}", e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("SPI write error: {:?}", e);
                        if let Err(e) = class.write_packet(&[S_NAK]).await {
                            log::error!("Error writing NAK: {:?}", e);
                        }
                    }
                }
                cs.set_high();
            }
            SerprogCommand::SSpiFreq => {
                log::debug!("Received SSpiFreq CMD");
                if let Err(e) = class.read_packet(&mut buf).await {
                    log::error!("Error reading packet: {:?}", e);
                    continue;
                }
                log::debug!("Received SSpiFreq packet: {:?}", buf);
                let try_freq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                log::debug!("Setting SPI frequency: {:?}", try_freq);
                spi.set_frequency(try_freq);
                let actual_freq = try_freq.to_le_bytes(); // TODO can we report what the hardware has set up?
                if let Err(e) = class
                    .write_packet(&[
                        S_ACK,
                        actual_freq[0],
                        actual_freq[1],
                        actual_freq[2],
                        actual_freq[3],
                    ])
                    .await
                {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            SerprogCommand::SPinState => {
                // This command should set SPI pins to active but we just use it to toggle the LED
                log::debug!("Received SPinState CMD");
                if let Err(e) = class.read_packet(&mut buf).await {
                    log::error!("Error reading packet: {:?}", e);
                    continue;
                }
                if buf[0] == 0 {
                    led.set_low();
                } else {
                    led.set_high();
                }
                if let Err(e) = class.write_packet(&[S_ACK]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
            _ => {
                log::debug!("Received unknown CMD");
                if let Err(e) = class.write_packet(&[S_NAK]).await {
                    log::error!("Error writing packet: {:?}", e);
                }
            }
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Print out the panic info
    log::error!("Panic occurred: {:?}", info);

    // Reboot the system
    cortex_m::peripheral::SCB::sys_reset();
}
