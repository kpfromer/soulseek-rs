use crate::{
    error,
    message::{Message, MessageHandler},
    peer::PeerMessage,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct PlaceInQueueResponse;

impl MessageHandler<PeerMessage> for PlaceInQueueResponse {
    fn get_code(&self) -> u32 {
        43
    }

    fn handle(&self, message: &mut Message, sender: UnboundedSender<PeerMessage>) {
        let filename = message.read_string();
        let place = message.read_int32();

        if let Err(e) = sender.send(PeerMessage::PlaceInQueueResponse { filename, place }) {
            error!("[place_in_queue_response] Failed to send PlaceInQueueResponse: {}", e);
        }
    }
}
