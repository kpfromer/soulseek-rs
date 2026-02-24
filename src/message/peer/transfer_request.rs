use crate::{
    message::{Message, MessageHandler},
    peer::PeerMessage,
    types::Transfer,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct TransferRequest;
impl MessageHandler<PeerMessage> for TransferRequest {
    fn get_code(&self) -> u32 {
        40
    }
    fn handle(&self, message: &mut Message, sender: UnboundedSender<PeerMessage>) {
        let transfer = Transfer::new_from_message(message);

        sender.send(PeerMessage::TransferRequest(transfer)).unwrap();
    }
}
