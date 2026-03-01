use crate::{
    error,
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

        if let Err(e) = sender.send(PeerMessage::TransferRequest(transfer)) {
            error!("[transfer_request] Failed to send TransferRequest: {}", e);
        }
    }
}
