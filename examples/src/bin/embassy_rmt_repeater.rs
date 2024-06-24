//! Demonstrates decoding pulse sequences with RMT
//! Connect GPIO27

//% CHIPS: esp32 esp32c3 esp32c6 esp32h2 esp32s2 esp32s3
//% FEATURES: async embassy embassy-time-timg0 embassy-generic-timers

#![no_std]
#![no_main]

use core::cell::RefCell;

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::ClockControl,
    gpio::{Input, Pull, Io, Level, Output, AnyInput},
    peripherals::Peripherals,
    prelude::*,
    Async,
    rmt::{asynch::{RxChannelAsync, TxChannelAsync}, PulseCode, Rmt, TxChannelConfig, RxChannelConfig,
        RxChannelCreatorAsync, TxChannelCreatorAsync},
    system::SystemControl,
};
use esp_println::{print, println};

const WIDTH: usize = 80;
const RMT_CLK_DIV: u8 = 128;

static RECEIVED_COUNT: Mutex<RefCell<Option<u32>>> =
    Mutex::new(RefCell::new(None));

static RECEIVED_DATA : critical_section::Mutex<RefCell<Option<[PulseCode; 48]>>> =
    Mutex::new(RefCell::new(
        Some([PulseCode {
                level1: true,
                length1: 0,
                level2: false,
                length2: 0,
            }; 48]) ));

#[cfg(debug_assertions)]
compile_error!("Run this example in release mode");

