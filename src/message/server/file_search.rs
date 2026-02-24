use crate::{debug, info};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    actor::server_actor::ServerMessage, message::Message,
    message::handlers::MessageHandler,
};

pub struct FileSearchHandler;

impl MessageHandler<ServerMessage> for FileSearchHandler {
    fn get_code(&self) -> u32 {
        26
    }
    fn handle(&self, message: &mut Message, _sender: UnboundedSender<ServerMessage>) {
        debug!("Handling file search message");
        let username = message.read_string();
        let token = message.read_int32();
        let query = message.read_string();
        info!(
            "Message search username:{}, token: {}, query: {}",
            username, token, query
        );
    }
}
