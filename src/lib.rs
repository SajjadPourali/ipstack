#![doc = include_str!("../README.md")]

use crate::stream::{IpStackStream, IpStackTcpStream, IpStackUdpStream, IpStackUnknownTransport};
use ahash::AHashMap;
use packet::{NetworkPacket, NetworkTuple, TransportHeader};
use std::{
    collections::hash_map::Entry::{Occupied, Vacant},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};

pub(crate) type PacketSender = UnboundedSender<NetworkPacket>;
pub(crate) type PacketReceiver = UnboundedReceiver<NetworkPacket>;
pub(crate) type SessionCollection = std::sync::Arc<tokio::sync::Mutex<AHashMap<NetworkTuple, PacketSender>>>;

mod error;
mod packet;
pub mod stream;

pub use self::error::{IpStackError, Result};
pub use ::etherparse::IpNumber;

const DROP_TTL: u8 = 0;

#[cfg(unix)]
const TTL: u8 = 64;

#[cfg(windows)]
const TTL: u8 = 128;

#[cfg(unix)]
const TUN_FLAGS: [u8; 2] = [0x00, 0x00];

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
const TUN_PROTO_IP6: [u8; 2] = [0x86, 0xdd];
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
const TUN_PROTO_IP4: [u8; 2] = [0x08, 0x00];

#[cfg(any(target_os = "macos", target_os = "ios"))]
const TUN_PROTO_IP6: [u8; 2] = [0x00, 0x0A];
#[cfg(any(target_os = "macos", target_os = "ios"))]
const TUN_PROTO_IP4: [u8; 2] = [0x00, 0x02];

pub struct IpStackConfig {
    pub mtu: u16,
    pub packet_information: bool,
    pub tcp_timeout: Duration,
    pub udp_timeout: Duration,
}

impl Default for IpStackConfig {
    fn default() -> Self {
        IpStackConfig {
            mtu: u16::MAX,
            packet_information: false,
            tcp_timeout: Duration::from_secs(60),
            udp_timeout: Duration::from_secs(30),
        }
    }
}

impl IpStackConfig {
    pub fn tcp_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.tcp_timeout = timeout;
        self
    }
    pub fn udp_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.udp_timeout = timeout;
        self
    }
    pub fn mtu(&mut self, mtu: u16) -> &mut Self {
        self.mtu = mtu;
        self
    }
    pub fn packet_information(&mut self, packet_information: bool) -> &mut Self {
        self.packet_information = packet_information;
        self
    }
}

pub struct IpStack {
    accept_receiver: UnboundedReceiver<IpStackStream>,
    pub handle: JoinHandle<Result<()>>,
}

impl IpStack {
    pub fn new<Device>(config: IpStackConfig, device: Device) -> IpStack
    where
        Device: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (accept_sender, accept_receiver) = mpsc::unbounded_channel::<IpStackStream>();
        IpStack {
            accept_receiver,
            handle: run(config, device, accept_sender),
        }
    }

    pub async fn accept(&mut self) -> Result<IpStackStream, IpStackError> {
        self.accept_receiver.recv().await.ok_or(IpStackError::AcceptError)
    }
}

fn run<Device: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    config: IpStackConfig,
    mut device: Device,
    accept_sender: UnboundedSender<IpStackStream>,
) -> JoinHandle<Result<()>> {
    let sessions: SessionCollection = std::sync::Arc::new(tokio::sync::Mutex::new(AHashMap::new()));
    let pi = config.packet_information;
    let offset = if pi && cfg!(unix) { 4 } else { 0 };
    let mut buffer = [0_u8; u16::MAX as usize + 4];
    let (up_pkt_sender, mut up_pkt_receiver) = mpsc::unbounded_channel::<NetworkPacket>();

    tokio::spawn(async move {
        loop {
            select! {
                Ok(n) = device.read(&mut buffer) => {
                    if let Err(e) = process_device_read(
                        &buffer[offset..n],
                        sessions.clone(),
                        up_pkt_sender.clone(),
                        &config,
                        &accept_sender,
                    ).await  {
                        log::debug!("process_device_read error: {}", e);
                    }
                }
                Some(packet) = up_pkt_receiver.recv() => {
                    process_upstream_recv(
                        packet,
                        sessions.clone(),
                        &mut device,
                        #[cfg(unix)]
                        pi,
                    )
                    .await?;
                }
            }
        }
    })
}

