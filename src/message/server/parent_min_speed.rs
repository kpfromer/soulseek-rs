use crate::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    actor::server_actor::ServerMessage,
    message::{Message, MessageHandler},
};

pub struct ParentMinSpeedHandler;

impl MessageHandler<ServerMessage> for ParentMinSpeedHandler {
    fn get_code(&self) -> u32 {
        83
    }

    fn handle(&self, message: &mut Message, sender: UnboundedSender<ServerMessage>) {
        let _ = sender;
        let number = message.read_int32();
        debug!("Parent min speed: {}", number);
    }
}
