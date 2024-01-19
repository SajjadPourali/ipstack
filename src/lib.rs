pub use error::{IpStackError, Result};
use etherparse::TransportHeader;
use packet::{NetworkPacket, NetworkTuple};
use std::{
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        HashMap,
    },
    time::Duration,
};
use stream::{IpStackStream, RawPacket};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
#[cfg(feature = "log")]
use tracing::{error, trace};

use crate::{
    packet::IpStackPacketProtocol,
    stream::{IpStackTcpStream, IpStackUdpStream},
};
mod error;
mod packet;
pub mod stream;

const DROP_TTL: u8 = 0;

#[cfg(unix)]
const TTL: u8 = 64;

#[cfg(windows)]
const TTL: u8 = 128;

#[cfg(unix)]
const TUN_FLAGS: [u8; 2] = [0x00, 0x00];

#[cfg(target_os = "linux")]
const TUN_PROTO_IP6: [u8; 2] = [0x86, 0xdd];
#[cfg(target_os = "linux")]
const TUN_PROTO_IP4: [u8; 2] = [0x08, 0x00];

#[cfg(target_os = "macos")]
const TUN_PROTO_IP6: [u8; 2] = [0x00, 0x02];
#[cfg(target_os = "macos")]
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
    pub fn tcp_timeout(&mut self, timeout: Duration) {
        self.tcp_timeout = timeout;
    }
    pub fn udp_timeout(&mut self, timeout: Duration) {
        self.udp_timeout = timeout;
    }
    pub fn mtu(&mut self, mtu: u16) {
        self.mtu = mtu;
    }
    pub fn packet_information(&mut self, packet_information: bool) {
        self.packet_information = packet_information;
    }
}

pub struct IpStack {
    accept_receiver: UnboundedReceiver<IpStackStream>,
}

impl IpStack {
    pub fn new<D>(config: IpStackConfig, mut device: D) -> IpStack
    where
        D: AsyncRead + AsyncWrite + std::marker::Unpin + std::marker::Send + 'static,
    {
        let (accept_sender, accept_receiver) = mpsc::unbounded_channel::<IpStackStream>();

        tokio::spawn(async move {
            let mut streams: HashMap<NetworkTuple, UnboundedSender<NetworkPacket>> = HashMap::new();
            let mut buffer = [0u8; u16::MAX as usize];

            let (pkt_sender, mut pkt_receiver) = mpsc::unbounded_channel::<NetworkPacket>();
            loop {
                // dbg!(streams.len());
                select! {
                    Ok(n) = device.read(&mut buffer) => {
                        let offset = if config.packet_information && cfg!(unix) {4} else {0};
                        // dbg!(&buffer[offset..n]);
                        let Ok(packet) = packet::parse_packet(&buffer[offset..n]) else {
                            #[cfg(feature = "log")]
                            trace!("parse error");
                            continue;
                        };
                        match packet{
                            packet::TunPacket::NetworkPacket(packet)=>{
                                let packet = *packet;
                                match streams.entry(packet.network_tuple()){
                                    Occupied(entry) =>{
                                        let t = packet.transport_protocol();
                                        if let Err(_x) = entry.get().send(packet){
                                            #[cfg(feature = "log")]
                                            trace!("{}", _x);
                                            match t{
                                                IpStackPacketProtocol::Tcp(_t) => {
                                                    // dbg!(t.flags());
                                                }
                                                IpStackPacketProtocol::Udp => {
                                                    // dbg!("udp");
                                                }
                                            }

                                        }
                                    }
                                    Vacant(entry) => {
                                        match packet.transport_protocol(){
                                            IpStackPacketProtocol::Tcp(h) => {
                                                match IpStackTcpStream::new(packet.src_addr(),packet.dst_addr(),h, pkt_sender.clone(),config.mtu,config.tcp_timeout).await{
                                                    Ok(stream) => {
                                                        entry.insert(stream.stream_sender());
                                                        accept_sender.send(IpStackStream::Tcp(stream))?;
                                                    }
                                                    Err(_e) => {
                                                        #[cfg(feature = "log")]
                                                        error!("{}", _e);
                                                    }
                                                }
                                            }
                                            IpStackPacketProtocol::Udp => {
                                                let stream = IpStackUdpStream::new(packet.src_addr(),packet.dst_addr(),packet.payload, pkt_sender.clone(),config.mtu,config.udp_timeout);
                                                entry.insert(stream.stream_sender());
                                                accept_sender.send(IpStackStream::Udp(stream))?;
                                            }
                                        }
                                    }
                                }
                            }
                            packet::TunPacket::RawPacket=>{
                                accept_sender.send(IpStackStream::RawPacket(RawPacket::new(buffer[offset..n].to_vec(), pkt_sender.clone(),config.mtu)))?;
                            }
                        }
                    }
                    Some(packet) = pkt_receiver.recv() => {
                        let t = packet.transport.clone();
                        if (matches!(t, TransportHeader::Tcp(_)) || matches!(t, TransportHeader::Udp(_))) && packet.ttl() == 0{
                            streams.remove(&packet.reverse_network_tuple());
                            continue;
                        }
                        #[allow(unused_mut)]
                        let Ok(mut packet_byte) = packet.to_bytes() else{
                            #[cfg(feature = "log")]
                            trace!("to_bytes error");
                            continue;
                        };
                        #[cfg(unix)]
                        if config.packet_information {
                            if packet.src_ip().is_ipv4(){
                                packet_byte.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP4].concat());
                            } else{
                                packet_byte.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP6].concat());
                            }
                        }
                        device.write_all(&packet_byte).await?;
                        // device.flush().await.unwrap();
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), IpStackError>(())
        });

        IpStack { accept_receiver }
    }
    pub async fn accept(&mut self) -> Result<IpStackStream, IpStackError> {
        if let Some(s) = self.accept_receiver.recv().await {
            Ok(s)
        } else {
            Err(IpStackError::AcceptError)
        }
    }
}
