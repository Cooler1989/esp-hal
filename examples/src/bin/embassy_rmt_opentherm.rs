//! Demonstrates decoding pulse sequences with RMT
//! Connect GPIO27

//% CHIPS: esp32 esp32c3 esp32c6 esp32h2 esp32s2 esp32s3
//% FEATURES: async embassy embassy-generic-timers esp-wifi esp-wifi/async esp-wifi/embassy-net esp-wifi/wifi-default esp-wifi/wifi esp-wifi/utils

#![no_std]
#![no_main]

use core::cell::RefCell;
use heapless::Vec;

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Config, Ipv4Address, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::ClockControl,
    gpio::{Input, Pull, Io, Level, Output, AnyInput},
    peripherals::Peripherals,
    prelude::*,
    rng::Rng,
    Async,
    rmt::{asynch::{RxChannelAsync, TxChannelAsync}, PulseCode, Rmt, TxChannelConfig, RxChannelConfig,
        RxChannelCreatorAsync, TxChannelCreatorAsync},
    system::SystemControl,
    timer::timg::TimerGroup,
};
use heapless::String;
use esp_println::{print, println};
use esp_wifi::{
    random,
    initialize,
    wifi::{
        ClientConfiguration,
        Configuration,
        WifiController,
        WifiDevice,
        WifiEvent,
        WifiStaDevice,
        WifiState,
    },
    EspWifiInitFor,
};

use rand_core::{Error, RngCore};
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    //  utils::rng_generator::CountingRng,
};

static RNG: Mutex<RefCell<Option<RngDummy>>> = Mutex::new(RefCell::new(None));
const CLIENT_ID: &'static str = "client_esp32_id";

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

struct RngDummy { }

impl RngDummy {
    pub fn new() -> Self {
        Self{}
    }
}

impl RngCore for RngDummy {
    fn next_u32(&mut self) -> u32 {
        unsafe {
            return random();
        }
    }
    fn next_u64(&mut self) -> u64 {
        unsafe {
            return random() as u64 | ((random() as u64) << 32);
        }
    }
    fn fill_bytes(&mut self, dst: &mut [u8]){
        unimplemented!()
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), rand_core::Error> {
        unimplemented!()
    }
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

use opentherm_boiler_controller_lib::{BoilerControl, TimeBaseRef, Instant};
use opentherm_boiler_controller_lib::opentherm_interface::{OpenThermEdgeTriggerBus, DataOt, Error as OtError, OpenThermMessage};
use opentherm_boiler_controller_lib::opentherm_interface::edge_trigger_capture_interface::{
    CaptureError, EdgeCaptureInterface, EdgeTriggerInterface, InitLevel, TriggerError,
};
use opentherm_boiler_controller_lib::opentherm_interface::api;
//  use opentherm_boiler_controller_lib::opentherm_interface::api::OpenThermBus;
use opentherm_boiler_controller_lib::opentherm_interface::OpenThermInterface;

pub struct EspOpenthermRmt<E: EdgeCaptureInterface, T: EdgeTriggerInterface>{
    edge_capture_drv: E,
    edge_trigger_drv: T,
    send_count: usize,
}

const TOTAL_CAPTURE_OT_FRAME_SIZE: usize = 34usize;

impl<E: EdgeCaptureInterface,T: EdgeTriggerInterface> EspOpenthermRmt<E, T> {
    pub fn new( edge_capture_driver: E, edge_trigger_driver: T) -> Self {
        Self{
            edge_capture_drv: edge_capture_driver,
            edge_trigger_drv: edge_trigger_driver,
            send_count: 0,
        }
    }
}

impl<E: EdgeCaptureInterface,T: EdgeTriggerInterface> EdgeTriggerInterface for EspOpenthermRmt<E, T> {
    async fn trigger(
        &mut self,
        iterator: impl Iterator<Item = bool>,
        period: core::time::Duration,
    ) -> Result<(), TriggerError> {
        self.edge_trigger_drv
            .trigger(iterator, period)
            .await
    }
}

const VEC_SIZE_OT: usize = 128usize;
// Inverted - invert polarity of the signal
// N - output vec size
impl<E: EdgeCaptureInterface,T: EdgeTriggerInterface> EdgeCaptureInterface<VEC_SIZE_OT> for EspOpenthermRmt<E, T> {
//  impl<const N: usize, const Inverted: bool> EdgeCaptureInterface<N>
    //  for RmtEdgeCapture<N, Inverted>
    //
    //  TODO:
    //  make a list
    async fn start_capture(
        &mut self,
        timeout_inactive_capture: core::time::Duration,
        timeout_till_active_capture: core::time::Duration,
    ) -> Result<(InitLevel, Vec<core::time::Duration, VEC_SIZE_OT>), CaptureError> {

        self.edge_capture_drv.start_capture(timeout_inactive_capture, timeout_till_active_capture).await
    }
}

//  Used to pull in the implementation of OpenThermInterface:
impl<E: EdgeCaptureInterface,T: EdgeTriggerInterface> OpenThermEdgeTriggerBus for EspOpenthermRmt<E, T> {}

pub struct RmtEdgeTrigger<const NegativeEdgeIsBinaryOne: bool> {
    rmt_channel_tx: esp_hal::rmt::Channel<Async,1>,
}

impl<const NegativeEdgeIsBinaryOne: bool> RmtEdgeTrigger<NegativeEdgeIsBinaryOne> {
    pub fn new(mut rmt_channel: esp_hal::rmt::Channel<Async,1>) -> Self {
        println!("Create new RmtEdgeTrigger dev");
        Self { rmt_channel_tx: rmt_channel }
    }
}

pub struct RmtEdgeCapture<'pin, const N: usize, const Inverted: bool = false> {
    input_pin: AnyInput<'pin>,
    rmt_channel_rx: esp_hal::rmt::Channel<Async, 0>,
}

