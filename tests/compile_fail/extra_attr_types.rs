use irpc::rpc_requests;

#[rpc_requests(Service, Msg)]
enum Enum {
    #[rpc(reply = NoSender, request = NoReceiver, fnord = Foo)]
    A(u8),
}

fn main() {}