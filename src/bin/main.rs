// generator version: 1.4.0
// generator parameters: -o unstable-hal -o alloc -o zed -o embassy -o esp32 -o wifi

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

// Explict import
extern crate alloc;

use embassy_executor::Spawner;
use embassy_net::{Ipv4Address, Runner, StackResources, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::Write;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::DriveMode,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController,
    scan::ScanConfig, sta::StationConfig,
};
use rust_mqtt::{
    buffer::AllocBuffer,
    client::{
        Client,
        event::Event,
        options::{ConnectOptions, SubscriptionOptions, TopicReference},
    },
    types::{MqttBinary, MqttString, TopicFilter},
};

// Creates safe references at runtime
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// An HTML to show in the web server
const PAGE: &str = include_str!("../../assets/index.html");

// Save the RGB status
static COLOR: Signal<CriticalSectionRawMutex, [u8; 3]> = Signal::new();

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // Logger
    esp_println::logger::init_logger_from_env();

    // Peripherals
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Set memory
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);

    // Initialize the task manager
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // START: Network / WiFi stack
    println!("Starting wifi...");
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(env!("SSID").try_into().unwrap())
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                env!("PASSWORD").try_into().unwrap(),
            )),
    );

    let wifi_interface = esp_radio::wifi::Interface::station();
    let mut controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap();

    println!("Wifi started!");

    // Get IP
    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<10>, StackResources::<10>::new()),
        seed,
    );

    println!("Scanning networks...");

    let scan_config = ScanConfig::default().with_max(10);
    let result = controller.scan_async(&scan_config).await.unwrap();
    for ap in result {
        println!("Found access point {:?}", ap);
    }

    // Task to control the network stack
    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    println!("Connecting...");
    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        println!("Got IP: {}", config.address);
    }
    // START: Network / WiFi stack

    // Control http and mqtt requests
    spawner.spawn(http_task(stack).unwrap());
    spawner.spawn(mqtt_task(stack).unwrap());

    // Hardware management
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut timer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    timer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(5),
        })
        .unwrap();

    // Physical pines
    let mut red = ledc.channel(channel::Number::Channel0, peripherals.GPIO14);
    let mut green = ledc.channel(channel::Number::Channel1, peripherals.GPIO12);
    let mut blue = ledc.channel(channel::Number::Channel2, peripherals.GPIO27);

    // Set the pines in push pull
    for ch in [&mut red, &mut green, &mut blue] {
        ch.configure(channel::config::Config {
            timer: &timer0,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();
    }

    loop {
        // Reads for changes in the state of the RGB, then set the values to the pines
        let [r, g, b] = COLOR.wait().await;
        red.set_duty((r as u16 * 100 / 255) as u8).unwrap();
        green.set_duty((g as u16 * 100 / 255) as u8).unwrap();
        blue.set_duty((b as u16 * 100 / 255) as u8).unwrap();
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("Start connection task");

    loop {
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(info) => {
                println!("Wifi connected to {:?}", info);

                // Wait until we're no longer connected
                let info = controller.wait_for_disconnect_async().await.ok();
                println!("Disconnected: {:?}", info);
            }
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
            }
        }

        Timer::after(Duration::from_millis(5000)).await
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

#[embassy_executor::task]
async fn mqtt_task(stack: embassy_net::Stack<'static>) {
    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 5012];
    let mut backoff_s: u64 = 1;

    loop {
        println!("MQTT connecting...");

        // Wait until WiFi responds
        stack.wait_config_up().await;

        match mqtt_session(stack, &mut rx_buf, &mut tx_buf, &mut backoff_s).await {
            Ok(()) => println!("MQTT session terminated"),
            Err(e) => println!("MQTT session error: {}", e),
        }

        println!("MQTT re-trying connection in {}seg", backoff_s);
        Timer::after(Duration::from_secs(backoff_s)).await;
        backoff_s = (backoff_s * 2).min(30);
    }
}

async fn mqtt_session(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    backoff_s: &mut u64,
) -> Result<(), &'static str> {
    let broker = (Ipv4Address::new(192, 168, 0, 163), 1883);

    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
    socket.set_timeout(Some(Duration::from_secs(20)));
    socket.set_keep_alive(Some(Duration::from_secs(15)));

    with_timeout(Duration::from_secs(5), socket.connect(broker))
        .await
        .map_err(|_| "MQTT TCP timeuot error")?
        .map_err(|_| "MQTT session error")?;

    let mut buffer = AllocBuffer;
    let mut client = Client::<'_, '_, _, _, 1, 1, 1, 0, 1, 0, 0>::new(&mut buffer);

    let opts = ConnectOptions::new()
        .clean_start()
        .user_name(MqttString::from_str(env!("MQTT_USER")).unwrap())
        .password(MqttBinary::from_slice(env!("MQTT_PASS").as_bytes()).unwrap());

    client
        .connect(
            socket,
            &opts,
            Some(MqttString::from_str("lampara-dormitorio").unwrap()),
        )
        .await
        .map_err(|e| {
            println!("Client lampara-dormitorio connect: {:?}", e);
            "Client lampara-dormitorio MQTT connect error"
        })?;

    // Suscripción única usando comodín (#) para capturar todo el árbol de la lámpara
    let topic_wildcard = MqttString::from_str("lampara/#").unwrap();
    client
        .subscribe(
            TopicFilter::new(topic_wildcard.as_borrowed()).unwrap(),
            &SubscriptionOptions::new(),
        )
        .await
        .map_err(|e| {
            println!("Subscribe lampara/#: {:?}", e);
            "subscribe wildcard error"
        })?;

    println!("MQTT up and listening lampara/#");
    *backoff_s = 1;

    // Save last color
    let mut last_color = [255, 255, 255];

    loop {
        match client.poll().await {
            Ok(Event::Publish(publication)) => {
                let payload_str = core::str::from_utf8(publication.message.as_ref());

                let topic_str = match &publication.topic {
                    TopicReference::Name(name) => name.as_ref().as_str(),
                    _ => "",
                };

                match topic_str {
                    "lampara/set" => {
                        println!("lampara/set: {:?}", payload_str.ok());
                        match payload_str.ok().and_then(parse_rgb) {
                            Some(rgb) => {
                                println!("lampara/set: {:?}", rgb);
                                last_color = rgb;
                                COLOR.signal(rgb);
                            }
                            None => {
                                println!("RGB value not valid: {:?}", payload_str)
                            }
                        }
                    }
                    "lampara/switch" => {
                        println!("lampara/switch: {:?}", payload_str.ok());
                        match payload_str {
                            Ok("ON") => {
                                COLOR.signal(last_color);
                            }
                            Ok("OFF") => {
                                COLOR.signal([0, 0, 0]);
                            }
                            Ok(other) => println!("Switch value not recognized: {}", other),
                            Err(_) => println!("Switch payload error"),
                        }
                    }
                    _ => {
                        println!("Unknown topic: {}", topic_str);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                println!("Poll error: {:?}", e);
                return Err("Poll error");
            }
        }
    }
}

#[embassy_executor::task(pool_size = 3)]
async fn http_task(stack: embassy_net::Stack<'static>) {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 2048];
    let mut buf = [0u8; 512];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        if socket.accept(80).await.is_err() {
            continue;
        }
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let req = core::str::from_utf8(&buf[..n]).unwrap_or("");

        let body = PAGE.as_bytes();

        println!("Getting a request: {:?}", req);
        match parse_request_line(req) {
            Some(("GET", "/set", query)) => {
                let r = query_param(query, "r").and_then(|v| v.parse::<u8>().ok());
                let g = query_param(query, "g").and_then(|v| v.parse::<u8>().ok());
                let b = query_param(query, "b").and_then(|v| v.parse::<u8>().ok());

                if let (Some(r), Some(g), Some(b)) = (r, g, b) {
                    println!("Color: {} {} {}", r, g, b);
                    COLOR.signal([r, g, b]);
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                        .await;
                }
            }
            Some(("GET", "/", _)) => {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = socket.write_all(body).await;
            }
            _ => {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
                    .await;
            }
        }
        let _ = socket.flush().await;

        println!("Response sent, closing socket");
        socket.close();
        embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;

        println!("Socket closed")
    }
}

// "GET /set?r=1&g=2 HTTP/1.1" -> ("GET", "/set", "r=1&g=2")
fn parse_request_line(req: &str) -> Option<(&str, &str, &str)> {
    let line = req.lines().next()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?; // "/set?r=1&g=2"
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some((method, path, query))
}

// Parse query params
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// "255,100,0" -> [255, 100, 0]
fn parse_rgb(s: &str) -> Option<[u8; 3]> {
    let mut it = s.trim().split(',');
    let r = it.next()?.trim().parse::<u8>().ok()?;
    let g = it.next()?.trim().parse::<u8>().ok()?;
    let b = it.next()?.trim().parse::<u8>().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some([r, g, b])
}
