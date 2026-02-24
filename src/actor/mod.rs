use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::trace;

pub mod peer_actor;
pub mod peer_registry;
pub mod server_actor;

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Disconnected,
    Connecting { since: Instant },
    Connected,
}
/// Core actor trait - each actor processes messages
pub trait Actor: Send + 'static {
    type Message: Send + 'static;

    /// Handle a single message
    fn handle(&mut self, msg: Self::Message);

    /// Called when actor starts (optional hook)
    fn on_start(&mut self) {}

    /// Called when actor stops (optional hook)
    fn on_stop(&mut self) {}

    /// Optional periodic tick for background work
    fn tick(&mut self) {}
}

pub struct ActorHandle<M: Send> {
    pub(crate) sender: mpsc::UnboundedSender<ActorMessage<M>>,
}

impl<M: Send> Clone for ActorHandle<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<M: Send> ActorHandle<M> {
    pub fn send(&self, msg: M) -> Result<(), String> {
        self.sender
            .send(ActorMessage::UserMessage(msg))
            .map_err(|e| format!("Failed to send message: {}", e))
    }

    /// Request actor to stop gracefully
    pub fn stop(&self) -> Result<(), String> {
        self.sender
            .send(ActorMessage::Stop)
            .map_err(|e| format!("Failed to send stop signal: {}", e))
    }
}

/// Internal actor message wrapper
pub(crate) enum ActorMessage<M> {
    UserMessage(M),
    Stop,
    #[allow(dead_code)]
    Tick,
}

/// Actor system that manages actor lifecycle
pub struct ActorSystem;

impl ActorSystem {
    pub fn new() -> Self {
        ActorSystem
    }

    /// Spawn a new actor and return its handle
    pub fn spawn<A: Actor>(&self, mut actor: A) -> ActorHandle<A::Message> {
        let (sender, receiver) = mpsc::unbounded_channel::<ActorMessage<A::Message>>();
        let handle = ActorHandle {
            sender: sender.clone(),
        };

        // Start the actor event loop as a tokio task
        tokio::spawn(async move {
            actor.on_start();
            Self::run_actor_loop(&mut actor, receiver).await;
            actor.on_stop();
        });

        handle
    }

    /// Spawn a new actor with initialization callback and return its handle
    /// The callback receives the actor handle before on_start is called
    pub fn spawn_with_handle<A: Actor, F>(
        &self,
        mut actor: A,
        init: F,
    ) -> ActorHandle<A::Message>
    where
        F: FnOnce(&mut A, ActorHandle<A::Message>) + Send + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel::<ActorMessage<A::Message>>();
        let handle = ActorHandle {
            sender: sender.clone(),
        };
        let handle_for_init = handle.clone();

        tokio::spawn(async move {
            init(&mut actor, handle_for_init);
            actor.on_start();
            Self::run_actor_loop(&mut actor, receiver).await;
            actor.on_stop();
        });

        handle
    }

    async fn run_actor_loop<A: Actor>(
        actor: &mut A,
        mut receiver: mpsc::UnboundedReceiver<ActorMessage<A::Message>>,
    ) {
        let tick_interval = Duration::from_millis(100);
        let mut message_count = 0;
        let mut tick_count = 0;

        loop {
            tokio::select! {
                msg = receiver.recv() => {
                    match msg {
                        Some(ActorMessage::UserMessage(msg)) => {
                            message_count += 1;
                            actor.handle(msg);
                        }
                        Some(ActorMessage::Stop) => {
                            trace!(
                                "[actor_system] Received Stop message, breaking loop"
                            );
                            break;
                        }
                        Some(ActorMessage::Tick) => {
                            tick_count += 1;
                            trace!(
                                "[actor_system] Received explicit Tick message #{}",
                                tick_count
                            );
                            actor.tick();
                        }
                        None => {
                            trace!(
                                "[actor_system] Channel disconnected, breaking loop"
                            );
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(tick_interval) => {
                    tick_count += 1;
                    actor.tick();
                }
            }
        }
        trace!(
            "[actor_system] run_actor_loop ENDED - processed {} messages, {} ticks",
            message_count, tick_count
        );
    }
}

impl Default for ActorSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CounterActor {
        count: Arc<AtomicUsize>,
    }

    impl Actor for CounterActor {
        type Message = usize;

        fn handle(&mut self, msg: Self::Message) {
            self.count.fetch_add(msg, Ordering::SeqCst);
        }

        fn on_start(&mut self) {
            println!("Counter actor started");
        }

        fn on_stop(&mut self) {
            println!("Counter actor stopped");
        }
    }

    #[tokio::test]
    async fn test_actor_system() {
        let system = Arc::new(ActorSystem::new());

        let count = Arc::new(AtomicUsize::new(0));
        let actor = CounterActor {
            count: count.clone(),
        };

        let handle = system.spawn(actor);

        // Send some messages
        handle.send(1).unwrap();
        handle.send(2).unwrap();
        handle.send(3).unwrap();

        // Give actor time to process
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(count.load(Ordering::SeqCst), 6);

        // Stop the actor
        handle.stop().unwrap();

        // Give actor time to process the stop message
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
