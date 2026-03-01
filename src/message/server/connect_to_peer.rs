use crate::actor::server_actor::ServerMessage;
use crate::error;
use crate::message::{Message, MessageHandler};
use crate::peer::Peer;
use tokio::sync::mpsc::UnboundedSender;
pub struct ConnectToPeerHandler;

impl MessageHandler<ServerMessage> for ConnectToPeerHandler {
    fn get_code(&self) -> u32 {
        18
    }
    fn handle(&self, message: &mut Message, sender: UnboundedSender<ServerMessage>) {
        let peer = Peer::new_from_message(message);
        if let Err(e) = sender.send(ServerMessage::ConnectToPeer(peer)) {
            error!("[connect_to_peer] Failed to send ConnectToPeer: {}", e);
        }
    }
}
