use async_trait::async_trait;
use futures_util::stream::{Stream, StreamExt};

use crate::protocol::{MessageBody, TryIntoMessage};
use crate::{Error, Task};

#[async_trait]
pub trait Broker {
    type Delivery: TryIntoMessage + Clone;
    type DeliveryError: Into<Error>;
    type Consumer: IntoIterator<
            Item = Result<Self::Delivery, Self::DeliveryError>,
            IntoIter = Self::ConsumerIterator,
        > + Stream<Item = Result<Self::Delivery, Self::DeliveryError>>
        + StreamExt;
    type ConsumerIterator: Iterator<Item = Result<Self::Delivery, Self::DeliveryError>>;

    async fn consume(&self, queue: &str) -> Result<Self::Consumer, Error>;

    async fn ack(&self, delivery: Self::Delivery) -> Result<(), Error>;

    async fn send_task<T: Task>(&self, body: MessageBody<T>, queue: &str) -> Result<(), Error>;
}

pub mod amqp;
