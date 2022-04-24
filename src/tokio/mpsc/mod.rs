use std::{ops::DerefMut, sync::Arc};

use futures::future;
use tokio::sync::{
    mpsc::{
        error::{SendError, TrySendError},
        Sender,
    },
    Mutex,
};

/// Handles fanning out to multiple [`tokio::sync::mpsc`] channels.
///
/// Can be cheaply cloned to share the target channels.
pub struct FanOut<T> {
    chans: Arc<Mutex<Vec<Sender<T>>>>,
}

impl<T> FanOut<T> {
    // Methods to reserve a permit cannot be implemented because it would require returning a
    // reference to the reserved channel (inside the `Permit`), which cannot be done because of an
    // internal lock on the collection of channels.

    /// Constructs a new `FanOut` from a `Vec<Sender<T>>`s.
    pub fn new(chans: Vec<Sender<T>>) -> Self {
        Self {
            chans: Arc::from(Mutex::new(chans)),
        }
    }

    /// Consumes the `FanOut` and returns the inner `Vec<Sender<T>>`.
    ///
    /// Note that if data was sent to the channels and any were encountered that
    /// were closed, they will have been removed from the collection, and the
    /// collection may not be in the original order. The collection may even be
    /// empty.
    ///
    /// If the `FanOut` is in use (if you have an unfinished `Future` to send to
    /// the channels), it will return `Err` with itself in it. You should try
    /// again once all pending `Future`s to send have completed.
    pub fn into_inner(this: Self) -> Result<Vec<Sender<T>>, Self> {
        Ok(Arc::try_unwrap(this.chans)
            .map_err(|chans| Self { chans })?
            .into_inner())
    }

    /// Sends to any single channel that's ready to receive data.
    ///
    /// Concurrent calls are executed in order.
    ///
    /// Canceling follows the same semantics of canceling a call to
    /// [`tokio::sync::mpsc::Sender<T>::send()`].
    ///
    /// Any channels that are closed are removed.
    /// If there are no remaining channels, `Err` is returned.
    pub async fn send(&self, mut data: T) -> Result<(), SendError<T>> {
        let mut chans = self.chans.lock().await;

        loop {
            // `&mut &*chans`? What?
            // This turns `Deref<Target = Vec<T>>` into `DerefMut<Target = &Vec<T>>`.
            // It makes it so we can do what we need to do without passing ownership
            // of `chans` to `send_locked()`.
            match Self::send_locked(&mut &*chans, data).await {
                Ok(()) => break Ok(()),
                Err((SendError(returned_data), Some(unusable))) => {
                    // The channel is no longer valid.
                    // Remove it from the collection and try again.

                    chans.swap_remove(unusable);

                    // Grab the `data` returned in the error to try sending again.
                    data = returned_data;

                    continue;
                }
                Err((err, None)) => {
                    // There are no more usable channels.
                    break Err(err);
                }
            }
        }
    }

    async fn send_locked<'a, Cs>(chans: Cs, data: T) -> Result<(), (SendError<T>, Option<usize>)>
    where
        Cs: DerefMut<Target = &'a Vec<Sender<T>>>,
        T: 'a,
    {
        if chans.is_empty() {
            return Err((SendError(data), None));
        }

        let (result, index, _) =
            future::select_all(chans.iter().map(|chan| Box::pin(chan.reserve()))).await;

        match result {
            Ok(permit) => {
                permit.send(data);

                Ok(())
            }
            Err(SendError(())) => Err((SendError(data), Some(index))),
        }
    }

    /// Tries to send to the first available channel, trying one a a time, in
    /// order.
    ///
    /// Concurrent calls are tried in order, after the previous call completes
    /// or is canceled.
    ///
    /// Canceling is safe. No data will be sent.
    ///
    /// Any closed channels encountered are removed. If there are no open
    /// channels, or the open channels are all full, `Err` is returned.
    pub async fn try_send(&self, data: T) -> Result<(), TrySendError<T>> {
        let mut chans = self.chans.lock().await;

        let mut data = Some(data);
        let mut closed = None;
        for (i, chan) in chans.iter().enumerate() {
            match chan.try_send(data.take().expect(
                "There's a bug. \
                    There should always be data to send, at this point.",
            )) {
                Ok(()) => {
                    // Can't just return from here because we need to clean up any closed channels
                    // that we encountered on previous passes through the loop.
                    break;
                }
                Err(err) => {
                    // Grab the returned data to try again in the next loop.
                    data = Some(match err {
                        TrySendError::Closed(data) => {
                            if closed.is_none() {
                                closed = Some(Vec::new());
                            }

                            if let Some(closed) = &mut closed {
                                closed.push(i)
                            }

                            data
                        }
                        TrySendError::Full(data) => data,
                    });
                }
            }
        }

        if let Some(closed) = closed {
            // Remove the closed channels from the collection so we don't try them anymore.

            for index in closed.into_iter().rev() {
                chans.swap_remove(index);
            }
        }

        match data {
            Some(data) => {
                // Did not succeed in sending the data to a channel. Why?

                if chans.is_empty() {
                    // All the channels were closed.
                    Err(TrySendError::Closed(data))
                } else {
                    // All the channels were full.
                    Err(TrySendError::Full(data))
                }
            }
            None => {
                // The data was successfully sent to a channel.
                Ok(())
            }
        }
    }
}

