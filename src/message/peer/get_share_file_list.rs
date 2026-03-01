use crate::{
    error,
    message::{Message, MessageHandler, server::MessageFactory},
    peer::PeerMessage,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct GetShareFileList;
impl MessageHandler<PeerMessage> for GetShareFileList {
    fn get_code(&self) -> u32 {
        4
    }
    fn handle(&self, _message: &mut Message, sender: UnboundedSender<PeerMessage>) {
        let message = MessageFactory::build_shared_folders_message(100, 800);

        if let Err(e) = sender.send(PeerMessage::SendMessage(message)) {
            error!("[get_share_file_list] Failed to send SendMessage: {}", e);
        }
    }
}