#[embassy_executor::task]
async fn signal_task(mut channel: esp_hal::rmt::Channel<Async, 1>, mut button: AnyInput<'static>) {
    let mut level = false;
    loop {
        let new_level = button.is_high();
        if level != new_level {
            level = new_level;
            if new_level == true {
                Timer::after(Duration::from_millis(500)).await;
                continue;
            }
            let (count, data) = critical_section::with(|cs| {
                let count = RECEIVED_COUNT.borrow_ref(cs);
                if *count == None {
                    return (0, None);
                }
                let data = RECEIVED_DATA.borrow_ref(cs);
                let ret_data = match *data {
                    Some(data) => {
                        println!("borrow_ref Some(data)");
                        let mut ret_data = [PulseCode::default(); 48];

                        for (i, entry) in data[..data.len()].iter().enumerate() {
                            ret_data[i].level1 = !(*entry).level1;
                            ret_data[i].length1 = (*entry).length1;
                            ret_data[i].level2 = !(*entry).level2;
                            ret_data[i].length2 = (*entry).length2;
                        }
                        ret_data[((*count).unwrap_or(0u32)) as usize] = PulseCode::default();
                        Some(ret_data)
                    },
                    None => {None}
                };

                let count = (*count).unwrap_or(0);
                (count, ret_data)
            });

            println!("button {new_level}, count: {count}");
            if let Some(data) = data {
                //  Send it to the ether:
                channel.transmit(&data).await.unwrap();
                let mut total = 0usize;
                let mut i = 0;
                for entry in &data[..data.len()] {
                    println!("[{i}]: e.l1 = {}, e.l2 ={}", entry.length1, entry.length2);
                    i+=1;
                    if entry.length1 == 0 {
                        break;
                    }
                    total += entry.length1 as usize;

                    if entry.length2 == 0 {
                        break;
                    }
                    total += entry.length2 as usize;
                }
                println!("data size {}, total: {}", data.len(), total);
                for entry in &data[..data.len()] {
                    if entry.length1 == 0 {
                        break;
                    }

                    let count = WIDTH / (total / entry.length1 as usize);
                    let c = if entry.level1 { '-' } else { '_' };
                    for _ in 0..count + 1 {
                        print!("{}", c);
                    }

                    if entry.length2 == 0 {
                        break;
                    }

                    let count = WIDTH / (total / entry.length2 as usize);
                    let c = if entry.level2 { '-' } else { '_' };
                    for _ in 0..count + 1 {
                        print!("{}", c);
                    }
                }
                println!();

            }
            else
            {
                println!("Missing data");
            }
            Timer::after(Duration::from_millis(1000)).await;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[main]
async fn main(spawner: Spawner) {
    println!("Init!");
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::boot_defaults(system.clock_control).freeze();

    let timer_group0 = esp_hal::timer::timg::TimerGroup::new_async(peripherals.TIMG0, &clocks);
    esp_hal_embassy::init(&clocks, timer_group0);

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    cfg_if::cfg_if! {
        if #[cfg(feature = "esp32h2")] {
            let freq = 32.MHz();
        } else {
            let freq = 80.MHz();
        }
    };

    {
        //  let mut input = Input::new(io.pins.gpio27, Pull::Down);
        //  let mut input = Input::new(io.pins.gpio4, Pull::Down);
    }

    let mut button = AnyInput::new(io.pins.gpio0, Pull::Up);
    let mut led = Output::new(io.pins.gpio2, Level::High);

    let rmt = Rmt::new_async(peripherals.RMT, freq, &clocks).unwrap();
    let rx_config = RxChannelConfig {
        clk_divider: RMT_CLK_DIV,
        idle_threshold: 10000,
        ..RxChannelConfig::default()
    };

    let mut data = [PulseCode {
        level1: true,
        length1: 100,
        level2: false,
        length2: 20,
    }; 48];
    data[data.len() - 1] = PulseCode::default();

    let mut channel_tx =
        TxChannelCreatorAsync::configure(rmt.channel1, io.pins.gpio27,
            TxChannelConfig{
                clk_divider: RMT_CLK_DIV,
                idle_output_level: false,
                idle_output: false,
                carrier_modulation: true,
                //  carrier_high: 0u16,
                //  carrier_low: 0u16,
                carrier_high: 1050u16,
                carrier_low: 1050u16,
                carrier_level: true,
        }).unwrap();

    for (i, entry) in data[..data.len()].iter().enumerate() {
        println!("[{i}],e={},l={}, e2={},l2={}", entry.level1, entry.length1, entry.level2, entry.length2);
    }

    println!("transmit");
    channel_tx.transmit(&data).await.unwrap();
    Timer::after(Duration::from_millis(500)).await;

    cfg_if::cfg_if! {
        if #[cfg(any(feature = "esp32", feature = "esp32s2"))] {
            let mut channel = RxChannelCreatorAsync::configure(rmt.channel0, io.pins.gpio4, rx_config).unwrap();
        } else if #[cfg(feature = "esp32s3")] {
            let mut channel = rmt.channel7.configure(io.pins.gpio4, rx_config).unwrap();
        } else {
            let mut channel = rmt.channel2.configure(io.pins.gpio4, rx_config).unwrap();
        }
    }

    spawner
        .spawn(signal_task(channel_tx, button))
        .unwrap();

    //  channel.transmit(&data).await.unwrap();

    for i in 1..7 {
        led.toggle();
        Timer::after(Duration::from_millis(100)).await;
    }
    loop {
        println!("receive");
        channel.receive(&mut data).await.unwrap();
        println!("channel received");

        let mut total = 0usize;
        let mut iter = data[..data.len()].iter().enumerate();
        let length = loop {
            match iter.next() {
                Some((i, entry)) => {
                    println!("[{i}]: e.l1 = {}, e.l2 ={}", entry.length1, entry.length2);
                    if entry.length1 == 0 {
                        break i + 1;
                    }
                    total += entry.length1 as usize;

                    if entry.length2 == 0 {
                        break i + 1;
                    }
                    total += entry.length2 as usize;
                },
                None => { break data.len(); }
            }

        };

        let mut ret_data = [PulseCode::default(); 48];
        for (i, entry) in data[..data.len()].iter().enumerate() {
            ret_data[i] = *entry;
        }

        for (i, entry) in ret_data[..data.len()].iter().enumerate() {
            println!("a[{i}]={:?}", *entry);
            if i == length { break; }
        }

        critical_section::with(|cs| {
            let mut count = RECEIVED_COUNT.borrow_ref_mut(cs);
            if let Some(_) = *count {
                return;
            }
            *count = Some(length as u32);
            let mut cs_data = RECEIVED_DATA.borrow_ref_mut(cs);

            *cs_data = Some(ret_data);
        });
        //  just a led toggle:
        for i in &data[..length] {
            led.toggle();
            Timer::after(Duration::from_millis(100)).await;
        }


        for entry in &data[..data.len()] {
            if entry.length1 == 0 {
                break;
            }

            let count = WIDTH / (total / entry.length1 as usize);
            let c = if entry.level1 { '-' } else { '_' };
            for _ in 0..count + 1 {
                print!("{}", c);
            }

            if entry.length2 == 0 {
                break;
            }

            let count = WIDTH / (total / entry.length2 as usize);
            let c = if entry.level2 { '-' } else { '_' };
            for _ in 0..count + 1 {
                print!("{}", c);
            }
        }

        println!();
        println!("loop closing");
        if length > 10 {
            println!("Start loop");
            loop {
                //  input.wait_for_falling_edge().await;
                println!("loop");
                Timer::after(Duration::from_millis(1000)).await;
            }
        }
    }

    println!("Start loop");
    loop {
        //  input.wait_for_falling_edge().await;
        println!("loop");
        Timer::after(Duration::from_millis(1000)).await;
    }
}
