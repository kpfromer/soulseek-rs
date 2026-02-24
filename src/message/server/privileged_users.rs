use crate::debug;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    actor::server_actor::ServerMessage,
    message::{Message, MessageHandler},
};

pub struct PrivilegedUsersHandler;

impl MessageHandler<ServerMessage> for PrivilegedUsersHandler {
    fn get_code(&self) -> u32 {
        69
    }

    fn handle(&self, message: &mut Message, _sender: UnboundedSender<ServerMessage>) {
        let number = message.read_int32();
        debug!("Number of privileged users: {}", number);
    }
}
