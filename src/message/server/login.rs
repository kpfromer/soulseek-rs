use crate::{
    actor::server_actor::ServerMessage, debug, error, info, message::Message,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::message::MessageHandler;

pub struct LoginHandler;

impl MessageHandler<ServerMessage> for LoginHandler {
    fn get_code(&self) -> u32 {
        1
    }

    fn handle(&self, message: &mut Message, sender: UnboundedSender<ServerMessage>) {
        let response = message.read_int8();

        if response != 1 {
            if let Err(e) = sender.send(ServerMessage::LoginStatus(false)) {
                error!("[login] Failed to send login failure status: {}", e);
            }
            return;
        }

        info!("Login successful");
        let greeting = message.read_string();
        debug!("Server greeting: {:?}", greeting);

        let own_ip = message.read_int32();
        debug!("Own IP: {}", own_ip);

        let password_hash = message.read_string();
        debug!("Password hash: {:?}", password_hash);

        let supporter = message.read_bool();
        debug!("Supporter status: {}", supporter);

        if let Err(e) = sender.send(ServerMessage::LoginStatus(true)) {
            error!("[login] Failed to send login success status: {}", e);
        }
    }
}
