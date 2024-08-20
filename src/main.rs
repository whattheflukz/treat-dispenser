#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]
#![allow(dead_code)]
#![allow(unreachable_code)]

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use defmt::{info, warn, error, trace, debug};
use rand::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json_core::{self, from_slice};
use embedded_svc::http::status;
use embedded_hal::pwm::SetDutyCycle;
use embedded_io_async::{Read, Write};
use esp_hal::{
    clock::{ ClockControl, CpuClock },
    gpio::{ any_pin::AnyPin, AnyInput, AnyOutput, GpioPin, Io },
    interrupt,
    ledc::{ self, channel::{ self, config }, timer::{self}, LSGlobalClkSource, Ledc, LowSpeed },
    mcpwm::operator::PwmPin,
    peripherals::{ Interrupt, Peripherals },
    prelude::{ _fugit_RateExtU32, * },
    riscv::register::hpmcounter10h::read,
    rng::Rng,
    rtc_cntl::Rtc,
    system,
    timer::{ systimer::{ self, SystemTimer }, timg::TimerGroup, ErasedTimer, OneShotTimer, PeriodicTimer }
};
use esp_wifi::{
    wifi::{ WifiController, WifiDevice, WifiEvent, WifiStaDevice, WifiState },
    { initialize, EspWifiInitFor },
};
use embassy_executor::{SendSpawner, Spawner};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex, RawMutex},
    channel::{Channel, Sender}, pubsub::PubSubChannel
};
use embassy_net::{
    dns::{ DnsQueryType, DnsSocket },
    tcp::{ client::{ self, TcpClientState, TcpClient }, TcpSocket },
    Config,
    Stack,
    StackResources,
};
use embassy_time::{ with_timeout, Duration, TimeoutError, Timer, WithTimeout };
use rust_mqtt::{ client::{ client::MqttClient, client_config::ClientConfig },
    packet::v5::reason_codes::ReasonCode,
    utils::rng_generator::CountingRng,
};
use core::{fmt::Write as CoreWrite, str::from_utf8};
use esp_backtrace as _; 
use esp_println::{print, println};

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

static mut CLOCKS: Option<esp_hal::clock::Clocks> = None;
static TREAT_CHANNEL: PubSubChannel::<CriticalSectionRawMutex, u32, 2, 2, 2> = PubSubChannel::<CriticalSectionRawMutex, u32, 2, 2, 2>::new();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const MQTT_USER: &str = env!("MQTT_USER");
const MQTT_PASSWORD: &str = env!("MQTT_PASSWORD");
const MQTT_HOST: &str = env!("MQTT_HOST");
const SLEEP_DURATION: u64 = 6_000;

#[derive(Debug, Serialize, Deserialize)]
struct SubStruct{
    pub kind: heapless::String<256>,
    pub etag: heapless::String<256>,
    #[serde(alias = "pageInfo")]
    pub page_info: PageInfo,
    pub items: [Items; 1]
}

#[derive(Debug, Serialize, Deserialize)]
struct PageInfo{
    #[serde(alias = "totalResults")]
    pub total_results: u32,
    #[serde(alias = "resultsPerPage")]
    pub results_per_page: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Items{
    pub kind: heapless::String<256>,
    pub etag: heapless::String<256>,
    pub id: heapless::String<256>,
    pub statistics: Statistics
}

#[derive(Debug, Serialize, Deserialize)]
struct Statistics{
    #[serde(alias = "viewCount")]
    pub view_count: heapless::String<256>,
    #[serde(alias = "subscriberCount")]
    pub subscriber_count: heapless::String<256>,
    #[serde(alias = "hiddenSubscriberCount")]
    pub hidden_subscriber_count: bool,
    #[serde(alias = "videoCount")]
    pub video_count: heapless::String<256>,
}

// maintains wifi connection, when it disconnects it tries to reconnect
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    const THREAD_INFO: &str = "[CONNECTION_TASK]:";
    println!("{} start connection task", THREAD_INFO);
    println!("{} Device capabilities: {:?}", THREAD_INFO, controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                println!("{} wifi disconnected", THREAD_INFO);
                Timer::after(Duration::from_millis(5000)).await
            },
            WifiState::Invalid => {
                println!("{} Invalid state", THREAD_INFO); Timer::after(Duration::from_millis(5000)).await
            },
            WifiState::StaDisconnected => {
                println!("{} wifi disconnected", THREAD_INFO);
                Timer::after(Duration::from_millis(5000)).await
            },
            WifiState::StaStarted => {
                println!("{} wifi started", THREAD_INFO);
                Timer::after(Duration::from_millis(5000)).await
            },
            WifiState::StaStopped => {
                println!("{} wifi stopped", THREAD_INFO);
                Timer::after(Duration::from_millis(5000)).await
            },
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config: esp_wifi::wifi::Configuration = esp_wifi::wifi::Configuration::Client(esp_wifi::wifi::ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("{} Starting wifi", THREAD_INFO);
            controller.start().await.unwrap();
            println!("{} Wifi started!", THREAD_INFO);
        }
        println!("{} About to connect...", THREAD_INFO);

