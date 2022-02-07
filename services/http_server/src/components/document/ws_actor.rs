use crate::{
    context::FlowyPersistence,
    components::web_socket::{entities::Socket, WSClientData, WSUser, WebSocketMessage},
    util::serde_ext::{md5, parse_from_bytes},
};
use actix_rt::task::spawn_blocking;
use async_stream::stream;
use http_flowy::errors::{internal_error, Result, ServerError};

use crate::components::web_socket::revision_data_to_ws_message;
use flowy_collaboration::{
    protobuf::{
        ClientRevisionWSData as ClientRevisionWSDataPB, ClientRevisionWSDataType as ClientRevisionWSDataTypePB,
        Revision as RevisionPB,
    },
    server_document::ServerDocumentManager,
    synchronizer::{RevisionSyncResponse, RevisionUser},
};
use futures::stream::StreamExt;
use lib_ws::WSChannel;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub enum DocumentWSActorMessage {
    ClientData {
        client_data: WSClientData,
        persistence: Arc<FlowyPersistence>,
        ret: oneshot::Sender<Result<()>>,
    },
}

pub struct DocumentWebSocketActor {
    actor_msg_receiver: Option<mpsc::Receiver<DocumentWSActorMessage>>,
    doc_manager: Arc<ServerDocumentManager>,
}

impl DocumentWebSocketActor {
    pub fn new(receiver: mpsc::Receiver<DocumentWSActorMessage>, manager: Arc<ServerDocumentManager>) -> Self {
        Self {
            actor_msg_receiver: Some(receiver),
            doc_manager: manager,
        }
    }

    pub async fn run(mut self) {
        let mut actor_msg_receiver = self
            .actor_msg_receiver
            .take()
            .expect("DocumentWebSocketActor's receiver should only take one time");

        let stream = stream! {
            loop {
                match actor_msg_receiver.recv().await {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };

        stream.for_each(|msg| self.handle_message(msg)).await;
    }

    async fn handle_message(&self, msg: DocumentWSActorMessage) {
        match msg {
            DocumentWSActorMessage::ClientData {
                client_data,
                persistence: _,
                ret,
            } => {
                let _ = ret.send(self.handle_document_data(client_data).await);
            }
        }
    }

    async fn handle_document_data(&self, client_data: WSClientData) -> Result<()> {
        let WSClientData { user, socket, data } = client_data;
        let document_client_data = spawn_blocking(move || parse_from_bytes::<ClientRevisionWSDataPB>(&data))
            .await
            .map_err(internal_error)??;

        tracing::trace!(
            "[DocumentWebSocketActor]: receive: {}:{}, {:?}",
            document_client_data.object_id,
            document_client_data.data_id,
            document_client_data.ty
        );

        let user = Arc::new(DocumentRevisionUser { user, socket });
        match &document_client_data.ty {
            ClientRevisionWSDataTypePB::ClientPushRev => {
                let _ = self
                    .doc_manager
                    .handle_client_revisions(user, document_client_data)
                    .await
                    .map_err(internal_error)?;
            }
            ClientRevisionWSDataTypePB::ClientPing => {
                let _ = self
                    .doc_manager
                    .handle_client_ping(user, document_client_data)
                    .await
                    .map_err(internal_error)?;
            }
        }

        Ok(())
    }
}

#[allow(dead_code)]
fn verify_md5(revision: &RevisionPB) -> Result<()> {
    if md5(&revision.delta_data) != revision.md5 {
        return Err(ServerError::internal().context("RevisionPB md5 not match"));
    }
    Ok(())
}

#[derive(Clone)]
pub struct DocumentRevisionUser {
    pub user: Arc<WSUser>,
    pub(crate) socket: Socket,
}

impl std::fmt::Debug for DocumentRevisionUser {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("DocumentRevisionUser")
            .field("user", &self.user)
            .field("socket", &self.socket)
            .finish()
    }
}

impl RevisionUser for DocumentRevisionUser {
    fn user_id(&self) -> String {
        self.user.id().to_string()
    }

    fn receive(&self, resp: RevisionSyncResponse) {
        let result = match resp {
            RevisionSyncResponse::Pull(data) => {
                let msg: WebSocketMessage = revision_data_to_ws_message(data, WSChannel::Document);
                self.socket.try_send(msg).map_err(internal_error)
            }
            RevisionSyncResponse::Push(data) => {
                let msg: WebSocketMessage = revision_data_to_ws_message(data, WSChannel::Document);
                self.socket.try_send(msg).map_err(internal_error)
            }
            RevisionSyncResponse::Ack(data) => {
                let msg: WebSocketMessage = revision_data_to_ws_message(data, WSChannel::Document);
                self.socket.try_send(msg).map_err(internal_error)
            }
        };

        match result {
            Ok(_) => {}
            Err(e) => log::error!("[DocumentRevisionUser]: {}", e),
        }
    }
}
