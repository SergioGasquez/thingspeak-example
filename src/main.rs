use anyhow::bail;
use core::str;
use embedded_svc::mqtt::client::{Connection, Event::*, Message, Publish, QoS};
use embedded_svc::wifi::{
    self, AuthMethod, ClientConfiguration, ClientConnectionStatus, ClientIpStatus, ClientStatus,
    Wifi as _,
};
use esp_idf_hal::{i2c, peripherals::Peripherals, prelude::*};
use esp_idf_svc::{
    mqtt::client::{EspMqttClient, MqttClientConfiguration},
    netif::EspNetifStack,
    nvs::EspDefaultNvs,
    sysloop::EspSysLoopStack,
    wifi::EspWifi,
};
use esp_idf_sys as _;
use log::info;
use serde::{Deserialize, Serialize};
use shared_bus::BusManagerSimple;
use shtcx::{shtc3, PowerMode};
use std::{sync::Arc, thread, thread::sleep, time::Duration};

const THINGSPEAK_BROKER_URL: &str = "mqtt://mqtt3.thingspeak.com:1883";

#[derive(Serialize, Deserialize)]
struct Data {
    tempareture: f32,
    humidity: f32,
}

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    #[default("")]
    client_id: &'static str,
    #[default("")]
    username: &'static str,
    #[default("")]
    password: &'static str,
    #[default("")]
    channel_id: &'static str,
}

#[allow(unused)]
pub struct Wifi {
    esp_wifi: EspWifi,
    netif_stack: Arc<EspNetifStack>,
    sys_loop_stack: Arc<EspSysLoopStack>,
    default_nvs: Arc<EspDefaultNvs>,
}

fn main() -> anyhow::Result<()> {
    esp_idf_sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let i2c = peripherals.i2c0;
    let sda = peripherals.pins.gpio10;
    let scl = peripherals.pins.gpio8;

    let config = <i2c::config::MasterConfig as Default>::default().baudrate(400.kHz().into());
    let i2c = i2c::Master::<i2c::I2C0, _, _>::new(i2c, i2c::MasterPins { sda, scl }, config)?;
    let bus = BusManagerSimple::new(i2c);
    let mut sht = shtc3(bus.acquire_i2c());
    sht.start_measurement(PowerMode::NormalMode).unwrap();

    let app_config = CONFIG;
    let topic_name = format!("channels/{}/publish", app_config.channel_id);
    info!(
        "About to initialize WiFi (SSID: {}, PASS: {})",
        app_config.wifi_ssid, app_config.wifi_pass
    );

    let _wifi = wifi(app_config.wifi_ssid, app_config.wifi_pass)?;

    println!("Hello, world!");

    let conf = MqttClientConfiguration {
        client_id: Some(app_config.client_id),
        username: Some(app_config.username),
        password: Some(app_config.password),
        keep_alive_interval: Some(Duration::from_secs(120)),
        ..Default::default()
    };
    let (mut client, mut connection) = EspMqttClient::new_with_conn(THINGSPEAK_BROKER_URL, &conf)?;
    info!("Connected");
    thread::spawn(move || {
        info!("MQTT Listening for messages");

        while let Some(msg) = connection.next() {
            match msg {
                Err(e) => info!("MQTT Message ERROR: {}", e),
                Ok(message) => match message {
                    Received(recieved_bytes) => match str::from_utf8(recieved_bytes.data()) {
                        Err(e) => info!("MQTT Error : unreadable message! ({})", e),
                        Ok(measurements) => info!("MQTT Message : {}", measurements),
                    },
                    BeforeConnect => info!("MQTT Message : Before connect"),
                    Connected(tf) => info!("MQTT Message : Connected({})", tf),
                    Disconnected => info!("MQTT Message : Disconnected"),
                    Subscribed(message_id) => info!("MQTT Message : Subscribed({})", message_id),
                    Unsubscribed(message_id) => {
                        info!("MQTT Message : Unsubscribed({})", message_id)
                    }
                    Published(message_id) => info!("MQTT Message : Published({})", message_id),
                    Deleted(message_id) => info!("MQTT Message : Deleted({})", message_id),
                },
            }
        }
        info!("MQTT connection loop exit");
    });
    loop {
        let measurement = sht.get_measurement_result().unwrap();
        let message = format!(
            "field1={}&field2={}&status=MQTTPUBLISH",
            measurement.temperature.as_degrees_celsius(),
            measurement.humidity.as_percent()
        );

        // let message = format!("{}", measurement.temperature.as_degrees_celsius());
        client.publish(&topic_name, QoS::AtMostOnce, false, message.as_bytes())?;
        sht.start_measurement(PowerMode::NormalMode).unwrap();
        sleep(Duration::from_millis(1000));
    }
}

pub fn wifi(ssid: &str, psk: &str) -> anyhow::Result<Wifi> {
    let mut auth_method = AuthMethod::WPA2Personal;
    if ssid.is_empty() {
        anyhow::bail!("missing WiFi name")
    }
    if psk.is_empty() {
        auth_method = AuthMethod::None;
        info!("Wifi password is empty");
    }
    let netif_stack = Arc::new(EspNetifStack::new()?);
    let sys_loop_stack = Arc::new(EspSysLoopStack::new()?);
    let default_nvs = Arc::new(EspDefaultNvs::new()?);
    let mut wifi = EspWifi::new(
        netif_stack.clone(),
        sys_loop_stack.clone(),
        default_nvs.clone(),
    )?;

    info!("Searching for Wifi network {}", ssid);

    let ap_infos = wifi.scan()?;

    let ours = ap_infos.into_iter().find(|a| a.ssid == ssid);

    let channel = if let Some(ours) = ours {
        info!(
            "Found configured access point {} on channel {}",
            ssid, ours.channel
        );
        Some(ours.channel)
    } else {
        info!(
            "Configured access point {} not found during scanning, will go with unknown channel",
            ssid
        );
        None
    };

    info!("setting Wifi configuration");
    wifi.set_configuration(&wifi::Configuration::Client(ClientConfiguration {
        ssid: ssid.into(),
        password: psk.into(),
        channel,
        auth_method,
        ..Default::default()
    }))?;

    info!("getting Wifi status");

    wifi.wait_status_with_timeout(Duration::from_secs(2100), |status| {
        !status.is_transitional()
    })
    .map_err(|err| anyhow::anyhow!("Unexpected Wifi status (Transitional state): {:?}", err))?;

    let status = wifi.get_status();

    if let wifi::Status(
        ClientStatus::Started(ClientConnectionStatus::Connected(ClientIpStatus::Done(
            _ip_settings,
        ))),
        _,
    ) = status
    {
        info!("Wifi connected");
    } else {
        bail!(
            "Could not connect to Wifi - Unexpected Wifi status: {:?}",
            status
        );
    }

    let wifi = Wifi {
        esp_wifi: wifi,
        netif_stack,
        sys_loop_stack,
        default_nvs,
    };

    Ok(wifi)
}