impl<I, T> From<I> for FanOut<T>
where
    I: IntoIterator<Item = Sender<T>>,
{
    fn from(chans: I) -> Self {
        Self::new(chans.into_iter().collect())
    }
}

#[cfg(test)]
mod test {
    use std::{fmt::Debug, time::Duration};

    use tokio::{
        sync::mpsc::{channel, error::TryRecvError, Receiver},
        time::timeout,
    };

    use super::*;

    const MS_10: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn send_basic() {
        let (tx1, mut rx1) = channel::<usize>(1);
        let (tx2, mut rx2) = channel::<usize>(1);
        let (tx3, mut rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        // The order here is critical to the test.

        // The channels each have a buffer size of 1. Fill them.
        assert_send_not_blocked(MS_10, &sender, 1).await;
        assert_send_not_blocked(MS_10, &sender, 2).await;
        assert_send_not_blocked(MS_10, &sender, 3).await;

        // The channels should now be full, so we shouldn't be able to send.
        assert_send_blocked(MS_10, &sender, usize::MAX).await;

        // Since all the channels are full, whichever one we read from should receive the next send.
        assert_eq!(3, recv_not_blocked(MS_10, &mut rx3).await);
        assert_send_not_blocked(MS_10, &sender, 4).await;
        assert_eq!(4, recv_not_blocked(MS_10, &mut rx3).await);

        // OK, go ahead and read from these early ones.
        assert_eq!(2, recv_not_blocked(MS_10, &mut rx2).await);
        assert_eq!(1, recv_not_blocked(MS_10, &mut rx1).await);

        // The channels should all be empty, now.
        assert_eq!(
            Err(TryRecvError::Empty),
            rx1.try_recv(),
            "Channel should be empty"
        );
        assert_eq!(
            Err(TryRecvError::Empty),
            rx2.try_recv(),
            "Channel should be empty"
        );
        assert_eq!(
            Err(TryRecvError::Empty),
            rx3.try_recv(),
            "Channel should be empty"
        );

        // Dropping the sender should close the channels.
        drop(sender);

        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx1.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx2.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx3.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
    }

    #[tokio::test]
    async fn send_some_closed() {
        let (tx1, mut rx1) = channel::<usize>(1);
        let (tx2, rx2) = channel::<usize>(1);
        let (tx3, mut rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        drop(rx2);

        assert_eq!(
            3,
            sender.chans.lock().await.len(),
            "All the channels should be in the collection"
        );
        assert_send_not_blocked(MS_10, &sender, 1).await;
        assert_send_not_blocked(MS_10, &sender, 2).await;
        assert_send_blocked(MS_10, &sender, 3).await;
        assert_eq!(
            2,
            sender.chans.lock().await.len(),
            "The closed channel should have been removed"
        );
        assert_eq!(1, recv_not_blocked(MS_10, &mut rx1).await);
        assert_eq!(2, recv_not_blocked(MS_10, &mut rx3).await);

        // Dropping the sender should close the remaining channels.
        drop(sender);

        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx1.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx3.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
    }

    #[tokio::test]
    async fn send_all_closed() {
        let (tx1, rx1) = channel::<usize>(1);
        let (tx2, rx2) = channel::<usize>(1);
        let (tx3, rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        drop(rx1);
        drop(rx2);
        drop(rx3);

        assert_eq!(
            3,
            sender.chans.lock().await.len(),
            "All the channels should be in the collection"
        );
        timeout(MS_10, sender.send(usize::MAX))
            .await
            .expect("Timed out sending with no open channels")
            .expect_err("Should have gotten an error when sending with no remaining open channels");
        assert_eq!(
            0,
            sender.chans.lock().await.len(),
            "The closed channels should have been removed"
        );
    }

    #[tokio::test]
    async fn try_send_basic() {
        let (tx1, mut rx1) = channel::<usize>(1);
        let (tx2, mut rx2) = channel::<usize>(1);
        let (tx3, mut rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        // The order here is critical to the test.

        // The channels each have a buffer size of 1. Fill them.
        assert_try_send(&sender, 1).await;
        assert_try_send(&sender, 2).await;
        assert_try_send(&sender, 3).await;

        // The channels should now be full, so we shouldn't be able to send.
        assert_try_send_full(&sender, usize::MAX).await;

        // Since all the channels are full, whichever one we read from should receive the next send.
        assert_eq!(3, recv_not_blocked(MS_10, &mut rx3).await);
        assert_try_send(&sender, 4).await;
        assert_eq!(4, recv_not_blocked(MS_10, &mut rx3).await);

        // OK, go ahead and read from these early ones.
        assert_eq!(2, recv_not_blocked(MS_10, &mut rx2).await);
        assert_eq!(1, recv_not_blocked(MS_10, &mut rx1).await);

        // The channels should all be empty, now.
        assert_eq!(
            Err(TryRecvError::Empty),
            rx1.try_recv(),
            "Channel should be empty"
        );
        assert_eq!(
            Err(TryRecvError::Empty),
            rx2.try_recv(),
            "Channel should be empty"
        );
        assert_eq!(
            Err(TryRecvError::Empty),
            rx3.try_recv(),
            "Channel should be empty"
        );

        // Dropping the sender should close the channels.
        drop(sender);

        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx1.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx2.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx3.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
    }

    #[tokio::test]
    async fn try_send_some_closed() {
        let (tx1, mut rx1) = channel::<usize>(1);
        let (tx2, rx2) = channel::<usize>(1);
        let (tx3, mut rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        drop(rx2);

        assert_eq!(
            3,
            sender.chans.lock().await.len(),
            "All the channels should be in the collection"
        );
        assert_send_not_blocked(MS_10, &sender, 1).await;
        assert_send_not_blocked(MS_10, &sender, 2).await;
        assert_send_blocked(MS_10, &sender, 3).await;
        assert_eq!(
            2,
            sender.chans.lock().await.len(),
            "The closed channel should have been removed"
        );
        assert_eq!(1, recv_not_blocked(MS_10, &mut rx1).await);
        assert_eq!(2, recv_not_blocked(MS_10, &mut rx3).await);

        // Dropping the sender should close the remaining channels.
        drop(sender);

        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx1.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
        assert_eq!(
            Err(TryRecvError::Disconnected),
            rx3.try_recv(),
            "Channel should be disconnected once the FanOut has been dropped"
        );
    }

    #[tokio::test]
    async fn try_send_all_closed() {
        let (tx1, rx1) = channel::<usize>(1);
        let (tx2, rx2) = channel::<usize>(1);
        let (tx3, rx3) = channel::<usize>(1);

        let sender = FanOut::from([tx1, tx2, tx3]);

        drop(rx1);
        drop(rx2);
        drop(rx3);

        assert_eq!(
            3,
            sender.chans.lock().await.len(),
            "All the channels should be in the collection"
        );
        assert_eq!(
            Err(TrySendError::Closed(usize::MAX)),
            sender.try_send(usize::MAX).await,
            ""
        );
        assert_eq!(
            0,
            sender.chans.lock().await.len(),
            "The closed channels should have been removed"
        );
    }

    async fn send_not_blocked<T>(
        duration: Duration,
        sender: &FanOut<T>,
        data: T,
    ) -> Result<(), SendError<T>> {
        timeout(duration, sender.send(data))
            .await
            .expect("Timeout sending data")
    }

    async fn assert_send_not_blocked<T>(duration: Duration, sender: &FanOut<T>, data: T) {
        send_not_blocked(duration, sender, data)
            .await
            .unwrap_or_else(|err| panic!("Could not send: {}", err))
    }
    async fn assert_try_send<T>(sender: &FanOut<T>, data: T)
    where
        T: Debug,
    {
        sender.try_send(data).await.expect("Could not send data")
    }

    async fn assert_send_blocked<T>(duration: Duration, sender: &FanOut<T>, data: T)
    where
        T: Debug,
    {
        timeout(duration, sender.send(data))
            .await
            .expect_err("Should have gotten a timeout sending data, but did not");
    }

    async fn assert_try_send_full<T>(sender: &FanOut<T>, data: T)
    where
        T: Debug,
    {
        match sender.try_send(data).await {
            Ok(()) => panic!("Channel should have been full, but still had capacity"),
            Err(TrySendError::Closed(_)) => panic!("Channel should have been full, but was closed"),
            Err(TrySendError::Full(_)) => {}
        }
    }

    #[allow(dead_code)]
    async fn assert_try_send_closed<T>(sender: &FanOut<T>, data: T)
    where
        T: Debug,
    {
        match sender.try_send(data).await {
            Ok(()) => panic!("Channel should have been full, but still had capacity"),
            Err(TrySendError::Full(_)) => panic!("Channel should have been closed, but was full"),
            Err(TrySendError::Closed(_)) => {}
        }
    }

    async fn recv_not_blocked<T>(duration: Duration, rx: &mut Receiver<T>) -> T {
        timeout(duration, rx.recv())
            .await
            .expect("Should not have blocked receiving")
            .expect("Should have received data")
    }

    #[allow(dead_code)]
    async fn recv_no_data<T>(duration: Duration, rx: &mut Receiver<T>)
    where
        T: Debug + PartialEq,
    {
        assert_eq!(
            None,
            timeout(duration, rx.recv())
                .await
                .expect("Should not have blocked receiving"),
            "There should have been nothing to receive",
        );
    }
}
