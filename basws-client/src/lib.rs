use async_channel::{Receiver, Sender};
pub use async_handle::Handle;
pub use async_trait::async_trait;
use basws_shared::{
    challenge,
    protocol::{
        InstallationConfig, ServerError, ServerRequest, ServerResponse, WsBatchResponse, WsRequest,
    },
    timing::current_timestamp,
    Uuid,
};
use futures::{stream::SplitSink, stream::SplitStream, SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
pub use url::Url;

#[derive(Debug, Clone)]
pub enum LoginState {
    Disconnected,
    Handshaking { config: Option<InstallationConfig> },
    Connected { installation_id: Uuid },
    Error { message: Option<String> },
}

#[async_trait]
pub trait WebsocketClientLogic: Send + Sync {
    type Request: Serialize + DeserializeOwned + Sync + Send + Clone + Debug;
    type Response: Serialize + DeserializeOwned + Sync + Send + Clone + Debug;

    fn server_url(&self) -> Url;
    fn protocol_version(&self) -> String;

    async fn state_changed(&self, state: &LoginState, client: Client<Self>) -> anyhow::Result<()>;
    async fn stored_installation_config(&self) -> Option<InstallationConfig>;
    async fn store_installation_config(&self, config: InstallationConfig) -> anyhow::Result<()>;

    async fn response_received(
        &self,
        response: Self::Response,
        request_id: i64,
        client: Client<Self>,
    ) -> anyhow::Result<()>;

    async fn handle_server_error(
        &self,
        error: ServerError,
        client: Client<Self>,
    ) -> anyhow::Result<()>;
}

pub struct Client<L>
where
    L: WebsocketClientLogic + ?Sized,
{
    data: Handle<ClientData<L>>,
}

impl<L> Clone for Client<L>
where
    L: WebsocketClientLogic,
{
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
        }
    }
}

struct ClientData<L>
where
    L: WebsocketClientLogic + ?Sized,
{
    logic: Box<L>,
    login_state: LoginState,
    sender: Sender<WsRequest<L::Request>>,
    receiver: Receiver<WsRequest<L::Request>>,
    average_roundtrip: f64,
    average_server_timestamp_delta: f64,
    request_counter: i64,
}

impl<L> ClientData<L>
where
    L: WebsocketClientLogic + 'static,
{
    pub async fn send(&self, request: ServerRequest<L::Request>) -> anyhow::Result<()> {
        let (mut id, overflowed) = self.request_counter.overflowing_add(1);
        if overflowed {
            id = 0;
        }
        // TODO self.request_counter = id;
        self.sender.send(WsRequest { id, request }).await?;
        Ok(())
    }
}

