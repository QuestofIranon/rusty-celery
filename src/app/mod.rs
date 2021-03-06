use futures::{select, StreamExt};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc::{self, UnboundedSender};

mod trace;

use crate::protocol::{Message, MessageBody, TryIntoMessage};
use crate::{Broker, Error, ErrorKind, Task};
use trace::{build_tracer, TraceBuilder, TracerTrait};

#[derive(Copy, Clone, Default)]
struct TaskOptions {
    timeout: Option<usize>,
    max_retries: Option<usize>,
    min_retry_delay: usize,
    max_retry_delay: usize,
}

impl TaskOptions {
    fn overrides<T: Task>(&self, task: &T) -> Self {
        Self {
            timeout: task.timeout().or(self.timeout),
            max_retries: task.max_retries().or(self.max_retries),
            min_retry_delay: task.min_retry_delay().unwrap_or(self.min_retry_delay),
            max_retry_delay: task.max_retry_delay().unwrap_or(self.max_retry_delay),
        }
    }
}

#[derive(Clone, Debug)]
enum TaskStatus {
    Pending,
    Finished,
}

#[derive(Clone, Debug)]
struct TaskEvent {
    status: TaskStatus,
}

impl TaskEvent {
    fn new(status: TaskStatus) -> Self {
        Self { status }
    }
}

struct Config<B>
where
    B: Broker + 'static,
{
    name: String,
    broker: B,
    default_queue_name: String,
    task_options: TaskOptions,
}

/// Used to create a `Celery` app with a custom configuration.
pub struct CeleryBuilder<B>
where
    B: Broker + 'static,
{
    config: Config<B>,
}

impl<B> CeleryBuilder<B>
where
    B: Broker + 'static,
{
    /// Get a `CeleryBuilder` for creating a `Celery` app with a custom configuration.
    fn new(name: &str, broker: B) -> Self {
        Self {
            config: Config {
                name: name.into(),
                broker,
                default_queue_name: "celery".into(),
                task_options: TaskOptions {
                    timeout: None,
                    max_retries: None,
                    min_retry_delay: 0,
                    max_retry_delay: 3600,
                },
            },
        }
    }

    /// Set the name of the default queue.
    pub fn default_queue_name(mut self, queue_name: &str) -> Self {
        self.config.default_queue_name = queue_name.into();
        self
    }

    /// Set a default timeout for tasks.
    pub fn task_timeout(mut self, task_timeout: usize) -> Self {
        self.config.task_options.timeout = Some(task_timeout);
        self
    }

    /// Set a default maximum number of retries for tasks.
    pub fn task_max_retries(mut self, task_max_retries: usize) -> Self {
        self.config.task_options.max_retries = Some(task_max_retries);
        self
    }

    /// Set a default minimum retry delay for tasks.
    pub fn task_min_retry_delay(mut self, task_min_retry_delay: usize) -> Self {
        self.config.task_options.min_retry_delay = task_min_retry_delay;
        self
    }

    /// Set a default maximum retry delay for tasks.
    pub fn task_max_retry_delay(mut self, task_max_retry_delay: usize) -> Self {
        self.config.task_options.max_retry_delay = task_max_retry_delay;
        self
    }

    /// Construct a `Celery` app with the current configuration .
    pub fn build(self) -> Celery<B> {
        Celery {
            name: self.config.name,
            broker: self.config.broker,
            default_queue_name: self.config.default_queue_name,
            task_trace_builders: RwLock::new(HashMap::new()),
            task_options: self.config.task_options,
        }
    }
}

/// A `Celery` app is used to produce or consume tasks asyncronously.
pub struct Celery<B: Broker> {
    /// An arbitrary, human-readable name for the app.
    pub name: String,

    /// The app's broker.
    pub broker: B,

    /// The default queue to send and receive from.
    pub default_queue_name: String,

    /// Mapping of task name to task tracer factory. Used to create a task tracer
    /// from an incoming message.
    task_trace_builders: RwLock<HashMap<String, TraceBuilder>>,

    /// Default task options.
    task_options: TaskOptions,
}

