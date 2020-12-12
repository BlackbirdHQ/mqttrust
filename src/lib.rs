#![no_std]

mod client;
mod options;
mod requests;
mod state;

use no_std_net::SocketAddr;
use state::{MqttState, StateError};

use core::convert::TryFrom;

use embedded_nal::{AddrType, Dns, TcpClient};
use heapless::{consts, spsc, ArrayLength, String, Vec};
use mqttrs::{decode_slice, encode_slice, Pid, Suback};

pub use client::{Mqtt, MqttClient, MqttClientError};
pub use mqttrs::{
    Connect, Packet, Protocol, Publish, QoS, QosPid, Subscribe, SubscribeReturnCodes,
    SubscribeTopic, Unsubscribe,
};
pub use options::{Broker, MqttOptions};
pub use requests::{PublishPayload, PublishRequest, Request, SubscribeRequest, UnsubscribeRequest};

#[derive(Debug, PartialEq)]
pub struct PublishNotification {
    pub dup: bool,
    pub qospid: QosPid,
    pub retain: bool,
    pub topic_name: String<consts::U256>,
    pub payload: Vec<u8, consts::U2048>,
}

/// Includes incoming packets from the network and other interesting events
/// happening in the eventloop
#[derive(Debug, PartialEq)]
pub enum Notification {
    /// Incoming publish from the broker
    Publish(PublishNotification),
    /// Incoming puback from the broker
    Puback(Pid),
    /// Incoming pubrec from the broker
    Pubrec(Pid),
    /// Incoming pubcomp from the broker
    Pubcomp(Pid),
    /// Incoming suback from the broker
    Suback(Suback),
    /// Incoming unsuback from the broker
    Unsuback(Pid),
    // Eventloop error
    Abort(EventError),
}

impl<'a> TryFrom<Publish<'a>> for PublishNotification {
    type Error = StateError;

    fn try_from(p: Publish<'a>) -> Result<Self, Self::Error> {
        Ok(PublishNotification {
            dup: p.dup,
            qospid: p.qospid,
            retain: p.retain,
            topic_name: String::from(p.topic_name),
            payload: Vec::from_slice(p.payload).map_err(|_| StateError::PayloadEncoding)?,
        })
    }
}

/// Critical errors during eventloop polling
#[derive(Debug, PartialEq)]
pub enum EventError {
    MqttState(StateError),
    Timeout,
    Encoding(mqttrs::Error),
    Network(NetworkError),
    BufferSize,
}

#[derive(Debug, PartialEq, defmt::Format)]
pub enum NetworkError {
    Read,
    Write,
    NoSocket,
    SocketOpen,
    SocketConnect,
    SocketClosed,
    DnsLookupFailed,
}

impl From<mqttrs::Error> for EventError {
    fn from(e: mqttrs::Error) -> Self {
        EventError::Encoding(e)
    }
}

impl From<StateError> for EventError {
    fn from(e: StateError) -> Self {
        EventError::MqttState(e)
    }
}

pub struct MqttEvent<'a, 'b, L, S, O, P>
where
    L: ArrayLength<Request<P>>,
    P: PublishPayload,
{
    /// Current state of the connection
    pub state: MqttState<O, P>,
    /// Options of the current mqtt connection
    pub options: MqttOptions<'b>,
    /// Network socket
    pub socket: Option<S>,
    /// Request stream
    pub requests: spsc::Consumer<'a, Request<P>, L, u8>,
    // Outgoing QoS 1, 2 publishes which aren't acked yet
    // pending_pub: FnvIndexMap<u16, PublishRequest<P>, consts::U3>,
    // Packet ids of released QoS 2 publishes
    // pending_rel: FnvIndexSet<u16, consts::U4>,
}

