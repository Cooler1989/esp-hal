//! Embassy DHCP Example
//!
//!
//! Set SSID and PASSWORD env variable before running this example.
//!
//! This gets an ip address via DHCP then performs an HTTP get request to some "random" server
//!
//! Because of the huge task-arena size configured this won't work on ESP32-S2

//% FEATURES: async embassy embassy-generic-timers esp-wifi esp-wifi/async esp-wifi/embassy-net esp-wifi/wifi-default esp-wifi/wifi esp-wifi/utils
//% CHIPS: esp32 esp32s3 esp32c2 esp32c3 esp32c6

#![no_std]
#![no_main]

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Config, Ipv4Address, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::ClockControl,
    peripherals::Peripherals,
    rng::Rng,
    system::SystemControl,
    timer::timg::TimerGroup,
};
use heapless::String;
use esp_println::println;
use esp_wifi::random;
use esp_wifi::{
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
use core::cell::RefCell;
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    //  utils::rng_generator::CountingRng,
};

static RNG: Mutex<RefCell<Option<RngDummy>>> = Mutex::new(RefCell::new(None));

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

use rand_core::{Error, RngCore};

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
        unimplemented!()
    }
    fn fill_bytes(&mut self, dst: &mut [u8]){
        unimplemented!()
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), rand_core::Error> {
        unimplemented!()
    }
}

const CLIENT_ID: &'static str = "client_esp32_id";

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let peripherals = Peripherals::take();

    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::max(system.clock_control).freeze();

    let timg0 = TimerGroup::new(peripherals.TIMG0, &clocks);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timg0.timer0,
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

    let config = Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

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

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
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
            //  use embedded_io_async::Write;
            //  let r = socket
            //      .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
            //      .await;
            //  if let Err(e) = r {
            //      println!("write error: {:?}", e);
            //      break;
            //  }
            //  let n = match socket.read(&mut buf).await {
            //      Ok(0) => {
            //          println!("read EOF");
            //          break;
            //      }
            //      Ok(n) => n,
            //      Err(e) => {
            //          println!("read error: {:?}", e);
            //          break;
            //      }
            //  };
            //  println!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        Timer::after(Duration::from_millis(3000)).await;
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
