use crate::{
    message::{Message, MessageHandler},
    peer::PeerMessage,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct TransferResponse;

impl MessageHandler<PeerMessage> for TransferResponse {
    fn get_code(&self) -> u32 {
        41
    }

    fn handle(&self, message: &mut Message, sender: UnboundedSender<PeerMessage>) {
        let token = message.read_int32();
        let allowed = message.read_int8();
        let reason = (allowed == 0).then(|| message.read_string());

        sender
            .send(PeerMessage::TransferResponse {
                token,
                allowed: allowed == 1,
                reason,
            })
            .unwrap();
    }
}
