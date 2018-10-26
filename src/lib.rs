#![deny(warnings)]
#![feature(test)]

extern crate test;
extern crate tokio;
#[macro_use(try_ready)]
extern crate futures;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate bytes;
extern crate num_cpus;
#[macro_use]
extern crate lazy_static;
extern crate btoi;
extern crate crc16;
extern crate itoa;
extern crate net2;
extern crate tokio_codec;
extern crate tokio_executor;
extern crate tokio_io;
extern crate tokio_timer;
#[macro_use]
extern crate serde_derive;
extern crate md5;
extern crate toml;

mod cluster;
mod cmd;
mod com;
mod fetcher;
pub mod fnv;
mod handler;
mod init;
pub mod ketama;
pub mod mc;
mod node;
mod notify;
pub mod proxy;
mod resp;
mod slots;

pub use cluster::Cluster;
use cmd::CmdCodec;
pub use com::*;
use handler::Handle;

use fetcher::Fetcher;
use init::ClusterInitilizer;
use resp::Resp;

use futures::lazy;
use futures::unsync::mpsc::channel;
use futures::Async;
// use futures::task::current;
use net2::TcpBuilder;
use tokio::net::TcpListener;
use tokio::prelude::{Future, Stream};
use tokio::runtime::current_thread;
use tokio_codec::Decoder;

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::thread;

pub fn run() -> Result<(), std::io::Error> {
    env_logger::init();
    let config = load_config();
    info!("asswecan has been lunched with config={:?}", config);
    let ths: Vec<_> = config
        .clusters
        .iter()
        .map(|cc| create_cluster(cc))
        .flatten()
        .collect();

    for th in ths {
        th.join().unwrap();
    }
    Ok(())
}

fn load_config() -> Config {
    use std::env;
    let path = env::var("AS_CFG").unwrap_or("as.toml".to_string());
    use std::fs;
    use std::io::{BufReader, Read};

    let fd = fs::File::open(&path).expect("fail to open config file(default: as.toml)");
    let mut rd = BufReader::new(fd);
    let mut data = String::new();
    rd.read_to_string(&mut data)
        .expect("fail to read config file");

    toml::from_str(&data).expect("fail to parse toml")
}

pub fn create_cluster(cc: &ClusterConfig) -> Vec<thread::JoinHandle<()>> {
    let count = usize::max(cc.thread, 1);
    info!(
        "asswecan start {} listen at {} with {} thread",
        &cc.name, &cc.listen_addr, count
    );
    (0..count)
        .into_iter()
        .map(|i| {
            let cc = cc.clone();
            let name = cc.name.clone();
            let thb = thread::Builder::new().name(format!("cluster-{}-{}", name, i + 1));
            thb.spawn(move || match cc.cache_type {
                CacheType::Memcache => {
                    let p = proxy::Proxy::new(cc).unwrap();
                    proxy::start_proxy::<mc::Req>(p);
                }
                CacheType::RedisCluster => {
                    let cluster = Cluster::new(cc);
                    start_cluster(cluster)
                }
                _ => {
                    warn!("cache type is not supported");
                }
            })
            .unwrap()
        })
        .collect()
}