impl<'pin, const N: usize, const Inverted: bool> RmtEdgeCapture<'pin, N, Inverted> {
    pub fn new(mut rmt_channel: esp_hal::rmt::Channel<Async,0>, input_pin_arg: AnyInput<'pin> ) -> Self {
        Self { rmt_channel_rx: rmt_channel, input_pin: input_pin_arg }
    }
    #[inline]
    fn insert(
        vector: &mut Vec<core::time::Duration, N>,
        period: core::time::Duration,
    ) -> Result<(), core::time::Duration> {
        vector.insert(0, period)
        //  vector.push(period)
    }
}

//  _____|''|_____|''|__|''|__    ___|''|__|'''''|__|''|___   //  0x200000003
//  -----|  1  |  0  |  0  |      |  0  |  0  |  1  |  1  |
impl<const NegativeEdgeIsBinaryOne: bool> EdgeTriggerInterface for RmtEdgeTrigger<NegativeEdgeIsBinaryOne> {
    async fn trigger(
        &mut self,
        iterator: impl Iterator<Item = bool>,
        period: core::time::Duration,
    ) -> Result<(), TriggerError> {

        println!("call trigger for RmtEdgeTrigger dev");
        let period = period.as_nanos() as u16;
        let period = 625u16;
        let mut data = [PulseCode::default(); 48];
        //  {
        //      level1: true,
        //      length1: 100,
        //      level2: false,
        //      length2: 20,
        //  }
        //  data[data.len() - 1] = PulseCode::default();

        for (i, entry) in iterator.enumerate() {
            let entry = if NegativeEdgeIsBinaryOne {
                !entry
            } else {
                entry
            };
            //  println!("[{i}],e={}", entry);
            match i%2 {
                0 => {
                    data[i/2].level1 = entry;
                    data[i/2].length1 = period;
                },
                _ => {
                    data[i/2].level2 = entry;
                    data[i/2].length2 = period;
                }
            }
        }

        //  for (i, entry) in data[..data.len()].iter().enumerate() {
        //      println!("[{i}],e={},l={}, e2={},l2={}", entry.level1, entry.length1, entry.level2, entry.length2);
        //  }

        println!("transmit");
        self.rmt_channel_tx.transmit(&data).await.unwrap();

        //  let period = MANCHESTER_OPENTHERM_RESOLUTION;
        //  self.output_pin.set_low(); //  generate idle state:
        //  Timer::after(3 * convert_duration_to_embassy(period)).await; //  await one period in idle state

        //  log::info!("Edge Trigger sent count: {count}");

        //  self.output_pin.set_low(); //  generate idle state after
        //  Timer::after(convert_duration_to_embassy(period)).await; //  await one period in idle state
        Ok(())
    }
}

