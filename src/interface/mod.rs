mod config;
mod peer_server;

use self::config::{ConfigurationServiceManager, UpdateEvent, Command, ConfigurationCodec};
use self::peer_server::{PeerServer, PeerServerMessage};

use base64;
use hex;
use byteorder::{ByteOrder, BigEndian, LittleEndian};
use snow::NoiseBuilder;
use protocol::Peer;
use std::io;
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, IpAddr, SocketAddr};
use std::time::Duration;
use types::{InterfaceInfo};

use pnet::packet::ipv4::Ipv4Packet;

use futures::{Future, Stream, Sink, future, unsync, sync, stream};
use tokio_core::reactor::{Core, Handle};
use tokio_core::net::{UdpSocket, UdpCodec};
use tokio_utun::{UtunStream, UtunCodec};
use tokio_io::{AsyncRead};
use tokio_io::codec::{Framed, Encoder, Decoder};
use tokio_uds::{UnixListener};
use tokio_timer::{Interval, Timer};
use treebitmap::{IpLookupTable, IpLookupTableOps};


pub fn debug_packet(header: &str, packet: &[u8]) {
    let packet = Ipv4Packet::new(packet);
    debug!("{} {:?}", header, packet);
}

pub type SharedPeer = Rc<RefCell<Peer>>;
pub type SharedState = Rc<RefCell<State>>;

pub struct State {
    pubkey_map: HashMap<[u8; 32], SharedPeer>,
    index_map: HashMap<u32, SharedPeer>,
    ip4_map: IpLookupTable<Ipv4Addr, SharedPeer>,
    ip6_map: IpLookupTable<Ipv6Addr, SharedPeer>,
    interface_info: InterfaceInfo,
}

pub struct Interface {
    name: String,
    state: SharedState,
}

struct VecUtunCodec;
#[allow(dead_code)]
enum UtunPacket {
    Inet4(Vec<u8>),
    Inet6(Vec<u8>),
}
impl UtunCodec for VecUtunCodec {
    type In = Vec<u8>;
    type Out = Vec<u8>;

    fn decode(&mut self, buf: &[u8]) -> io::Result<Self::In> {
        debug!("utun packet type {}", buf[3]);
        Ok(buf[4..].to_vec())
    }

    fn encode(&mut self, mut msg: Self::Out, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&[0u8, 0, 0, 2]);
        buf.append(&mut msg);
    }
}

impl Interface {
    pub fn new(name: &str) -> Self {
        let state = State {
            pubkey_map: HashMap::new(),
            index_map: HashMap::new(),
            ip4_map: IpLookupTable::new(),
            ip6_map: IpLookupTable::new(),
            interface_info: InterfaceInfo::default(),
        };
        Interface {
            name: name.to_owned(),
            state: Rc::new(RefCell::new(state)),
        }
    }

    pub fn start(&mut self) {
        let mut core = Core::new().unwrap();

        let (utun_tx, utun_rx) = unsync::mpsc::channel::<Vec<u8>>(1024);

        let peer_server = PeerServer::bind(core.handle(), self.state.clone(), utun_tx.clone());

        let utun_stream = UtunStream::connect(&self.name, &core.handle()).unwrap().framed(VecUtunCodec{});
        let (utun_writer, utun_reader) = utun_stream.split();

        let utun_read_fut = peer_server.tx().sink_map_err(|_| ()).send_all(
            utun_reader.map_err(|_|())).map_err(|_|());

        let utun_write_fut = utun_writer.sink_map_err(|_| ()).send_all(
            utun_rx.map_err(|_| ())).map_err(|_| ());

        let utun_fut = utun_write_fut.join(utun_read_fut);

        let handle = core.handle();
        let listener = UnixListener::bind(ConfigurationServiceManager::get_path(&self.name).unwrap(), &handle).unwrap();
        let (config_tx, config_rx) = sync::mpsc::channel::<UpdateEvent>(1024);
        let h = handle.clone();
        let config_server = listener.incoming().for_each({
            let config_tx = config_tx.clone();
            let state = self.state.clone();
            move |(stream, _)| {
                let (sink, stream) = stream.framed(ConfigurationCodec {}).split();
                debug!("UnixServer connection.");

                let handle = h.clone();
                let responses = stream.and_then({
                    let config_tx = config_tx.clone();
                    let state = state.clone();
                    move |command| {
                        let state = state.borrow();
                        match command {
                            Command::Set(_version, items) => {
                                config_tx.clone().send_all(stream::iter_ok(items)).wait().unwrap();
                                future::ok("errno=0\nerrno=0\n\n".to_string())
                            },
                            Command::Get(_version) => {
                                let info = &state.interface_info;
                                let peers = &state.pubkey_map;
                                let mut s = String::new();
                                if let Some(private_key) = info.private_key {
                                    s.push_str(&format!("private_key={}\n", hex::encode(private_key)));
                                }

                                for (_, peer) in peers.iter() {
                                    s.push_str(&peer.borrow().to_config_string());
                                }
                                future::ok(format!("{}errno=0\n\n", s))
                            }
                        }
                    }
                });

                let fut = sink.send_all(responses).map(|_| ()).map_err(|_| ());

                handle.spawn(fut);

                Ok(())
            }
        }).map_err(|_| ());

        let config_fut = config_rx.for_each({
            let tx = peer_server.udp_tx().clone();
            let handle = handle.clone();
            let state = self.state.clone();
            move |event| {
                let mut state = state.borrow_mut();
                match event {
                    UpdateEvent::PrivateKey(private_key) => {
                        state.interface_info.private_key = Some(private_key);
                        debug!("set new private key");
                    },
                    UpdateEvent::ListenPort(port) => {
                        state.interface_info.listen_port = Some(port);
                        debug!("set new listen port");
                    },
                    UpdateEvent::UpdatePeer(info) => {
                        info!("added new peer: {}", info);
                        let mut noise = NoiseBuilder::new("Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s".parse().unwrap())
                            .local_private_key(&state.interface_info.private_key.expect("no private key!"))
                            .remote_public_key(&info.pub_key)
                            .prologue("WireGuard v1 zx2c4 Jason@zx2c4.com".as_bytes())
                            .psk(2, &info.psk.expect("no psk!"))
                            .build_initiator().unwrap();

                        let mut peer = Peer::new(info.clone());
                        peer.set_next_session(noise.into());

                        let init_packet = peer.get_handshake_packet();
                        let our_index = peer.our_next_index().unwrap();
                        let peer = Rc::new(RefCell::new(peer));

                        for (ip_addr, mask) in info.allowed_ips {
                            match ip_addr {
                                IpAddr::V4(v4_addr) => { state.ip4_map.insert(v4_addr, mask, peer.clone()); },
                                IpAddr::V6(v6_addr) => { state.ip6_map.insert(v6_addr, mask, peer.clone()); },
                            }
                        }

                        let _ = state.index_map.insert(our_index, peer.clone());
                        let _ = state.pubkey_map.insert(info.pub_key, peer);

                        handle.spawn(tx.clone().send((info.endpoint.unwrap(), init_packet)).then(|_| Ok(())));
                    },
                    _ => unimplemented!()
                }

                future::ok(())
            }
        }).map_err(|_| ());

        core.run(peer_server.join(utun_fut.join(config_fut.join(config_server)))).unwrap();
    }
}