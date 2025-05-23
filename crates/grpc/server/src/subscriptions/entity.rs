use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use futures_util::StreamExt;
use rand::Rng;
use starknet::core::types::Felt;
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::RwLock;
use torii_sqlite::constants::SQL_FELT_DELIMITER;
use torii_sqlite::error::{Error, ParseError};
use torii_sqlite::simple_broker::SimpleBroker;
use torii_sqlite::types::OptimisticEntity;
use tracing::{error, trace};

use super::{match_entity, SUBSCRIPTION_CHANNEL_SIZE};
use torii_proto::proto::world::SubscribeEntityResponse;
use torii_proto::Clause;

pub(crate) const LOG_TARGET: &str = "torii::grpc::server::subscriptions::entity";

#[derive(Debug)]
pub struct EntitiesSubscriber {
    /// The clause that the subscriber is interested in
    pub(crate) clause: Option<Clause>,
    /// The channel to send the response back to the subscriber.
    pub(crate) sender: Sender<Result<SubscribeEntityResponse, tonic::Status>>,
}
#[derive(Debug, Default)]
pub struct EntityManager {
    subscribers: RwLock<HashMap<u64, EntitiesSubscriber>>,
}

impl EntityManager {
    pub async fn add_subscriber(
        &self,
        clause: Option<Clause>,
    ) -> Result<Receiver<Result<SubscribeEntityResponse, tonic::Status>>, Error> {
        let subscription_id = rand::thread_rng().gen::<u64>();
        let (sender, receiver) = channel(SUBSCRIPTION_CHANNEL_SIZE);

        // NOTE: unlock issue with firefox/safari
        // initially send empty stream message to return from
        // initial subscribe call
        let _ = sender
            .send(Ok(SubscribeEntityResponse {
                entity: None,
                subscription_id,
            }))
            .await;

        self.subscribers
            .write()
            .await
            .insert(subscription_id, EntitiesSubscriber { clause, sender });

        Ok(receiver)
    }

    pub async fn update_subscriber(&self, id: u64, clause: Option<Clause>) {
        let sender = {
            let subscribers = self.subscribers.read().await;
            if let Some(subscriber) = subscribers.get(&id) {
                subscriber.sender.clone()
            } else {
                return; // Subscriber not found, exit early
            }
        };

        self.subscribers
            .write()
            .await
            .insert(id, EntitiesSubscriber { clause, sender });
    }

    pub(super) async fn remove_subscriber(&self, id: u64) {
        self.subscribers.write().await.remove(&id);
    }
}

#[must_use = "Service does nothing unless polled"]
#[allow(missing_debug_implementations)]
pub struct Service {
    simple_broker: Pin<Box<dyn Stream<Item = OptimisticEntity> + Send>>,
    entity_sender: UnboundedSender<OptimisticEntity>,
}

impl Service {
    pub fn new(subs_manager: Arc<EntityManager>) -> Self {
        let (entity_sender, entity_receiver) = unbounded_channel();
        let service = Self {
            simple_broker: Box::pin(SimpleBroker::<OptimisticEntity>::subscribe()),
            entity_sender,
        };

        tokio::spawn(Self::publish_updates(subs_manager, entity_receiver));

        service
    }

    async fn publish_updates(
        subs: Arc<EntityManager>,
        mut entity_receiver: UnboundedReceiver<OptimisticEntity>,
    ) {
        while let Some(entity) = entity_receiver.recv().await {
            if let Err(e) = Self::process_entity_update(&subs, &entity).await {
                error!(target = LOG_TARGET, error = %e, "Processing entity update.");
            }
        }
    }

    async fn process_entity_update(
        subs: &Arc<EntityManager>,
        entity: &OptimisticEntity,
    ) -> Result<(), Error> {
        let mut closed_stream = Vec::new();
        let hashed = Felt::from_str(&entity.id).map_err(ParseError::FromStr)?;
        // keys is empty when an entity is updated with StoreUpdateRecord or Member but the entity
        // has never been set before. In that case, we dont know the keys
        let keys = entity
            .keys
            .trim_end_matches(SQL_FELT_DELIMITER)
            .split(SQL_FELT_DELIMITER)
            .filter_map(|key| {
                if key.is_empty() {
                    None
                } else {
                    Some(Felt::from_str(key))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(ParseError::FromStr)?;

        for (idx, sub) in subs.subscribers.read().await.iter() {
            // Check if the subscriber is interested in this entity
            // If we have a clause of hashed keys, then check that the id of the entity
            // is in the list of hashed keys.

            // If we have a clause of keys, then check that the key pattern of the entity
            // matches the key pattern of the subscriber.
            if let Some(clause) = &sub.clause {
                if !match_entity(hashed, &keys, &entity.updated_model, clause) {
                    continue;
                }
            }

            if entity.deleted {
                let resp = SubscribeEntityResponse {
                    entity: Some(torii_proto::proto::types::Entity {
                        hashed_keys: hashed.to_bytes_be().to_vec(),
                        models: vec![],
                        event_id: entity.event_id.clone(),
                        executed_at_timestamp: entity.executed_at.timestamp() as u64,
                        created_at_timestamp: entity.created_at.timestamp() as u64,
                        updated_at_timestamp: entity.updated_at.timestamp() as u64,
                        is_deleted: true,
                    }),
                    subscription_id: *idx,
                };

                if sub.sender.send(Ok(resp)).await.is_err() {
                    closed_stream.push(*idx);
                }

                continue;
            }

            // This should NEVER be None
            let model = entity
                .updated_model
                .as_ref()
                .unwrap()
                .as_struct()
                .unwrap()
                .clone();
            let resp = SubscribeEntityResponse {
                entity: Some(torii_proto::proto::types::Entity {
                    hashed_keys: hashed.to_bytes_be().to_vec(),
                    models: vec![model.into()],
                    event_id: entity.event_id.clone(),
                    executed_at_timestamp: entity.executed_at.timestamp() as u64,
                    created_at_timestamp: entity.created_at.timestamp() as u64,
                    updated_at_timestamp: entity.updated_at.timestamp() as u64,
                    is_deleted: false,
                }),
                subscription_id: *idx,
            };

            if sub.sender.send(Ok(resp)).await.is_err() {
                closed_stream.push(*idx);
            }
        }

        for id in closed_stream {
            trace!(target = LOG_TARGET, id = %id, "Closing entity stream.");
            subs.remove_subscriber(id).await
        }

        Ok(())
    }
}

impl Future for Service {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        while let Poll::Ready(Some(entity)) = this.simple_broker.poll_next_unpin(cx) {
            if let Err(e) = this.entity_sender.send(entity) {
                error!(target = LOG_TARGET, error = %e, "Sending entity update to processor.");
            }
        }

        Poll::Pending
    }
}