pub fn start_cluster(cluster: Cluster) {
    let addr = cluster
        .cc
        .listen_addr
        .clone()
        .parse::<SocketAddr>()
        .expect("parse socket never fail");

    let fut = lazy(move || -> Result<(SocketAddr, Cluster), ()> { Ok((addr, cluster)) })
        .and_then(|(addr, cluster)| {
            let listen = create_reuse_port_listener(&addr).expect("bind never fail");
            // cluster.init_node_conn().unwrap();
            let initilizer = ClusterInitilizer::new(cluster, listen).map_err(|err| {
                error!("fail to init cluster with given server due {:?}", err);
            });
            initilizer
        })
        .and_then(|(cluster, listen)| {
            // TODO: how to spawn timer func with current_thread
            let fetcher = Fetcher::new(cluster.clone())
                .for_each(|_| {
                    debug!("success fetch new slots_map");
                    Ok(())
                })
                .map_err(|err| {
                    error!("fail to fetch new slots_mapd due {:?}", err);
                });
            current_thread::spawn(fetcher);
            Ok((cluster, listen))
        })
        .and_then(|(cluster, listen)| {
            let rc_cluster = cluster.clone();
            let amt = listen
                .incoming()
                .for_each(move |sock| {
                    sock.set_nodelay(true).expect("set nodelay must ok");
                    let codec = CmdCodec::default();
                    let (cmd_tx, resp_rx) = codec.framed(sock).split();
                    let cluster = rc_cluster.clone();
                    // TODO: remove magic number.
                    let (handle_resp_tx, handle_resp_rx) = channel(2048);
                    let input = resp_rx
                        .forward(handle_resp_tx)
                        .map_err(|err| match err {
                            Error::IoError(ref e) if e.kind() == ErrorKind::ConnectionReset => {}
                            e => error!("fail to send into handle due to {:?}", e),
                        })
                        .then(|_| Ok(()));
                    current_thread::spawn(input);
                    let handle = Handle::new(
                        cluster,
                        handle_resp_rx.map_err(|_| {
                            error!("fail to pass handle");
                            Error::Critical
                        }),
                        cmd_tx,
                    )
                    .map_err(|err| {
                        error!("get handle error due {:?}", err);
                    });
                    current_thread::spawn(handle);
                    Ok(())
                })
                .map_err(|err| {
                    error!("fail to start_cluster due {:?}", err);
                });
            current_thread::spawn(amt);
            Ok(())
        });

    current_thread::block_on_all(fut).unwrap();
}

#[cfg(windows)]
fn create_reuse_port_listener(addr: &SocketAddr) -> Result<TcpListener, std::io::Error> {
    let builder = TcpBuilder::new_v4()?;
    let std_listener = builder
        .reuse_address(true)
        .expect("os not support SO_REUSEADDR")
        .bind(addr)?
        .listen(std::i32::MAX)?;
    let hd = tokio::reactor::Handle::current();
    TcpListener::from_std(std_listener, &hd)
}

#[cfg(not(windows))]
fn create_reuse_port_listener(addr: &SocketAddr) -> Result<TcpListener, std::io::Error> {
    use net2::unix::UnixTcpBuilderExt;

    let builder = TcpBuilder::new_v4()?;
    let std_listener = builder
        .reuse_address(true)
        .expect("os not support SO_REUSEADDR")
        .reuse_port(true)
        .expect("os not support SO_REUSEPORT")
        .bind(addr)?
        .listen(std::i32::MAX)?;
    let hd = tokio::reactor::Handle::current();
    TcpListener::from_std(std_listener, &hd)
}

#[derive(Deserialize, Debug)]
pub struct Config {
    clusters: Vec<ClusterConfig>,
}

#[derive(Deserialize, Debug, Clone, Copy)]
pub enum CacheType {
    #[serde(rename = "redis")]
    Redis,
    #[serde(rename = "memcache")]
    Memcache,
    #[serde(rename = "memcache_binary")]
    MemcacheBinary,
    #[serde(rename = "redis_cluster")]
    RedisCluster,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClusterConfig {
    pub name: String,
    pub listen_addr: String,
    pub hash_tag: Option<String>,

    pub ping_fail_limit: Option<usize>,
    pub ping_auto_eject: Option<bool>,
    pub thread: usize,
    pub cache_type: CacheType,
    pub servers: Vec<String>,
    pub fetch: Option<u64>,

    // dead codes

    // command not support now
    pub dial_timeout: Option<u64>,
    pub read_timeout: Option<u64>,
    pub write_timeout: Option<u64>,
    // dead option: not support other proto
    pub listen_proto: Option<String>,
    // dead option: always 1
    pub node_connections: Option<String>,

}

pub struct Batch<S>
where
    S: Stream,
{
    input: S,
    max: usize,
}

impl<S> Stream for Batch<S>
where
    S: Stream,
{
    type Item = VecDeque<S::Item>;
    type Error = S::Error;

    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        let mut buf = VecDeque::new();
        loop {
            match self.input.poll() {
                Ok(Async::NotReady) => {
                    if buf.is_empty() {
                        return Ok(Async::NotReady);
                    }
                    return Ok(Async::Ready(Some(buf)));
                }
                Ok(Async::Ready(None)) => {
                    return Ok(Async::Ready(None));
                }
                Ok(Async::Ready(Some(item))) => {
                    buf.push_back(item);
                    if buf.len() == self.max {
                        return Ok(Async::Ready(Some(buf)));
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }
}
