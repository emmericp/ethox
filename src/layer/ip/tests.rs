use super::*;
use crate::managed::Slice;
use crate::nic::{external::External, Device};
use crate::layer::{eth, ip};
use crate::wire::{EthernetAddress, IpAddress, IpCidr, IpProtocol, Ipv4Cidr, Payload, PayloadMut};

const MAC_ADDR_SRC: EthernetAddress = EthernetAddress([0, 1, 2, 3, 4, 5]);
const IP_ADDR_SRC: IpAddress = IpAddress::v4(127, 0, 0, 1);
const MAC_ADDR_DST: EthernetAddress = EthernetAddress([6, 5, 4, 3, 2, 1]);
const IP_ADDR_DST: IpAddress = IpAddress::v4(127, 0, 0, 2);

static PAYLOAD_BYTES: [u8; 50] =
    [0xaa, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0xff];

fn simple_send<P: PayloadMut>(frame: ip::RawPacket<P>) {
    let init = ip::Init {
        src_mask: Ipv4Cidr::UNSPECIFIED.into(),
        dst_addr: IP_ADDR_DST.into(),
        payload: PAYLOAD_BYTES.len(),
        protocol: IpProtocol::Unknown(0xEF),
    };
    let mut prepared = frame.prepare(init)
        .expect("Found no valid routes");
    prepared
        .packet
        .payload_mut()
        .as_mut_slice()
        .copy_from_slice(&PAYLOAD_BYTES[..]);
    prepared.send()
        .expect("Could actuall egress packet");
}

fn simple_recv<P: Payload>(frame: Packet<P>) {
    assert_eq!(frame.packet.payload().as_slice(), &PAYLOAD_BYTES[..]);
}

#[test]
fn simple() {
    let mut nic = External::new_send(Slice::One(vec![0; 1024]));

    let mut eth = [eth::Neighbor::default(); 1];
    let mut eth = eth::Endpoint::new(MAC_ADDR_SRC, {
        let mut eth_cache = eth::NeighborCache::new(&mut eth[..]);
        eth_cache.fill(IP_ADDR_DST, MAC_ADDR_DST, None).unwrap();
        eth_cache
    });

    let mut ip = [ip::Route::unspecified(); 2];
    let mut ip = ip::Endpoint::new(IpCidr::new(IP_ADDR_SRC, 24), {
        let ip_routes = ip::Routes::new(&mut ip[..]);
        // No routes necessary for local link.
        ip_routes
    });

    let sent = nic.tx(1,
        eth.send(ip.send_with(simple_send)));
    assert_eq!(sent, Ok(1));
}