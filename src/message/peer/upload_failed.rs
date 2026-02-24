use crate::info;
use crate::{
    message::{Message, MessageHandler},
    peer::PeerMessage,
    types::UploadFailed,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct UploadFailedHandler;
impl MessageHandler<PeerMessage> for UploadFailedHandler {
    fn get_code(&self) -> u32 {
        46
    }
    fn handle(&self, message: &mut Message, _sender: UnboundedSender<PeerMessage>) {
        let upload_failed = UploadFailed::new_from_message(message);
        info!("Upload failed for ${}", upload_failed.filename);
    }
}
