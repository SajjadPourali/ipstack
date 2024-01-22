An asynchronous lightweight implementation of TCP/IP stack for Tun device.
Unstable, under development.

### Usage
````rust
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use udp_stream::UdpStream;
use tokio::io{AsyncRead, AsyncWrite};
async fn copy_from_lhs_to_rhs(lhs: impl AsyncRead + AsyncWrite, rhs: impl AsyncRead + AsyncWrite) {
    let (mut lhs_reader, mut lhs_writer) = tokio::io::split(lhs);
    let (mut rhs_reader, mut rhs_writer) = tokio::io::split(rhs);
    let _r = tokio::join! {
        tokio::io::copy(&mut lhs_reader, &mut rhs_writer) ,
        tokio::io::copy(&mut rhs_reader, &mut lhs_writer),
    };
}
#[tokio::main]
async fn main(){
    const MTU: u16 = 1500;
    let ipv4 = Ipv4Addr::new(10, 0, 0, 1);
    let netmask = Ipv4Addr::new(255, 255, 255, 0);
    let mut config = tun2::Configuration::default();
    config.address(ipv4).netmask(netmask).mtu(MTU as i32).up();

    #[cfg(target_os = "linux")]
    config.platform_config(|config| {
        config.packet_information(true);
        config.apply_settings(true);
    });

    let mut ipstack_config = ipstack::IpStackConfig::default();
    ipstack_config.mtu(MTU);
    let packet_information = cfg!(all(target_family = "unix", not(target_os = "android")));
    ipstack_config.packet_information(packet_information);
    let mut ip_stack = ipstack::IpStack::new(ipstack_config, tun2::create_as_async(&config).unwrap());

    while let Ok(stream) = ip_stack.accept().await {
        match stream {
            IpStackStream::Tcp(tcp) => {
                let rhs = TcpStream::connect("1.1.1.1:80").await.unwrap();
                tokio::spawn(async move {
                    copy_from_lhs_to_rhs(tcp, rhs).await;
                });
            }
            IpStackStream::Udp(udp) => {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53);
                let rhs = UdpStream::connect(addr).await.unwrap();
                tokio::spawn(async move {
                    copy_from_lhs_to_rhs(udp, rhs).await;
                });
            }
            _ => {
                println!("unknown transport");
                continue;
            }
        }
    }
}
````

We also suggest that you take a look at the complete [examples](examples).