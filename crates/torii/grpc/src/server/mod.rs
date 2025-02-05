pub mod error;
pub mod logger;
pub mod subscription;

use std::pin::Pin;
use std::sync::Arc;

use dojo_types::schema::KeysClause;
use futures::Stream;
use protos::world::{
    MetadataRequest, MetadataResponse, SubscribeEntitiesRequest, SubscribeEntitiesResponse,
};
use sqlx::{Pool, Sqlite};
use starknet::core::utils::cairo_short_string_to_felt;
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use starknet_crypto::FieldElement;
use tokio::sync::mpsc::Receiver;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use torii_core::error::{Error, ParseError};
use torii_core::model::{parse_sql_model_members, SqlModelMember};

use self::subscription::SubscribeRequest;
use crate::protos::types::clause::ClauseType;
use crate::protos::{self};

#[derive(Clone)]
pub struct DojoWorld {
    world_address: FieldElement,
    pool: Pool<Sqlite>,
    subscriber_manager: Arc<subscription::SubscriberManager>,
}

impl DojoWorld {
    pub fn new(
        pool: Pool<Sqlite>,
        block_rx: Receiver<u64>,
        world_address: FieldElement,
        provider: Arc<JsonRpcClient<HttpTransport>>,
    ) -> Self {
        let subscriber_manager = Arc::new(subscription::SubscriberManager::default());

        tokio::task::spawn(subscription::Service::new_with_block_rcv(
            block_rx,
            world_address,
            provider,
            Arc::clone(&subscriber_manager),
        ));

        Self { pool, world_address, subscriber_manager }
    }
}

impl DojoWorld {
    pub async fn metadata(&self) -> Result<protos::types::WorldMetadata, Error> {
        let (world_address, world_class_hash, executor_address, executor_class_hash): (
            String,
            String,
            String,
            String,
        ) = sqlx::query_as(&format!(
            "SELECT world_address, world_class_hash, executor_address, executor_class_hash FROM \
             worlds WHERE id = '{:#x}'",
            self.world_address
        ))
        .fetch_one(&self.pool)
        .await?;

        let models: Vec<(String, String, u32, u32, String)> = sqlx::query_as(
            "SELECT name, class_hash, packed_size, unpacked_size, layout FROM models",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut models_metadata = Vec::with_capacity(models.len());
        for model in models {
            let schema = self.model_schema(&model.0).await?;
            models_metadata.push(protos::types::ModelMetadata {
                name: model.0,
                class_hash: model.1,
                packed_size: model.2,
                unpacked_size: model.3,
                layout: hex::decode(&model.4).unwrap(),
                schema: serde_json::to_vec(&schema).unwrap(),
            });
        }

        Ok(protos::types::WorldMetadata {
            world_address,
            world_class_hash,
            executor_address,
            executor_class_hash,
            models: models_metadata,
        })
    }

    async fn model_schema(&self, model: &str) -> Result<dojo_types::schema::Ty, Error> {
        let model_members: Vec<SqlModelMember> = sqlx::query_as(
            "SELECT id, model_idx, member_idx, name, type, type_enum, enum_options, key FROM \
             model_members WHERE model_id = ? ORDER BY model_idx ASC, member_idx ASC",
        )
        .bind(model)
        .fetch_all(&self.pool)
        .await?;

        Ok(parse_sql_model_members(model, &model_members))
    }

    pub async fn model_metadata(&self, model: &str) -> Result<protos::types::ModelMetadata, Error> {
        let (name, class_hash, packed_size, unpacked_size, layout): (
            String,
            String,
            u32,
            u32,
            String,
        ) = sqlx::query_as(
            "SELECT name, class_hash, packed_size, unpacked_size, layout FROM models WHERE id = ?",
        )
        .bind(model)
        .fetch_one(&self.pool)
        .await?;

        let schema = self.model_schema(model).await?;
        let layout = hex::decode(&layout).unwrap();

        Ok(protos::types::ModelMetadata {
            name,
            layout,
            class_hash,
            packed_size,
            unpacked_size,
            schema: serde_json::to_vec(&schema).unwrap(),
        })
    }

    async fn subscribe_entities(
        &self,
        queries: Vec<protos::types::EntityQuery>,
    ) -> Result<Receiver<Result<protos::world::SubscribeEntitiesResponse, tonic::Status>>, Error>
    {
        let mut subs = Vec::with_capacity(queries.len());
        for query in queries {
            let clause: KeysClause = query
                .clause
                .ok_or(Error::UnsupportedQuery)
                .and_then(|clause| clause.clause_type.ok_or(Error::UnsupportedQuery))
                .and_then(|clause_type| match clause_type {
                    ClauseType::Keys(clause) => Ok(clause),
                    _ => Err(Error::UnsupportedQuery),
                })?
                .try_into()
                .map_err(ParseError::FromByteSliceError)?;

            let model = cairo_short_string_to_felt(&query.model)
                .map_err(ParseError::CairoShortStringToFelt)?;

            let protos::types::ModelMetadata { packed_size, .. } =
                self.model_metadata(&query.model).await?;

            subs.push(SubscribeRequest {
                keys: clause.keys,
                model: subscription::ModelMetadata {
                    name: model,
                    packed_size: packed_size as usize,
                },
            });
        }

        let res = self.subscriber_manager.add_subscriber(subs).await;

        Ok(res)
    }
}

type ServiceResult<T> = Result<Response<T>, Status>;
type SubscribeEntitiesResponseStream =
    Pin<Box<dyn Stream<Item = Result<SubscribeEntitiesResponse, Status>> + Send>>;

#[tonic::async_trait]
impl protos::world::world_server::World for DojoWorld {
    async fn world_metadata(
        &self,
        _request: Request<MetadataRequest>,
    ) -> Result<Response<MetadataResponse>, Status> {
        let metadata = self.metadata().await.map_err(|e| match e {
            Error::Sql(sqlx::Error::RowNotFound) => Status::not_found("World not found"),
            e => Status::internal(e.to_string()),
        })?;

        Ok(Response::new(MetadataResponse { metadata: Some(metadata) }))
    }

    type SubscribeEntitiesStream = SubscribeEntitiesResponseStream;

    async fn subscribe_entities(
        &self,
        request: Request<SubscribeEntitiesRequest>,
    ) -> ServiceResult<Self::SubscribeEntitiesStream> {
        let SubscribeEntitiesRequest { queries } = request.into_inner();
        let rx =
            self.subscribe_entities(queries).await.map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::SubscribeEntitiesStream))
    }
}
