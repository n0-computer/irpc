use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use irpc::{
    channel::{mpsc, RecvError, SendError},
    util::{make_client_endpoint, make_server_endpoint, AsyncWriteVarintExt},
};
use quinn::Endpoint;
use testresult::TestResult;
use tokio::{task::JoinHandle, time::timeout};

pub fn create_connected_endpoints() -> TestResult<(Endpoint, Endpoint, SocketAddr)> {
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let port = server.local_addr()?.port();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    Ok((server, client, server_addr))
}