        match controller.connect().await {
            Ok(_) => println!("{} Wifi connected!", THREAD_INFO),
            Err(e) => {
                println!("{} Failed to connect to wifi: {:?}", THREAD_INFO, e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

// A background task, to process network events - when new packets, they need to processed, embassy-net, wraps smoltcp
#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}

#[embassy_executor::task]
async fn servo_driver_task(mut servo_pin: GpioPin<0>, mut ledc: Ledc<'static>) {
    const THREAD_INFO: &str = "[SERVO_DRIVER_TASK]:";
    let mut treat_sub_channel = TREAT_CHANNEL.subscriber().unwrap();
    let mut treat_pub_channel = TREAT_CHANNEL.publisher().unwrap();
    let mut last_treat_count = treat_sub_channel.try_next_message_pure().unwrap_or(0);

    const PWM_FREQUENCY: u32 = 50;
    const MIN_DUTY_CYCLE: u16 = 1000; 
    const MAX_DUTY_CYCLE: u16 = 4000; 
    const DUTY_FRACTION: u16 = 65535;
    let mut timer = ledc.get_timer::<LowSpeed>(timer::Number::Timer0);
    timer.configure(
        timer::config::Config {
        duty: timer::config::Duty::Duty14Bit,
        clock_source: timer::LSClockSource::APBClk,
        frequency: PWM_FREQUENCY.Hz(),
    }).unwrap();

    let mut channel = ledc.get_channel(channel::Number::Channel0, &mut servo_pin);
    channel
        .configure(channel::config::Config {
            timer: &timer,
            duty_pct: 0, 
            pin_config: esp_hal::ledc::channel::config::PinConfig::PushPull,
        }).unwrap();
    
    loop {

        let treat_count = treat_sub_channel.try_next_message_pure().unwrap_or(0);
        let dispense = { treat_count > last_treat_count };

        println!("{} Dispense: {}", THREAD_INFO, dispense);

        if !dispense {
            println!("{} No treats to dispense: {}", THREAD_INFO, treat_count);
            Timer::after(Duration::from_millis(SLEEP_DURATION)).await;
            continue;
        }

        println!("{} Dispensing treats: {}", THREAD_INFO, treat_count);
        
        println!("{} Moving to minimum position: {:?}", THREAD_INFO, MIN_DUTY_CYCLE);
        for i in MIN_DUTY_CYCLE..1600{
            //println!("i: {:?}", i);
            _ = channel.set_duty_cycle_fraction(i,DUTY_FRACTION);
            Timer::after(Duration::from_millis(10)).await;
        }

        println!("{} Moving to maximum position: {:?}", THREAD_INFO, MAX_DUTY_CYCLE);
        for i in 2900..MAX_DUTY_CYCLE{
            //println!("i: {:?}", i);
            _ = channel.set_duty_cycle_fraction(i,DUTY_FRACTION);
            Timer::after(Duration::from_millis(10)).await;
        }

        _ = channel.set_duty_cycle(0);

        last_treat_count += 1;

        println!("{} Treat dispensed: {}", THREAD_INFO, treat_count);
    }
}

#[embassy_executor::task]
async fn post_mqtt_message_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    const THREAD_INFO: &str = "[POST_MQTT_MESSAGE_TASK]:";
    let mut treat_sub_channel = TREAT_CHANNEL.subscriber().unwrap();
    let mut treat_pub_channel = TREAT_CHANNEL.publisher().unwrap();
    treat_pub_channel.publish_immediate(0);
    let mut last_sub_count: u64 = 0;
    loop {
        println!("{} Waiting for link", THREAD_INFO);
        if stack.is_link_up() { println!("{} Link is up", THREAD_INFO); }
        else {
            println!("{} Link is down", THREAD_INFO);
            Timer::after(Duration::from_millis(5000)).await;
            continue;
        }

        let mut rx_buffer = [0; 4096];
        let mut tx_buffer = [0; 4096];
        let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);
        let address = match stack
            .dns_query(MQTT_HOST.try_into().unwrap(), DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(e) => {
                println!("DNS lookup error: {:?}", e);
                continue;
            }
        };

        let remote_endpoint = (address, 1883);
        let mut connection = socket.connect(remote_endpoint).await;
        if let Err(e) = connection {
            println!("connect error: {:?}", e);
            continue;
        }

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_username(MQTT_USER.try_into().unwrap());
        config.add_password(MQTT_PASSWORD.try_into().unwrap());
        config.max_packet_size = 100;
        let mut recv_buffer = [0; 80];
        let mut write_buffer = [0; 80];

        let mut client = MqttClient::<_, 5, _>::new(socket, &mut write_buffer, 80, &mut recv_buffer, 80, config);
        match client.connect_to_broker().await {
            Ok(()) => { println!("{} Connected to broker", THREAD_INFO); }
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    println!("MQTT Network Error: {:?}
                    ", mqtt_error);
                    Timer::after(Duration::from_millis(5000)).await;
                    continue;
                }
                _ => {
                    println!("Other MQTT Error: {:?}", mqtt_error);
                    Timer::after(Duration::from_millis(5000)).await;
                    continue;
                }
            },
        }

        const TOPIC: &str = "treat/dispense";
        let res = match client.subscribe_to_topic(TOPIC).await {
            Ok(r) => {
                println!("{} Subscribed to topic: {}", THREAD_INFO, TOPIC);
                r
            },
            Err(e) => {
                println!("{} Error subscribing to topic: {:?}", THREAD_INFO, e);
                continue;
            }
        };

        loop {
            
            let msg = match client.receive_message().await { 
                Ok(m) => m,
                Err(e) => {
                    println!("{} No message received, {:?}", THREAD_INFO, e);
                    break;
                }
            };

            println!("{} done waiting for message", THREAD_INFO);

            // get locally stored number of treats to dispense
            let mut dispensed_treats: u32 = match treat_sub_channel.try_next_message_pure() {
                Some(t) => t,
                None => {
                    println!("{} Error getting treats from channel", THREAD_INFO);
                    break;
                },
            };
            println!("{} Loop Initial Dispensed treats: {}", THREAD_INFO, dispensed_treats);

            // if a message was received, update the number of treats to dispense
            if msg.0 != "" {
                let from_mqtt_treats: u32 = from_utf8(msg.1).unwrap().parse().unwrap();
                if from_mqtt_treats > dispensed_treats {
                    let _ = treat_pub_channel.publish_immediate(from_mqtt_treats);
                    dispensed_treats = from_mqtt_treats;
                    println!("{} Dispensed treats: {}", THREAD_INFO, dispensed_treats);
                    println!("{} Message received: {}: {}", THREAD_INFO, msg.0, from_utf8(msg.1).unwrap());
                    continue;
                }
            }
        }
    }
}

#[main]
async fn main(spawner: Spawner) -> ! {
    const THREAD_INFO: &str = "[MAIN]:";
    esp_println::logger::init_logger_from_env();
    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM;
   
    unsafe {
        CLOCKS = Some(ClockControl::boot_defaults(system::SystemClockControl::new()).freeze());
    }

    let clocks = unsafe { CLOCKS.as_ref().unwrap() };
    let mut rtc = Rtc::new(peripherals.LPWR , None);
    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks, None);
    let mut wdt0 = timer_group0.wdt;
    let timer_group1 = TimerGroup::new(peripherals.TIMG1, &clocks, None);
    let mut wdt1 = timer_group1.wdt;
    let timer0: ErasedTimer = timer_group0.timer0.into();
    let timer = PeriodicTimer::new(timer0);
    let timer0: ErasedTimer = timer_group1.timer0.into();
    let timers = [OneShotTimer::new(timer0)];
    let timers = mk_static!([OneShotTimer<ErasedTimer>; 1], timers);
    let io = esp_hal::gpio::Io::new(peripherals.GPIO, peripherals.IO_MUX);
    let servo_pin = io.pins.gpio0;
    let mut ledc = Ledc::new(
        peripherals.LEDC,
        &clocks
    );
    
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        &clocks,
    )
    .unwrap();
    esp_hal_embassy::init(&clocks, timers);

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let config = Config::dhcpv4(Default::default());
    println!("Configuring network stack: {:?}", config);
    let seed = 26511234; // very random, very secure seed
    let resources: StackResources<16> = StackResources::<16>::new();
    let resources: &'static mut StackResources<16> = static_cell::make_static!(resources);

    let stack = Stack::new(
        wifi_interface,
        config,
        resources,
        seed
    );

    // Init network stack
    let stack = &*static_cell::make_static!(stack);

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

    loop {
        if stack.is_link_up() { break }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address); //dhcp IP address
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let post_mqtt_res = spawner.spawn(post_mqtt_message_task(&stack)).ok();
    spawner.spawn(servo_driver_task(servo_pin, ledc)).ok();
    let mut main_loop_iteration: u64 = 0;

    loop { 
        println!("{} iteration: {}", THREAD_INFO, main_loop_iteration);
        Timer::after(Duration::from_millis(5_000)).await; 
        main_loop_iteration += 1;
    }
}

pub async fn sleep(millis: u32) {
    Timer::after(Duration::from_millis(millis as u64)).await;
}
