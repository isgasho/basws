use crate::AccountHandle;
use async_channel::Sender;
use basws_shared::{protocol::WsBatchResponse, timing::NetworkTiming};
use uuid::Uuid;

pub struct ConnectedClient<Response, Account> {
    pub installation_id: Option<Uuid>,
    sender: Sender<WsBatchResponse<Response>>,
    pub account: Option<AccountHandle<Account>>,
    pub network_timing: NetworkTiming,
}

impl<Response, Account> ConnectedClient<Response, Account>
where
    Response: Send + Sync + 'static,
{
    pub fn new(sender: Sender<WsBatchResponse<Response>>) -> Self {
        Self {
            sender,
            account: None,
            installation_id: None,
            network_timing: Default::default(),
        }
    }

    pub fn new_with_installation_id(
        installation_id: Uuid,
        sender: Sender<WsBatchResponse<Response>>,
    ) -> Self {
        Self {
            sender,
            account: None,
            installation_id: Some(installation_id),
            network_timing: Default::default(),
        }
    }

    pub async fn send(&self, response: WsBatchResponse<Response>) -> anyhow::Result<()> {
        Ok(self.sender.send(response).await?)
    }
}
