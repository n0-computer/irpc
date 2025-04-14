use irpc::{channel::oneshot, Client, LocalSender, Service};
use irpc_derive::rpc_requests;
use serde::{Deserialize, Serialize};

mod clock {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct ClockService;

    impl Service for ClockService {}

    #[rpc_requests(ClockService, message = ClockMessage)]
    #[derive(Serialize, Deserialize)]
    pub enum ClockProtocol {
        #[rpc(tx = oneshot::Sender<u64>)]
        GetTime(GetTime),
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct GetTime;
}

mod calc {

    use super::*;

    #[derive(Debug, Clone)]
    pub struct CalcService;

    impl Service for CalcService {}

    #[rpc_requests(CalcService, message = CalcMessage)]
    #[derive(Serialize, Deserialize)]
    pub enum CalcProtocol {
        #[rpc(tx = oneshot::Sender<u128>)]
        Multiply(Multiply),
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Multiply {
        pub a: u128,
        pub b: u128,
    }

    struct CalcActor {
        recv: tokio::sync::mpsc::Receiver<CalcMessage>,
    }

    impl CalcActor {
        pub fn spawn() -> CalcApi {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let actor = Self { recv: rx };
            n0_future::task::spawn(actor.run());
            let local = LocalSender::<CalcMessage, CalcService>::from(tx);
            CalcApi {
                inner: local.into(),
            }
        }

        async fn run(mut self) {
            while let Some(msg) = self.recv.recv().await {
                self.handle(msg).await;
            }
        }

        async fn handle(&mut self, msg: CalcMessage) {
            match msg {
                CalcMessage::Multiply(msg) => {
                    let res = msg.a * msg.b;
                    msg.tx.send(res).await.ok();
                }
            }
        }
    }

    pub struct CalcApi {
        inner: Client<CalcMessage, CalcProtocol, CalcService>,
    }

    impl CalcApi {
        pub async fn multiply(&self, a: u128, b: u128) -> std::result::Result<u128, irpc::Error> {
            let msg = Multiply { a, b };
            self.inner.rpc(msg).await
        }
    }
}

mod combined {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct CombinedService;

    impl Service for CombinedService {}

    #[derive(Serialize, Deserialize)]
    pub enum CombinedProtocol {
        Clock(clock::ClockProtocol),
        Calc(calc::CalcProtocol),
    }

    pub enum CombinedMessage {
        Clock(clock::ClockMessage),
        Calc(calc::CalcMessage),
    }
}
