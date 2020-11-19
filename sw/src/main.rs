#![no_main]
#![no_std]

// note: to get vscode to reload file, do shift-ctrl-p, 'reload window'. developer:Reload window

use core::panic::PanicInfo;
use riscv_rt::entry;

extern crate betrusted_hal;
extern crate utralib;
extern crate volatile;

use betrusted_hal::hal_hardi2c::*;
use betrusted_hal::hal_time::*;
use betrusted_hal::api_gasgauge::*;
use betrusted_hal::api_lm3509::*;
use betrusted_hal::api_bq25618::*;
use betrusted_hal::api_tusb320::*;

extern crate wfx_sys;
extern crate wfx_rs;
extern crate wfx_bindings;

extern crate xous_nommu;
use wfx_rs::hal_wf200::wfx_init;
use wfx_rs::hal_wf200::wfx_scan_ongoing;
use wfx_rs::hal_wf200::wfx_start_scan;
use wfx_rs::hal_wf200::wfx_handle_event;
use wfx_rs::hal_wf200::wf200_mutex_get;
use wfx_rs::hal_wf200::wf200_ssid_get_list;
use wfx_rs::hal_wf200::wf200_ssid_updated;
use wfx_bindings::*;

use gyro_rs::hal_gyro::BtGyro;

use utralib::generated::*;
use volatile::Volatile;

#[macro_use]
mod debug;

mod spi;
use spi::*;
mod comstates;
use comstates::*;


const CONFIG_CLOCK_FREQUENCY: u32 = 18_000_000;

// allocate a global, unsafe static string for debug output
#[used]  // This is necessary to keep DBGSTR from being optimized out
static mut DBGSTR: [u32; 4] = [0, 0, 0, 0];

#[panic_handler]
fn panic(_panic: &PanicInfo<'_>) -> ! {
    loop {}
}

fn ticktimer_int_handler(_irq_no: usize) {
    let mut ticktimer_csr = CSR::new(HW_TICKTIMER_BASE as *mut u32);
    let mut crg_csr = CSR::new(HW_CRG_BASE as *mut u32);
    let mut power_csr = CSR::new(HW_POWER_BASE as *mut u32);

    // disarm the watchdog
    crg_csr.wfo(utra::crg::WATCHDOG_RESET_CODE, 0x600d);
    crg_csr.wfo(utra::crg::WATCHDOG_RESET_CODE, 0xc0de);

    // fast-monitor the keyboard wakeup inputs if the soc is in the off state
    if power_csr.rf(utra::power::POWER_SOC_ON) == 0 {
        // drive sense for keyboard
        let power =
        power_csr.ms(utra::power::POWER_SELF, 1)
        | power_csr.ms(utra::power::POWER_DISCHARGE, 1)
        | power_csr.ms(utra::power::POWER_KBDDRIVE, 1);
        power_csr.wo(utra::power::POWER, power);

        if power_csr.rf(utra::power::STATS_MONKEY) == 3 { // both keys have to be hit
            sprintln!("detect power up event!");
            // power on the SOC
            let power =
            power_csr.ms(utra::power::POWER_SELF, 1)
            | power_csr.ms(utra::power::POWER_SOC_ON, 1);
            power_csr.wo(utra::power::POWER, power);
        } else {
            // re-engage discharge fets, disable keyboard drive
            let power =
            power_csr.ms(utra::power::POWER_SELF, 1)
            | power_csr.ms(utra::power::POWER_KBDDRIVE, 0)
            | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
            power_csr.wo(utra::power::POWER, power);
        }
    }

    set_msleep_target_ticks(50); // resetting this will also clear the alarm

    ticktimer_csr.wfo(utra::ticktimer::EV_PENDING_ALARM, 1);
}

fn com_int_handler(_irq_no: usize) {
    let mut com_csr = CSR::new(HW_COM_BASE as *mut u32);
    // nop handler, here just to wake up the CPU in case of an incoming SPI packet and run the normal loop
    com_csr.wfo(utra::com::EV_PENDING_SPI_AVAIL, 1);
}

#[allow(dead_code)]  // used for debugging
fn dump_rom_addr(addr: u32) {
    let rom_ptr: *mut u32 = (addr + HW_SPIFLASH_MEM as u32) as *mut u32;
    let rom = rom_ptr as *mut Volatile<u32>;
    for i in 0..64 {
        if i % 8 == 0 {
            sprint!("\n\r0x{:06x}: ", addr + i * 4);
        }
        let data: u32 = unsafe{(*rom.add(i as usize)).read()};
        sprint!("{:02x} {:02x} {:02x} {:02x} ", data & 0xFF, (data >> 8) & 0xff, (data >> 16) & 0xff, (data >> 24) & 0xff);
    }
    sprintln!("");
}