impl<'a, 'b, L, S, O, P> MqttEvent<'a, 'b, L, S, O, P>
where
    L: ArrayLength<Request<P>>,
    O: embedded_hal::timer::CountDown,
    O::Time: From<u32>,
    P: PublishPayload + Clone,
{
    pub fn new(
        requests: spsc::Consumer<'a, Request<P>, L, u8>,
        outgoing_timer: O,
        options: MqttOptions<'b>,
    ) -> Self {
        MqttEvent {
            state: MqttState::new(outgoing_timer),
            options,
            socket: None,
            requests,
            // pending_pub: IndexMap::new(),
            // pending_rel: IndexSet::new(),
        }
    }

    pub fn connect<N: Dns + TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
    ) -> nb::Result<bool, EventError> {
        // connect to the broker
        self.network_connect(network)?;
        if self.mqtt_connect(network)? {
            // Handle state after reconnect events
            // self.populate_pending();
            return Ok(true);
        }

        Ok(false)
    }

    fn should_handle_request(&mut self) -> bool {
        let qos_space = self.state.outgoing_pub.len() < self.options.inflight();

        let qos_0 = if let Some(Request::Publish(p)) = self.requests.peek() {
            p.qos == QoS::AtMostOnce
        } else {
            false
        };

        if qos_0 {
            true
        } else {
            self.requests.ready() && qos_space
        }
    }

    pub fn yield_event<N: TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
    ) -> nb::Result<Notification, ()> {
        let packet_buf = &mut [0u8; 1024];

        let o = if self.should_handle_request() {
            // Handle requests
            let request = unsafe { self.requests.dequeue_unchecked() };
            self.state.handle_outgoing_request(request, packet_buf)
        } else if let Some(packet) = match self.receive(network, packet_buf) {
            Ok(p) => p,
            Err(EventError::Encoding(e)) => {
                defmt::debug!("Encoding error!");
                return Ok(Notification::Abort(e.into()));
            }
            Err(e) => {
                defmt::debug!("Disconnecting from receive error!");
                self.disconnect(network);
                return Ok(Notification::Abort(e));
            }
        } {
            // Handle incoming
            self.state.handle_incoming_packet(packet)
        // } else if let Some(p) = self.get_pending_rel() {
        //     // Handle pending PubRec
        //     self.state.handle_outgoing_packet(Packet::Pubrec(p))
        // } else if let Some(publish) = self.get_pending_pub() {
        //     // Handle pending Publish
        //     self.state
        //         .handle_outgoing_request(publish.into(), packet_buf)
        } else if self.state.last_outgoing_timer.try_wait().is_ok() {
            // Handle ping
            self.state.handle_outgoing_packet(Packet::Pingreq)
        } else {
            Ok((None, None))
        };

        let (notification, outpacket) = match o {
            Ok((n, p)) => (n, p),
            Err(e) => {
                defmt::debug!("Disconnecting from handling error!");
                self.disconnect(network);
                return Ok(Notification::Abort(e.into()));
            }
        };

        if let Some(p) = outpacket {
            if let Err(e) = self.send(network, p) {
                defmt::debug!("Disconnecting from send error!");
                self.disconnect(network);
                return Ok(Notification::Abort(e));
            } else {
                self.state
                    .last_outgoing_timer
                    .try_start(self.options.keep_alive_ms())
                    .ok();
            }
        }

        if let Some(n) = notification {
            Ok(n)
        } else {
            Err(nb::Error::WouldBlock)
        }
    }

    // pub fn get_pending_rel(&mut self) -> Option<Pid> {
    //     let p = match self.pending_rel.iter().next() {
    //         Some(p) => *p,
    //         None => return None,
    //     };
    //     self.pending_rel.remove(&p);
    //     Pid::try_from(p).ok()
    // }

    // pub fn get_pending_pub(&mut self) -> Option<PublishRequest<P>> {
    //     let pid = match self.pending_pub.keys().next() {
    //         Some(p) => *p,
    //         None => return None,
    //     };
    //     self.pending_pub.remove(&pid)
    // }

    // fn populate_pending(&mut self) {
    //     let pending_pub = core::mem::replace(&mut self.state.outgoing_pub, IndexMap::new());

    //     defmt::info!("Populating pending publish: {:?}", pending_pub.len());

    //     self.pending_pub
    //         .extend(pending_pub.iter().map(|(key, value)| (*key, value.clone())));

    //     let pending_rel = core::mem::replace(&mut self.state.outgoing_rel, IndexSet::new());

    //     defmt::info!("populating pending rel: {:?}", pending_rel.len());

    //     self.pending_rel.extend(pending_rel.iter());
    // }

    pub fn send<'d, N: TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
        pkt: Packet<'d>,
    ) -> Result<usize, EventError> {
        match self.socket {
            Some(ref mut socket) => {
                let mut tx_buf: [u8; 1024] = [0; 1024];
                let size = encode_slice(&pkt, &mut tx_buf)?;
                nb::block!(network.send(socket, &tx_buf[..size])).map_err(|_| {
                    defmt::error!("[send] NetworkError::Write");
                    EventError::Network(NetworkError::Write)
                })
            }
            _ => Err(EventError::Network(NetworkError::NoSocket)),
        }
    }

    pub fn receive<'d, N: TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
        packet_buf: &'d mut [u8],
    ) -> Result<Option<Packet<'d>>, EventError> {
        match self.socket {
            Some(ref mut socket) => {
                match network.receive(socket, packet_buf) {
                    Ok(0) | Err(nb::Error::WouldBlock) => Ok(None),
                    Ok(size) => {
                        let p = decode_slice(&packet_buf[..size]).map_err(EventError::Encoding);

                        // if let Ok(Some(Packet::Puback(pid))) = p {
                        //     defmt::info!("Got Puback! {:?}", pid.get());
                        // }
                        p
                    }
                    _ => Err(EventError::Network(NetworkError::Read)),
                }
            }
            _ => Err(EventError::Network(NetworkError::NoSocket)),
        }
    }

    fn lookup_host<N: Dns + TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
    ) -> Result<(heapless::String<heapless::consts::U256>, SocketAddr), EventError> {
        match self.options.broker() {
            (Broker::Hostname(h), p) => {
                let socket_addr = SocketAddr::new(
                    network.gethostbyname(h, AddrType::IPv4).map_err(|_e| {
                        defmt::info!("Failed to resolve IP!");
                        EventError::Network(NetworkError::DnsLookupFailed)
                    })?,
                    p,
                );
                Ok((heapless::String::from(h), socket_addr))
            }
            (Broker::IpAddr(ip), p) => {
                let socket_addr = SocketAddr::new(ip, p);
                let domain = network.gethostbyaddr(ip).map_err(|_e| {
                    defmt::info!("Failed to resolve hostname!");
                    EventError::Network(NetworkError::DnsLookupFailed)
                })?;

                Ok((domain, socket_addr))
            }
        }
    }

    fn network_connect<N: Dns + TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
    ) -> Result<(), EventError> {
        if let Some(socket) = &self.socket {
            match network.is_connected(socket) {
                Ok(true) => return Ok(()),
                Err(_e) => {
                    self.socket = None;
                    return Err(EventError::Network(NetworkError::SocketClosed));
                }
                Ok(false) => {}
            }
        };

        self.state.connection_status = state::MqttConnectionStatus::Disconnected;

        self.socket = network
            .socket()
            .map_err(|_e| EventError::Network(NetworkError::SocketOpen))?
            .into();

        match self.lookup_host(network) {
            Ok((_hostname, socket_addr)) => {
                Some(
                    network
                        .connect(
                            self.socket.as_mut().unwrap_or_else(|| unreachable!()),
                            socket_addr,
                        )
                        .map_err(|_e| EventError::Network(NetworkError::SocketConnect))?,
                );

                // if let Some(root_ca) = self.options.ca() {
                //     // Add root CA
                // };

                // if let Some((certificate, private_key)) = self.options.client_auth() {
                //     // Enable SSL for self.socket, with broker (hostname)
                // };

                defmt::debug!("Network connected!");

                Ok(())
            }
            Err(e) => {
                // Make sure to cleanup socket, in case we fail DNS lookup for some reason
                self.disconnect(network);
                Err(e)
            }
        }
    }

    pub fn disconnect<N: TcpClient<TcpSocket = S>>(&mut self, network: &N) {
        self.state.connection_status = state::MqttConnectionStatus::Disconnected;
        if let Some(socket) = self.socket.take() {
            network.close(socket).ok();
        }
    }

    fn mqtt_connect<N: TcpClient<TcpSocket = S>>(
        &mut self,
        network: &N,
    ) -> nb::Result<bool, EventError> {
        match self.state.connection_status {
            state::MqttConnectionStatus::Connected => Ok(false),
            state::MqttConnectionStatus::Disconnected => {
                defmt::info!("MQTT connecting..");
                self.state.await_pingresp = false;
                self.state.outgoing_pub.clear();

                let (username, password) = self.options.credentials();

                let connect = Connect {
                    protocol: Protocol::MQTT311,
                    keep_alive: (self.options.keep_alive_ms() / 1000) as u16,
                    client_id: self.options.client_id(),
                    clean_session: self.options.clean_session(),
                    last_will: self.options.last_will(),
                    username,
                    password,
                };

                // mqtt connection with timeout
                match self.send(network, connect.into()) {
                    Ok(_) => {
                        self.state
                            .handle_outgoing_connect()
                            .map_err(|e| nb::Error::Other(e.into()))?;

                        self.state.last_outgoing_timer.try_start(50000).ok();
                    }
                    Err(e) => {
                        defmt::debug!("Disconnecting from send error!");
                        self.disconnect(network);
                        return Err(nb::Error::Other(e));
                    }
                }

                Err(nb::Error::WouldBlock)
            }
            state::MqttConnectionStatus::Handshake => {
                if self.state.last_outgoing_timer.try_wait().is_ok() {
                    defmt::debug!("Disconnecting from handshake timeout!");
                    self.disconnect(network);
                    return Err(nb::Error::Other(EventError::Timeout));
                }

                let packet_buf = &mut [0u8; 4];
                match self.receive(network, packet_buf) {
                    Ok(Some(packet)) => {
                        self.state
                            .handle_incoming_connack(packet)
                            .map_err(|e| nb::Error::Other(e.into()))?;

                        defmt::debug!("MQTT connected!");

                        Ok(true)
                    }
                    Ok(None) => Err(nb::Error::WouldBlock),
                    Err(e) => Err(nb::Error::Other(e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal::timer::CountDown;
    use heapless::{consts, spsc::Queue, String, Vec};
    use mqttrs::{Connack, ConnectReturnCode};

    #[derive(Debug)]
    struct CdMock {
        time: u32,
    }

    impl CountDown for CdMock {
        type Error = core::convert::Infallible;
        type Time = u32;
        fn try_start<T>(&mut self, count: T) -> Result<(), Self::Error>
        where
            T: Into<Self::Time>,
        {
            self.time = count.into();
            Ok(())
        }
        fn try_wait(&mut self) -> nb::Result<(), Self::Error> {
            Ok(())
        }
    }

    struct MockNetwork {
        pub should_fail_read: bool,
        pub should_fail_write: bool,
    }

    impl Dns for MockNetwork {
        type Error = ();

        fn gethostbyname(
            &self,
            _hostname: &str,
            _addr_type: embedded_nal::AddrType,
        ) -> Result<embedded_nal::IpAddr, Self::Error> {
            unimplemented!()
        }
        fn gethostbyaddr(
            &self,
            _addr: embedded_nal::IpAddr,
        ) -> Result<heapless::String<consts::U256>, Self::Error> {
            unimplemented!()
        }
    }

    impl TcpClient for MockNetwork {
        type TcpSocket = ();
        type Error = ();

        fn socket(&self) -> Result<Self::TcpSocket, Self::Error> {
            Ok(())
        }

        fn connect(
            &self,
            _socket: &mut Self::TcpSocket,
            _remote: embedded_nal::SocketAddr,
        ) -> nb::Result<(), Self::Error> {
            Ok(())
        }

        fn is_connected(&self, _socket: &Self::TcpSocket) -> Result<bool, Self::Error> {
            Ok(true)
        }

        fn send(
            &self,
            _socket: &mut Self::TcpSocket,
            buffer: &[u8],
        ) -> nb::Result<usize, Self::Error> {
            if self.should_fail_write {
                Err(nb::Error::Other(()))
            } else {
                Ok(buffer.len())
            }
        }

        fn receive(
            &self,
            _socket: &mut Self::TcpSocket,
            buffer: &mut [u8],
        ) -> nb::Result<usize, Self::Error> {
            if self.should_fail_read {
                Err(nb::Error::Other(()))
            } else {
                let connack = Packet::Connack(Connack {
                    session_present: false,
                    code: ConnectReturnCode::Accepted,
                });
                let size = encode_slice(&connack, buffer).unwrap();
                Ok(size)
            }
        }

        fn close(&self, _socket: Self::TcpSocket) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    #[ignore]
    fn retry_behaviour() {
        static mut Q: Queue<Request<Vec<u8, consts::U10>>, consts::U5, u8> =
            Queue(heapless::i::Queue::u8());

        let network = MockNetwork {
            should_fail_read: false,
            should_fail_write: false,
        };

        let (_p, c) = unsafe { Q.split() };
        let mut event = MqttEvent::<_, (), _, _>::new(
            c,
            CdMock { time: 0 },
            MqttOptions::new("client", Broker::Hostname(""), 8883),
        );

        event
            .state
            .outgoing_pub
            .insert(
                2,
                PublishRequest {
                    dup: false,
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    topic_name: String::from("some/topic/name2"),
                    payload: Vec::new(),
                },
            )
            .unwrap();

        event
            .state
            .outgoing_pub
            .insert(
                3,
                PublishRequest {
                    dup: false,
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    topic_name: String::from("some/topic/name3"),
                    payload: Vec::new(),
                },
            )
            .unwrap();

        event
            .state
            .outgoing_pub
            .insert(
                4,
                PublishRequest {
                    dup: false,
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    topic_name: String::from("some/topic/name4"),
                    payload: Vec::new(),
                },
            )
            .unwrap();

        event.state.connection_status = state::MqttConnectionStatus::Handshake;
        event.socket = Some(());

        event.connect(&network).unwrap();

        // assert_eq!(event.pending_pub.len(), 3);

        // let mut key_iter = event.pending_pub.keys();
        // assert_eq!(key_iter.next(), Some(&2));
        // assert_eq!(key_iter.next(), Some(&3));
        // assert_eq!(key_iter.next(), Some(&4));
    }
}