impl<L> Client<L>
where
    L: WebsocketClientLogic + 'static,
{
    pub fn new(logic: L) -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            data: Handle::new(ClientData {
                logic: Box::new(logic),
                login_state: LoginState::Disconnected,
                sender,
                receiver,
                average_roundtrip: 0.0,
                average_server_timestamp_delta: 0.0,
                request_counter: 0,
            }),
        }
    }

    pub async fn spawn(self) {
        tokio::spawn(self.run());
    }

    async fn set_login_state(&self, state: LoginState) -> anyhow::Result<()> {
        let client = self.clone();
        let mut network = self.data.write().await;
        network.login_state = state;
        network
            .logic
            .state_changed(&network.login_state, client)
            .await
    }

    pub async fn login_state(&self) -> LoginState {
        let network = self.data.read().await;
        network.login_state.clone()
    }

    async fn receiver(&self) -> Receiver<WsRequest<L::Request>> {
        let network = self.data.read().await;
        network.receiver.clone()
    }

    pub async fn average_roundtrip(&self) -> f64 {
        let network = self.data.read().await;
        network.average_roundtrip
    }

    pub async fn average_server_timestamp_delta(&self) -> f64 {
        let network = self.data.read().await;
        network.average_server_timestamp_delta
    }

    async fn server_url(&self) -> Url {
        let data = self.data.read().await;
        data.logic.server_url()
    }

    async fn protocol_version(&self) -> String {
        let data = self.data.read().await;
        data.logic.protocol_version()
    }

    async fn stored_installation_config(&self) -> Option<InstallationConfig> {
        let data = self.data.read().await;
        data.logic.stored_installation_config().await
    }

    async fn store_installation_config(&self, config: InstallationConfig) -> anyhow::Result<()> {
        let data = self.data.read().await;
        data.logic.store_installation_config(config).await
    }

    async fn response_received(
        &self,
        response: L::Response,
        request_id: i64,
    ) -> anyhow::Result<()> {
        let client = self.clone();
        let data = self.data.read().await;
        data.logic
            .response_received(response, request_id, client)
            .await
    }

    async fn handle_error(&self, error: ServerError) -> anyhow::Result<()> {
        let client = self.clone();
        let data = self.data.read().await;
        data.logic.handle_server_error(error, client).await
    }

    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            // let socket = match Client::new(server_url).connect().await {
            //     Ok(socket) => socket,
            //     Err(err) => {
            //         println!("Error connecting to socket. {}", err);
            //         tokio::time::delay_for(Duration::from_millis(100)).await;
            //         Client::set_login_state(LoginState::Error { message: None }).await;
            //         continue;
            //     }
            // };
            let url = self.server_url().await;

            match connect_async(url).await {
                Ok((ws, _)) => {
                    let (tx, rx) = ws.split();
                    let _ = tokio::try_join!(self.send_loop(tx), self.receive_loop(rx));
                }
                Err(err) => {
                    println!("Error connecting to server: {:?}", err);
                }
            }
            self.set_login_state(LoginState::Disconnected).await?;
            tokio::time::delay_for(tokio::time::Duration::from_millis(500)).await
        }
    }

    async fn receive_loop(
        &self,
        mut rx: SplitStream<WebSocketStream<TcpStream>>,
    ) -> anyhow::Result<()> {
        loop {
            match rx.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    match serde_cbor::from_slice::<WsBatchResponse<L::Response>>(&bytes) {
                        Ok(response) => self.handle_batch_response(response).await?,
                        Err(error) => println!("Error deserializing message. {:?}", error),
                    }
                }
                Some(Err(err)) => {
                    println!("Websocket Error: {:?}", err);
                    anyhow::bail!("Error on websocket");
                }
                None => {
                    println!("Socket Disconnected");
                    anyhow::bail!("Disconnected on read");
                }
                _ => {}
            }
        }
    }

    async fn handle_batch_response(
        &self,
        batch: WsBatchResponse<L::Response>,
    ) -> anyhow::Result<()> {
        for response in batch.results {
            match response {
                ServerResponse::NewInstallation(config) => {
                    self.set_login_state(LoginState::Connected {
                        installation_id: config.id,
                    })
                    .await?;
                    self.store_installation_config(config).await?;
                }
                ServerResponse::Connected { installation_id } => {
                    self.set_login_state(LoginState::Connected { installation_id })
                        .await?;
                }
                ServerResponse::Challenge { nonce } => {
                    let config = self.stored_installation_config().await.ok_or_else(|| {
                        anyhow::anyhow!("Server issued challenge, but client has no stored config")
                    })?;

                    self.request(ServerRequest::ChallengeResponse(
                        challenge::compute_challenge(&config.private_key, &nonce),
                    ))
                    .await?
                }
                ServerResponse::Ping {
                    average_roundtrip,
                    average_server_timestamp_delta,
                    timestamp,
                } => {
                    let mut data = self.data.write().await;
                    data.average_roundtrip = average_roundtrip;
                    data.average_server_timestamp_delta = average_server_timestamp_delta;
                    data.send(ServerRequest::Pong {
                        original_timestamp: timestamp,
                        timestamp: current_timestamp(),
                    })
                    .await?;
                }
                ServerResponse::Error(error) => self.handle_error(error).await?,
                ServerResponse::Response(response) => {
                    self.response_received(response, batch.request_id).await?
                }
            }
        }

        Ok(())
    }

    pub async fn request(&self, request: ServerRequest<L::Request>) -> anyhow::Result<()> {
        let data = self.data.read().await;
        data.send(request).await?;
        Ok(())
    }

    async fn send_loop(
        &self,
        mut tx: SplitSink<WebSocketStream<TcpStream>, Message>,
    ) -> anyhow::Result<()> {
        let config = self.stored_installation_config().await;
        let installation_id = config.as_ref().map(|config| config.id);
        self.set_login_state(LoginState::Handshaking { config })
            .await?;

        self.request(ServerRequest::Greetings {
            version: self.protocol_version().await,
            installation_id,
        })
        .await?;

        let receiver = self.receiver().await;
        loop {
            let request = receiver.recv().await?;

            if let Err(err) = tx
                .send(Message::Binary(serde_cbor::to_vec(&request).unwrap()))
                .await
            {
                println!("Error sending message: {}", err);
                anyhow::bail!("Disconnected on send");
            }
        }
    }
}
