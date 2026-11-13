use super::*;

#[derive(Clone)]
pub(crate) struct Commands {
    durable: mpsc::Sender<Command>,
    ephemeral: mpsc::Sender<Command>,
}
pub(crate) struct CommandsReceiver {
    pub(super) durable: mpsc::Receiver<Command>,
    pub(super) ephemeral: mpsc::Receiver<Command>,
}
pub(crate) fn commands(capacity: usize) -> (Commands, CommandsReceiver) {
    let (durable, durable_rx) = mpsc::channel(capacity);
    let (ephemeral, ephemeral_rx) = mpsc::channel(capacity);
    (
        Commands { durable, ephemeral },
        CommandsReceiver {
            durable: durable_rx,
            ephemeral: ephemeral_rx,
        },
    )
}
impl Commands {
    pub async fn send(&self, command: Command) -> Result<(), mpsc::error::SendError<Command>> {
        let live = match &command {
            Command::Local { request, .. } => matches!(
                &request.value,
                Request::Publish {
                    ephemeral: true,
                    ..
                }
            ),
            Command::Remote { frame, .. } => matches!(&frame.value, Frame::Publication {
                publication: ChannelPublication::Event(event), ..
            } if event.ephemeral),
            _ => false,
        };
        if live && matches!(command, Command::Remote { .. }) {
            // A slow live worker must not stop the peer reader from admitting
            // subsequent durable/control frames on the same connection.
            match self.ephemeral.try_send(command) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
                Err(mpsc::error::TrySendError::Closed(command)) => {
                    Err(mpsc::error::SendError(command))
                }
            }
        } else if live {
            self.ephemeral.send(command).await
        } else {
            self.durable.send(command).await
        }
    }
}
impl CommandsReceiver {
    #[cfg(test)]
    pub async fn recv_durable(&mut self) -> Option<Command> {
        self.durable.recv().await
    }
    #[cfg(test)]
    pub async fn recv(&mut self) -> Option<Command> {
        tokio::select! {
            item = self.durable.recv() => match item { Some(item) => Some(item), None => self.ephemeral.recv().await },
            item = self.ephemeral.recv() => match item { Some(item) => Some(item), None => self.durable.recv().await },
        }
    }
    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<Command, mpsc::error::TryRecvError> {
        self.durable
            .try_recv()
            .or_else(|_| self.ephemeral.try_recv())
    }
}