async fn process_device_read(
    data: &[u8],
    sessions: SessionCollection,
    up_pkt_sender: PacketSender,
    config: &IpStackConfig,
    accept_sender: &UnboundedSender<IpStackStream>,
) -> Result<()> {
    let Ok(packet) = NetworkPacket::parse(data) else {
        let stream = IpStackStream::UnknownNetwork(data.to_owned());
        accept_sender.send(stream)?;
        return Ok(());
    };

    if let TransportHeader::Unknown = packet.transport_header() {
        let stream = IpStackStream::UnknownTransport(IpStackUnknownTransport::new(
            packet.src_addr().ip(),
            packet.dst_addr().ip(),
            packet.payload,
            &packet.ip,
            config.mtu,
            up_pkt_sender,
        ));
        accept_sender.send(stream)?;
        return Ok(());
    }

    let sessions_clone = sessions.clone();
    let network_tuple = packet.network_tuple();
    match sessions.lock().await.entry(network_tuple) {
        Occupied(entry) => {
            use std::io::{Error, ErrorKind::Other};
            entry.get().send(packet).map_err(|e| Error::new(Other, e))?;
            log::trace!("packet sent to stream: {}", network_tuple);
        }
        Vacant(entry) => {
            let (packet_sender, mut ip_stack_stream) = create_stream(packet, config, up_pkt_sender)?;
            if let IpStackStream::Udp(ref mut stream) = ip_stack_stream {
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                stream.set_destroy_messenger(tx);
                tokio::spawn(async move {
                    rx.await.ok();
                    sessions_clone.lock().await.remove(&network_tuple);
                    log::trace!("session removed: {}", network_tuple);
                });
            }
            entry.insert(packet_sender);
            accept_sender.send(ip_stack_stream)?;
        }
    }
    Ok(())
}

fn create_stream(packet: NetworkPacket, cfg: &IpStackConfig, up_pkt_sender: PacketSender) -> Result<(PacketSender, IpStackStream)> {
    let src_addr = packet.src_addr();
    let dst_addr = packet.dst_addr();
    match packet.transport_header() {
        TransportHeader::Tcp(h) => {
            let stream = IpStackTcpStream::new(src_addr, dst_addr, h.clone(), up_pkt_sender, cfg.mtu, cfg.tcp_timeout)?;
            Ok((stream.stream_sender(), IpStackStream::Tcp(stream)))
        }
        TransportHeader::Udp(_) => {
            let stream = IpStackUdpStream::new(src_addr, dst_addr, packet.payload, up_pkt_sender, cfg.mtu, cfg.udp_timeout);
            Ok((stream.stream_sender(), IpStackStream::Udp(stream)))
        }
        TransportHeader::Unknown => {
            unreachable!()
        }
    }
}

async fn process_upstream_recv<Device: AsyncWrite + Unpin + 'static>(
    up_packet: NetworkPacket,
    sessions: SessionCollection,
    device: &mut Device,
    #[cfg(unix)] packet_information: bool,
) -> Result<()> {
    if up_packet.ttl() == DROP_TTL {
        let network_tuple = up_packet.reverse_network_tuple();
        sessions.lock().await.remove(&network_tuple);
        log::trace!("session removed: {}", network_tuple);
        return Ok(());
    }
    #[allow(unused_mut)]
    let Ok(mut packet_bytes) = up_packet.to_bytes() else {
        log::trace!("to_bytes error");
        return Ok(());
    };
    #[cfg(unix)]
    if packet_information {
        if up_packet.src_addr().is_ipv4() {
            packet_bytes.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP4].concat());
        } else {
            packet_bytes.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP6].concat());
        }
    }
    device.write_all(&packet_bytes).await?;
    // device.flush().await?;

    Ok(())
}