impl<B> Celery<B>
where
    B: Broker + 'static,
{
    /// Get a `CeleryBuilder` for creating a `Celery` app with a custom configuration.
    pub fn builder(name: &str, broker: B) -> CeleryBuilder<B> {
        CeleryBuilder::new(name, broker)
    }

    /// Create a new `Celery` app with the given name and broker.
    pub fn new(name: &str, broker: B) -> Self {
        Self::builder(name, broker).build()
    }

    /// Send a task to a remote worker.
    pub async fn send_task<T: Task>(&self, task: T, queue: &str) -> Result<(), Error> {
        let body = MessageBody::new(task);
        let data = serde_json::to_vec(&body)?;
        let message = Message::builder(T::NAME, data).build();
        debug!("Sending message {:?}", message);
        self.broker.send(&message, queue).await
    }

    /// Register a task.
    pub fn register_task<T: Task + 'static>(&self) -> Result<(), Error> {
        let mut task_trace_builders = self
            .task_trace_builders
            .write()
            .map_err(|_| Error::from(ErrorKind::SyncError))?;
        if task_trace_builders.contains_key(T::NAME) {
            Err(ErrorKind::TaskAlreadyExists(T::NAME.into()).into())
        } else {
            task_trace_builders.insert(T::NAME.into(), Box::new(build_tracer::<T>));
            info!("Registered task {}", T::NAME);
            Ok(())
        }
    }

    fn get_task_tracer(
        &self,
        message: Message,
        event_tx: UnboundedSender<TaskEvent>,
    ) -> Result<Box<dyn TracerTrait>, Error> {
        let task_trace_builders = self
            .task_trace_builders
            .read()
            .map_err(|_| Error::from(ErrorKind::SyncError))?;
        if let Some(build_tracer) = task_trace_builders.get(&message.headers.task) {
            Ok(build_tracer(message, self.task_options, event_tx)?)
        } else {
            Err(ErrorKind::UnregisteredTaskError(message.headers.task).into())
        }
    }

    /// Trie converting a delivery into a `Message`, executing the corresponding task,
    /// and communicating with the broker.
    async fn try_handle_delivery(
        &self,
        delivery_result: Result<B::Delivery, B::DeliveryError>,
        event_tx: UnboundedSender<TaskEvent>,
    ) -> Result<(), Error> {
        let delivery = delivery_result.map_err(|e| e.into())?;
        debug!("Received delivery: {:?}", delivery);

        let message = match delivery.try_into_message() {
            Ok(message) => message,
            Err(e) => {
                self.broker.ack(delivery).await?;
                return Err(e);
            }
        };

        let mut tracer = match self.get_task_tracer(message, event_tx) {
            Ok(tracer) => tracer,
            Err(e) => {
                self.broker.ack(delivery).await?;
                return Err(e);
            }
        };

        if tracer.is_delayed() {
            // Task has an ETA, so we need to increment the prefetch count.
            if let Err(e) = self.broker.increase_prefetch_count().await {
                // If for some reason this operation fails, we should stop tracing
                // this task and send it back to the broker to retry.
                // Otherwise we could reach the prefetch_count and end up blocking
                // other deliveries if there are a high number of messages with a
                // future ETA.
                self.broker.retry(delivery, None).await?;
                return Err(e);
            };
        }

        match tracer.trace().await {
            Ok(_) => {
                self.broker.ack(delivery).await?;
            }
            Err(e) => match e.kind() {
                ErrorKind::Retry => {
                    let retry_eta = tracer.retry_eta();
                    self.broker.retry(delivery, retry_eta).await?
                }
                _ => self.broker.ack(delivery).await?,
            },
        };

        if tracer.is_delayed() {
            self.broker.decrease_prefetch_count().await?;
        }

        Ok(())
    }

    /// Wraps `try_handle_delivery` to catch any and all errors that might occur.
    async fn handle_delivery(
        &self,
        delivery_result: Result<B::Delivery, B::DeliveryError>,
        event_tx: UnboundedSender<TaskEvent>,
    ) {
        if let Err(e) = self.try_handle_delivery(delivery_result, event_tx).await {
            error!("{}", e);
        }
    }

    /// Consume tasks from a queue.
    pub async fn consume(&'static self, queue: &str) -> Result<(), Error> {
        // Stream of deliveries from the queue.
        let mut deliveries = Box::pin(self.broker.consume(queue).await?.fuse());

        // Stream of OS signals.
        let mut signals = signal(SignalKind::interrupt())?.fuse();

        // A sender and receiver for task related events.
        // NOTE: we can use an unbounded channel since we already have backpressure
        // from the `prefetch_count` setting.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TaskEvent>();
        let mut event_rx = event_rx.fuse();
        let mut pending_tasks = 0;

        // This is the main loop where we receive deliveries and pass them off
        // to be handled by spawning `self.handle_delivery`.
        // At the same time we are also listening for a SIGINT (Ctrl+C) interruption.
        // If that occurs we break from this loop and move to the warm shutdown loop
        // if there are still any pending tasks (tasks being executed, not including
        // tasks being delayed due to a future ETA).
        loop {
            select! {
                maybe_delivery_result = deliveries.next() => {
                    if let Some(delivery_result) = maybe_delivery_result {
                        let event_tx = event_tx.clone();
                        tokio::spawn(self.handle_delivery(delivery_result, event_tx));
                    }
                },
                _ = signals.next() => {
                    warn!("Ope! Hitting Ctrl+C again will terminate all running tasks!");
                    info!("Warm shutdown...");
                    break;
                },
                maybe_event = event_rx.next() => {
                    if let Some(event) = maybe_event {
                        debug!("Received task event {:?}", event);
                        match event.status {
                            TaskStatus::Pending => pending_tasks += 1,
                            TaskStatus::Finished => pending_tasks -= 1,
                        };
                    }
                },
            };
        }

        if pending_tasks > 0 {
            // Warm shutdown loop. When there are still pendings tasks we wait for them
            // to finish. We get updates about pending tasks through the `event_rx` channel.
            // We also watch for a second SIGINT, in which case we immediately shutdown.
            info!("Waiting on {} pending tasks...", pending_tasks);
            loop {
                select! {
                    _ = signals.next() => {
                        warn!("Okay fine, shutting down now. See ya!");
                        return Err(ErrorKind::ForcedShutdown.into());
                    },
                    maybe_event = event_rx.next() => {
                        if let Some(event) = maybe_event {
                            debug!("Received task event {:?}", event);
                            match event.status {
                                TaskStatus::Pending => pending_tasks += 1,
                                TaskStatus::Finished => pending_tasks -= 1,
                            };
                            if pending_tasks <= 0 {
                                break;
                            }
                        }
                    },
                };
            }
        }

        info!("No more pending tasks. See ya!");

        Ok(())
    }
}
