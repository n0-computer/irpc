use quic_rpc_derive::rpc_requests;

#[rpc_requests(Service, Msg)]
enum Enum {
    A(u8),
    B(u8),
}

fn main() {}