// Inverted - invert polarity of the signal
// N - output vec size
impl<'pin, const N: usize, const Inverted: bool> EdgeCaptureInterface<N>
    for RmtEdgeCapture<'pin, N, Inverted>
{
    //  TODO:
    //  make a list
    async fn start_capture(
        &mut self,
        timeout_inactive_capture: core::time::Duration,
        timeout_till_active_capture: core::time::Duration,
    ) -> Result<(InitLevel, Vec<core::time::Duration, N>), CaptureError> {

        let init_state = match self.input_pin.is_high() {
            true => InitLevel::High,
            false => InitLevel::Low,
        };

        //  let mut capture_timestamp = start_timestamp;
        //  let mut current_level = init_state.clone();
        let mut timestamps = Vec::<core::time::Duration, N>::new();

        const CAPTURE_DATA_SIZE: usize = VEC_SIZE_OT;
        let mut capture_data = [PulseCode {
            level1: true,
            length1: 1,
            level2: false,
            length2: 1,
        }; CAPTURE_DATA_SIZE];

        //  let mut capture_data: [u32; CAPTURE_DATA_SIZE] = [0; CAPTURE_DATA_SIZE];

        //  TODO: implement timeout or just handle it here:
        let timestamps = match self.rmt_channel_rx.receive(&mut capture_data).await {
            Ok(()) => {  //  Convert the data and return them
                timestamps
            },
            Err(_) => { return Err(CaptureError::GenericError); }
        };

        //  todo!()  //  Assert

        //  log::info!("Return InitLevel: {:?}", init_state);
        Ok((init_state, timestamps))
    }
}

const WIDTH: usize = 80;
const RMT_CLK_DIV: u8 = 64;

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

struct EspTime {}

impl EspTime {
    pub fn new() -> Self {
        Self{}
    }
}
impl TimeBaseRef for EspTime {
    fn now(&self) -> Instant {
        todo!()
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}

#[embassy_executor::task]
async fn mqtt_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    println!("Waiting to get IP address...");

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let remote_endpoint = (Ipv4Address::new(192, 168, 7, 1), 1883);
        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");

        let mut rng = RngDummy::new();
        log::info!("rng.next_u32() = {:x}", rng.next_u32());
        log::info!("rng.next_u32() = {:x}", rng.next_u32());
        log::info!("rng.next_u32() = {:x}", rng.next_u32());
        //  critical_section::with(|cs| RNG.borrow_ref_mut(cs).replace(rng));

        let mut config = ClientConfig::<5, RngDummy>::new(rust_mqtt::client::client_config::MqttVersion::MQTTv5, rng);
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id(CLIENT_ID);
        config.max_packet_size = 512;

        let mut recv_buffer = [0; 512];
        let mut write_buffer = [0; 512];
        let mut client = MqttClient::<_, 5, _>::new(socket, &mut write_buffer, 80, &mut recv_buffer, 80, config);

        if let Err(_) = client.connect_to_broker().await {
            log::error!("Unable to connect to MQTT borker");
        }

        loop {
            Timer::after(Duration::from_millis(500)).await;
            let temperature_string: String<32> = String::try_from("21").unwrap();
            match client
                .send_message(
                    "temperature/1",
                    temperature_string.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    true,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        log::info!("MQTT Network Error");
                        //  unimplemented!();
                        continue;
                    }
                    _ => {
                        //  log::info!("Other MQTT Error: {:?}", mqtt_error);
                        //  unimplemented!();
                        continue;
                    }
                },
            }
            //  let mut buf = [0; 1024];
        }

        loop {
            Timer::after(Duration::from_millis(3000)).await;
        }
    }

}