fn com_tx(tx: u16) {
    let com_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_fifo = com_ptr as *mut Volatile<u32>;

    unsafe{ (*com_fifo).write(tx as u32); }
}

fn com_rx(timeout: u32) -> Result<u16, &'static str> {
    let com_csr = CSR::new(HW_COM_BASE as *mut u32);
    let com_rd_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_rd = com_rd_ptr as *mut Volatile<u32>;

    if timeout != 0 && (com_csr.rf(utra::com::STATUS_RX_AVAIL) != 0) {
        let start = get_time_ms();
        loop {
            if com_csr.rf(utra::com::STATUS_RX_AVAIL) == 1 {
                break;
            } else if start + timeout < get_time_ms() {
                return Err("timeout")
            }
        }
    }
    Ok(unsafe{ (*com_rd).read() as u16 })
}

#[entry]
fn main() -> ! {
    let mut power_csr = CSR::new(HW_POWER_BASE as *mut u32);
    let mut com_csr = CSR::new(HW_COM_BASE as *mut u32);
    let mut crg_csr = CSR::new(HW_CRG_BASE as *mut u32);
    let mut ticktimer_csr = CSR::new(HW_TICKTIMER_BASE as *mut u32);
    let mut wifi_csr = CSR::new(HW_WIFI_BASE as *mut u32);

    let com_rd_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_rd = com_rd_ptr as *mut Volatile<u32>;

    // Initialize the no-MMU version of Xous, which will give us
    // basic access to tasks and interrupts.
    xous_nommu::init();

    let mut i2c = Hardi2c::new();

    time_init();

    i2c.i2c_init(CONFIG_CLOCK_FREQUENCY);

    let mut charger: BtCharger = BtCharger::new();

    // this needs to be one of the first things called after I2C comes up
    charger.chg_set_safety(&mut i2c);

    gg_start(&mut i2c);

    charger.chg_set_autoparams(&mut i2c);
    charger.chg_start(&mut i2c);

    let mut usb_cc = BtUsbCc::new();
    usb_cc.init(&mut i2c);

    let mut gyro: BtGyro = BtGyro::new();
    gyro.init();

    let mut last_run_time : u32 = get_time_ms();
    let mut loopcounter: u32 = 0; // in seconds, so this will last ~125 years
    let mut voltage : i16 = 0;
    let mut current: i16 = 0;
    let mut stby_current: i16 = 0;
    let mut pd_loop_timer: u32 = 0;
    let mut backlight : BtBacklight = BtBacklight::new();
    let mut com_sentinel: u16 = 0;

    backlight.set_brightness(&mut i2c, 0, 0); // make sure the backlight is off on boot

    let mut start_time: u32 = get_time_ms();
    let mut wifi_ready: bool = false;

    charger.update_regs(&mut i2c);

    let mut usb_cc_event = false;

    let use_wifi: bool = true;

/*  // quick test routine for SPI flashing
    let mut idcode: [u8; 3] = [0; 3];
    spi_cmd(CMD_RDID, None, Some(&mut idcode));
    sprintln!("SPI ID code: {:02x} {:02x} {:02x}", idcode[0], idcode[1], idcode[2]);
    let test_addr = 0x8_0000;
    dump_rom_addr(test_addr);
    spi_erase_region(test_addr, 4096);

    dump_rom_addr(test_addr);

    let mut test_data: [u8; 256] = [0; 256];
    for i in 0..256 {
        test_data[i] = (255 - i) as u8;
    }
    spi_program_page(test_addr, &mut test_data);

    dump_rom_addr(test_addr);
*/
    spi_standby();

    xous_nommu::syscalls::sys_interrupt_claim(utra::ticktimer::TICKTIMER_IRQ, ticktimer_int_handler).unwrap();
    set_msleep_target_ticks(50);
    ticktimer_csr.wfo(utra::ticktimer::EV_PENDING_ALARM, 1); // clear the pending signal just in case
    ticktimer_csr.wfo(utra::ticktimer::EV_ENABLE_ALARM, 1); // enable the interrupt

    /////// NOTE TO SELF: if using GDB, must disable the watchdog!!!
    crg_csr.wfo(utra::crg::WATCHDOG_ENABLE, 1); // enable the watchdog reset

    xous_nommu::syscalls::sys_interrupt_claim(utra::com::COM_IRQ, com_int_handler).unwrap();
    com_csr.wfo(utra::com::EV_ENABLE_SPI_AVAIL, 1);

    let mut flash_update_lock = false;
    loop {
        if !flash_update_lock {
            //////////////////////// WIFI HANDLER BLOCK ---------
            if !use_wifi && (get_time_ms() - start_time > 1500) {
                delay_ms(250); // force just a delay, so requests queue up
                start_time = get_time_ms();
            }
            // slight delay to allow for wishbone-tool to connect for debuggening
            if (get_time_ms() - start_time > 1500) && !wifi_ready && use_wifi {
                sprintln!("initializing wifi!");
                // delay_ms(250); // let the message print
                // init the wifi interface
                if wfx_init() == SL_STATUS_OK {
                    sprintln!("Wifi ready");
                    wifi_ready = true;
                } else {
                    sprintln!("Wifi init failed");
                }
                start_time = get_time_ms();
            }
            if wifi_ready && use_wifi {
                if get_time_ms() - start_time > 20_000 {
                    sprintln!("starting ssid scan");
                    wfx_start_scan();
                    start_time = get_time_ms();
                }
            }
            if wfx_rs::hal_wf200::wf200_event_get() && use_wifi {
                // first thing -- clear the event. So that if we get another event
                // while handling this packet, we have a chance of detecting that.
                // we lack mutexes, so we need to think about this behavior very carefully.

                if wf200_mutex_get() { // don't process events while the driver has locked us out
                    wfx_rs::hal_wf200::wf200_event_clear();

                    // handle the Rx packet
                    if wfx_scan_ongoing() {
                        wfx_handle_event();
                    }
                }
            }
            //////////////////////// ---------------------------

            //////////////////////// CHARGER HANDLER BLOCK -----
            // I2C can't happen inside an interrupt routine, so we do it in the main loop
            // real time response is also not critical; note this runs "lazily", only if the COM loop is idle
            if get_time_ms() - last_run_time > 1000 {
                last_run_time = get_time_ms();
                loopcounter += 1;

                // routine pings & housekeeping; split i2c traffic across two phases to even the CPU load
                if loopcounter % 2 == 0 {
                    charger.chg_keepalive_ping(&mut i2c);
                    if !usb_cc_event {
                        usb_cc_event = usb_cc.check_event(&mut i2c);
                    }
                } else {
                    voltage = gg_voltage(&mut i2c);
                    if power_csr.rf(utra::power::STATS_STATE) == 1 {
                        current = gg_avg_current(&mut i2c);
                    } else {
                        // TODO: need more fine control over this
                        // at the moment, system can power on for 1 full second prior to getting this reading
                        stby_current = gg_avg_current(&mut i2c);
                    }
                }

                // check if we should turn the SoC on or not based on power status change events
                if charger.chg_is_charging(&mut i2c, false) || (power_csr.rf(utra::power::STATS_STATE) == 1) {
                    // sprintln!("charger insert or soc on event!");
                    let power =
                        power_csr.ms(utra::power::POWER_SELF, 1)
                        | power_csr.ms(utra::power::POWER_SOC_ON, 1)
                        | power_csr.ms(utra::power::POWER_DISCHARGE, 0);
                    power_csr.wo(utra::power::POWER, power); // turn off discharge if the soc is up
                }
            }
            //////////////////////// ---------------------------
        }

        //////////////////////// COM HANDLER BLOCK ---------
        while com_csr.rf(utra::com::STATUS_RX_AVAIL) == 1 {
            let rx: u16;
            unsafe{ rx = (*com_rd).read() as u16; }

            match rx {
                ComState::SSID_CHECK => {
                    if wf200_ssid_updated() { com_tx(1); } else { com_tx(0); }
                },
                ComState::SSID_FETCH => {
                    let ssid_list = wf200_ssid_get_list();

                    for index in 0..16 * 6 {
                        com_tx(ssid_list[index / 16].ssid[(index % 16)*2] as u16 |
                                 ((ssid_list[index / 16].ssid[(index % 16)*2+1] as u16) << 8)
                        );
                    }
                },
                ComState::LOOP_TEST => {
                    com_tx((rx & 0xFF) | ((com_sentinel as u16 & 0xFF) << 8));
                    com_sentinel += 1;
                },
                ComState::GAS_GAUGE => {
                    com_tx(current as u16);
                    com_tx(stby_current as u16);
                    com_tx(voltage as u16);
                    com_tx(power_csr.r(utra::power::POWER) as u16);
                },
                ComState::STAT => {
                    com_tx(0x8888);  // first is just a response to the initial command
                    charger.update_regs(&mut i2c);
                    for i in 0..0xC {
                        com_tx(charger.registers[i] as u16);
                    }
                    com_tx(voltage as u16);
                    com_tx(stby_current as u16);
                    com_tx(current as u16);
                },
                ComState::POWER_OFF => {
                    com_tx(power_csr.r(utra::power::POWER) as u16);
                    sprintln!("got power down request from soc!");
                    // ignore rapid, successive power down requests
                    if get_time_ms() - pd_loop_timer > 1500 {
                        let power =
                        power_csr.ms(utra::power::POWER_SELF, 1)
                        | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
                        power_csr.wo(utra::power::POWER, power);

                        set_msleep_target_ticks(500); // extend next service so we can discharge

                        pd_loop_timer = get_time_ms();
                    }
                },
                ComState::POWER_SHIPMODE => {
                    charger.set_shipmode(&mut i2c);
                    let power =
                    power_csr.ms(utra::power::POWER_SELF, 1)
                    | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
                    power_csr.wo(utra::power::POWER, power);
                    set_msleep_target_ticks(500); // extend next service so we can discharge

                    pd_loop_timer = get_time_ms();
                },
                ComState::READ_CHARGE_STATE => {
                    if charger.chg_is_charging(&mut i2c, false) { com_tx(1); } else { com_tx(0); }
                },
                ComState::GYRO_UPDATE => {
                    gyro.update_xyz();
                },
                ComState::GYRO_READ => {
                    com_tx(gyro.x);
                    com_tx(gyro.y);
                    com_tx(gyro.z);
                    com_tx(gyro.id as u16);
                },
                ComState::POLL_USB_CC => {
                    if usb_cc_event { com_tx(1) } else { com_tx(0) } usb_cc_event = false; // clear the usb_cc_event pending flag as its been checked
                    for i in 0..3 {
                        com_tx(usb_cc.status[i] as u16);
                    }
                },
                ComState::CHG_START => { // charging mode
                    charger.chg_start(&mut i2c);
                },
                ComState::CHG_BOOST_ON => { // boost on
                    charger.chg_boost(&mut i2c);
                },
                ComState::CHG_BOOST_OFF => { // boost off
                    charger.chg_boost_off(&mut i2c);
                },
                ComState::BL_START..=ComState::BL_END => {
                    let main_bl_level: u8 = (rx & 0x1F) as u8;
                    let sec_bl_level: u8 = ((rx >> 5) & 0x1F) as u8;
                    backlight.set_brightness(&mut i2c, main_bl_level, sec_bl_level);
                },
                ComState::LINK_READ => {
                    // this a "read continuation" command, in other words, return read data
                    // based on the current ComState
                },
                ComState::LINK_SYNC => {
                    // sync link command, when received, empty all the FIFOs, and prime Tx with dummy data
                    com_csr.wfo(utra::com::CONTROL_RESET, 1);  // reset fifos
                    com_csr.wfo(utra::com::CONTROL_CLRERR, 1); // clear all error flags
                    // com_tx(ComState::IDLE);
                },
                ComState::FLASH_ERASE => {
                    let mut error = false;
                    let mut address: u32 = 0;
                    let mut len: u32 = 0;
                    // receive address in "network order" (big endian)
                    match com_rx(100) {
                        Ok(result) => address = (result as u32) << 16,
                        _ => error = true,
                    }
                    match com_rx(100) {
                        Ok(result) => address |= (result as u32) & 0xFFFF,
                        _ => error = true,
                    }
                    // receive len, in bytes
                    match com_rx(100) {
                        Ok(result) => len = (result as u32) << 16,
                        _ => error = true,
                    }
                    match com_rx(100) {
                        Ok(result) => len |= (result as u32) & 0xFFFF,
                        _ => error = true,
                    }
                    if !error {
                        sprintln!("Erasing {} bytes from 0x{:08x}", len, address);
                        spi_erase_region(address, len);
                    }
                },
                ComState::FLASH_PP => {
                    let mut error = false;
                    let mut address: u32 = 0;
                    let mut page: [u8; 256] = [0; 256];
                    // receive address in "network order" (big endian)
                    match com_rx(100) {
                        Ok(result) => address = (result as u32) << 16,
                        _ => error = true,
                    }
                    match com_rx(100) {
                        Ok(result) => address |= (result as u32) & 0xFFFF,
                        _ => error = true,
                    }
                    for i in 0..128 {
                        match com_rx(100) {
                            Ok(result) => {
                                let b = result.to_le_bytes();
                                page[i*2] = b[0];
                                page[i*2+1] = b[1];
                            },
                            _ => error = true,
                        }
                    }
                    if !error {
                        // sprintln!("Programming 256 bytes to 0x{:08x}", address);
                        spi_program_page(address, &mut page);
                    }
                },
                ComState::FLASH_LOCK => {
                    flash_update_lock = true;
                    wifi_csr.wfo(utra::wifi::EV_ENABLE_WIRQ, 0);
                },
                ComState::FLASH_UNLOCK => {
                    flash_update_lock = false;
                    wifi_csr.wfo(utra::wifi::EV_ENABLE_WIRQ, 1);
                },
                ComState::FLASH_WAITACK => {
                    com_tx(ComState::FLASH_ACK);
                }
                _ => {
                    com_tx(ComState::ERROR);
                },
            }
        }
        //////////////////////// ---------------------------
        // unsafe { riscv::asm::wfi() }; // potential for power savings?
    }
}