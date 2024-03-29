mod common;

use mqttrust::{Mqtt, QoS, SubscribeTopic};
use mqttrust_core::bbqueue::BBBuffer;
use mqttrust_core::{EventLoop, MqttOptions, Notification};

use common::clock::SysClock;
use common::network::Network;
use native_tls::TlsConnector;
use std::sync::Arc;
use std::thread;

use crate::common::credentials;

static mut Q: BBBuffer<{ 1024 * 6 }> = BBBuffer::new();

fn main() {
    env_logger::init();

    let (p, c) = unsafe { Q.try_split_framed().unwrap() };

    let hostname = credentials::HOSTNAME.unwrap();

    let connector = TlsConnector::builder()
        .identity(credentials::identity())
        .add_root_certificate(credentials::root_ca())
        .build()
        .unwrap();

    let mut network = Network::new_tls(connector, String::from(hostname));

    let thing_name = "mqttrust";

    let mut mqtt_eventloop = EventLoop::new(
        c,
        SysClock::new(),
        MqttOptions::new(thing_name, hostname.into(), 8883),
    );

    let mqtt_client = mqttrust_core::Client::new(p, thing_name);

    thread::Builder::new()
        .name("eventloop".to_string())
        .spawn(move || loop {
            match nb::block!(mqtt_eventloop.connect(&mut network)) {
                Err(_) => continue,
                Ok(true) => {
                    log::info!("Successfully connected to broker");
                }
                Ok(false) => {}
            }

            match mqtt_eventloop.yield_event(&mut network) {
                Ok(Notification::Publish(_)) => {}
                Ok(n) => {
                    log::trace!("{:?}", n);
                }
                _ => {}
            }
        })
        .unwrap();

    loop {
        thread::sleep(std::time::Duration::from_millis(5000));
        mqtt_client
            .subscribe(&[SubscribeTopic {
                topic_path: format!("{}/device/advisor", thing_name).as_str(),
                qos: QoS::AtLeastOnce,
            }])
            .unwrap();

        mqtt_client
            .publish(
                format!("{}/device/advisor/hello", thing_name).as_str(),
                format!("Hello from {}", thing_name).as_bytes(),
                QoS::AtLeastOnce,
            )
            .unwrap();
    }
}