#[embassy_executor::task]
async fn boiler_task(mut boiler: BoilerControl<EspOpenthermRmt<RmtEdgeCapture<'static, 128>,
    RmtEdgeTrigger::<true>>, EspTime>) {
    loop {
        println!("send the trigger data");
        //  rmt_tx.trigger(data.clone().into_iter(), core::time::Duration::from_millis(500)).await;
        //  input.wait_for_falling_edge().await;
        println!("loop");
        boiler.process().await.unwrap();
        Timer::after(Duration::from_millis(800)).await;
    }
}

#[main]
async fn main(spawner: Spawner) {
    println!("Init!");
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::boot_defaults(system.clock_control).freeze();

    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    cfg_if::cfg_if! {
        if #[cfg(feature = "esp32h2")] {
            let freq = 32.MHz();
        } else {
            let freq = 80.MHz();
        }
    };

    let init = initialize(
        EspWifiInitFor::Wifi,
        timer_group0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    #[cfg(feature = "esp32")]
    {
        let timg1 = TimerGroup::new(peripherals.TIMG1, &clocks);
        esp_hal_embassy::init(&clocks, timg1.timer0);
    }

    #[cfg(not(feature = "esp32"))]
    {
        let systimer = esp_hal::timer::systimer::SystemTimer::new(peripherals.SYSTIMER)
            .split::<esp_hal::timer::systimer::Target>();
        esp_hal_embassy::init(&clocks, systimer.alarm0);
    }

    let mut rng = RngDummy::new();
    log::info!("rng.next_u32() = {:x}", rng.next_u32());
    log::info!("rng.next_u32() = {:x}", rng.next_u32());

    let config = Config::dhcpv4(Default::default());

    let mut button = AnyInput::new(io.pins.gpio0, Pull::Up);
    let mut led = Output::new(io.pins.gpio2, Level::High);

    let rmt = Rmt::new_async(peripherals.RMT, freq, &clocks).unwrap();
    let rx_config = RxChannelConfig {
        clk_divider: RMT_CLK_DIV,
        idle_threshold: 10000,
        ..RxChannelConfig::default()
    };

    let mut channel_tx =
        TxChannelCreatorAsync::configure(rmt.channel1, io.pins.gpio27,
            TxChannelConfig{
                clk_divider: RMT_CLK_DIV,
                idle_output_level: true,
                idle_output: true,
                carrier_modulation: false,
                //  carrier_high: 0u16,
                //  carrier_low: 0u16,
                carrier_high: 1050u16,
                carrier_low: 1050u16,
                carrier_level: true,
        }).unwrap();

    cfg_if::cfg_if! {
        if #[cfg(any(feature = "esp32", feature = "esp32s2"))] {
            let mut channel_rx = RxChannelCreatorAsync::configure(rmt.channel0, io.pins.gpio4, rx_config).unwrap();
        } else if #[cfg(feature = "esp32s3")] {
            let mut channel_rx = rmt.channel7.configure(io.pins.gpio4, rx_config).unwrap();
        } else {
            let mut channel_rx = rmt.channel2.configure(io.pins.gpio4, rx_config).unwrap();
        }
    }

    const NegativeEdgeIsBinaryOne: bool = true;
    let mut rmt_tx = RmtEdgeTrigger::<NegativeEdgeIsBinaryOne>::new(channel_tx);
    //  TODO: set correct pin number:
    let capture_level_input = AnyInput::new(io.pins.gpio23, Pull::None);
    let mut rmt_rx: RmtEdgeCapture<128> = RmtEdgeCapture::new(channel_rx, capture_level_input);

    let opentherm_device = EspOpenthermRmt::new(rmt_rx, rmt_tx);
    let esp_time = EspTime::new();

    let mut boiler = BoilerControl::new(opentherm_device, esp_time);

    //  Mqtt
    let seed = rng.next_u64(); // very random, very secure seed

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();
    spawner.spawn(mqtt_task(&stack)).ok();
    spawner.spawn(boiler_task(boiler)).ok();


    println!("Start loop");
    loop {
        Timer::after(Duration::from_millis(1000)).await;
        led.toggle();
    }
}
