// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use futures::StreamExt;
use futures_async_stream::try_stream;
use itertools::Itertools;
use prometheus::{IntCounter, Opts};
use risingwave_common::array::DataChunk;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::error::{Result, RwError};
use risingwave_common::util::select_all;
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::ExchangeSource as ProstExchangeSource;
use risingwave_pb::plan_common::Field as NodeField;
use risingwave_rpc_client::ComputeClientPoolRef;

use crate::exchange_source::ExchangeSourceImpl;
use crate::execution::grpc_exchange::GrpcExchangeSource;
use crate::execution::local_exchange::LocalExchangeSource;
use crate::executor::ExecutorBuilder;
use crate::task::{BatchTaskContext, TaskId};

pub type ExchangeExecutor<C> = GenericExchangeExecutor<C>;
use super::BatchTaskMetrics;
use crate::executor::{BoxedDataChunkStream, BoxedExecutor, BoxedExecutorBuilder, Executor};
pub struct GenericExchangeExecutor<C> {
    sources: Vec<ExchangeSourceImpl>,
    context: C,

    schema: Schema,
    task_id: TaskId,
    identity: String,

    /// Batch metrics.
    /// None: Local mode don't record mertics.
    metrics: Option<BatchTaskMetrics>,
}

/// `CreateSource` determines the right type of `ExchangeSource` to create.
#[async_trait::async_trait]
pub trait CreateSource: Send {
    async fn create_source(
        &self,
        context: impl BatchTaskContext,
        prost_source: &ProstExchangeSource,
    ) -> Result<ExchangeSourceImpl>;
}

#[derive(Clone)]
pub struct DefaultCreateSource {
    client_pool: ComputeClientPoolRef,
}

impl DefaultCreateSource {
    pub fn new(client_pool: ComputeClientPoolRef) -> Self {
        Self { client_pool }
    }
}

#[async_trait::async_trait]
impl CreateSource for DefaultCreateSource {
    async fn create_source(
        &self,
        context: impl BatchTaskContext,
        prost_source: &ProstExchangeSource,
    ) -> Result<ExchangeSourceImpl> {
        let peer_addr = prost_source.get_host()?.into();
        let task_output_id = prost_source.get_task_output_id()?;
        let task_id = TaskId::from(task_output_id.get_task_id()?);

        if context.is_local_addr(&peer_addr) && prost_source.local_execute_plan.is_none() {
            trace!("Exchange locally [{:?}]", task_output_id);

            Ok(ExchangeSourceImpl::Local(LocalExchangeSource::create(
                task_output_id.try_into()?,
                context,
                task_id,
            )?))
        } else {
            trace!(
                "Exchange remotely from {} [{:?}]",
                &peer_addr,
                task_output_id,
            );

            Ok(ExchangeSourceImpl::Grpc(
                GrpcExchangeSource::create(
                    self.client_pool.get_by_addr(peer_addr).await?,
                    task_output_id.clone(),
                    prost_source.local_execute_plan.clone(),
                )
                .await?,
            ))
        }
    }
}

pub struct GenericExchangeExecutorBuilder {}

#[async_trait::async_trait]
impl BoxedExecutorBuilder for GenericExchangeExecutorBuilder {
    async fn new_boxed_executor<C: BatchTaskContext>(
        source: &ExecutorBuilder<'_, C>,
        inputs: Vec<BoxedExecutor>,
    ) -> Result<BoxedExecutor> {
        ensure!(
            inputs.is_empty(),
            "Exchange executor should not have children!"
        );
        let node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::Exchange
        )?;

        ensure!(!node.get_sources().is_empty());
        let prost_sources: Vec<ProstExchangeSource> = node.get_sources().to_vec();
        let source_creators =
            vec![DefaultCreateSource::new(source.context().client_pool()); prost_sources.len()];
        let mut sources: Vec<ExchangeSourceImpl> = vec![];

        for (prost_source, source_creator) in prost_sources.iter().zip_eq(source_creators) {
            let source = source_creator
                .create_source(source.context.clone(), prost_source)
                .await?;
            sources.push(source);
        }

        let input_schema: Vec<NodeField> = node.get_input_schema().to_vec();
        let fields = input_schema.iter().map(Field::from).collect::<Vec<Field>>();
        Ok(Box::new(GenericExchangeExecutor::<C> {
            sources,
            context: source.context().clone(),
            schema: Schema { fields },
            task_id: source.task_id.clone(),
            identity: source.plan_node().get_identity().clone(),
            metrics: source.context().get_task_metrics(),
        }))
    }
}

impl<C: BatchTaskContext> Executor for GenericExchangeExecutor<C> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute()
    }
}

impl<C: BatchTaskContext> GenericExchangeExecutor<C> {
    #[try_stream(boxed, ok = DataChunk, error = RwError)]
    async fn do_execute(self: Box<Self>) {
        let mut stream = select_all(
            self.sources
                .into_iter()
                .map(|source| {
                    data_chunk_stream(source, self.metrics.clone(), self.identity.clone())
                })
                .collect_vec(),
        )
        .boxed();

        while let Some(data_chunk) = stream.next().await {
            let data_chunk = data_chunk?;
            yield data_chunk
        }
    }
}

#[try_stream(boxed, ok = DataChunk, error = RwError)]
async fn data_chunk_stream(
    mut source: ExchangeSourceImpl,
    metrics: Option<BatchTaskMetrics>,
    identity: String,
) {
    // create the collector
    let source_id = source.get_task_id();
    let counter = if let Some(ref metrics) = metrics {
        let mut labels = metrics.task_labels();
        labels.insert("executor_id".to_string(), identity.clone());
        labels.insert(
            "source_query_id".to_string(),
            source_id.query_id.to_string(),
        );
        labels.insert(
            "source_stage_id".to_string(),
            source_id.stage_id.to_string(),
        );
        labels.insert("source_task_id".to_string(), source_id.task_id.to_string());

        let opts = Opts::new(
            "batch_exchange_recv_row_number",
            "Total number of row that have been received from upstream source",
        )
        .const_labels(labels);
        let counter = IntCounter::with_opts(opts).unwrap();
        metrics.register(Box::new(counter.clone()))?;
        Some(counter)
    } else {
        // no metrics to collect, no counter
        None
    };

    loop {
        if let Some(res) = source.take_data().await? {
            if res.cardinality() == 0 {
                debug!("Exchange source {:?} output empty chunk.", source);
            }

            if let Some(ref counter) = counter {
                counter.inc_by(res.cardinality().try_into().unwrap());
            }

            yield res;
            continue;
        }
        break;
    }

    if let (Some(counter), Some(metrics)) = (counter, metrics) {
        metrics.unregister(Box::new(counter));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use rand::Rng;
    use risingwave_common::array::column::Column;
    use risingwave_common::array::{DataChunk, I32Array};
    use risingwave_common::array_nonnull;
    use risingwave_common::types::DataType;

    use super::*;
    use crate::executor::test_utils::{FakeCreateSource, FakeExchangeSource};
    use crate::task::ComputeNodeContext;
    #[tokio::test]
    async fn test_exchange_multiple_sources() {
        let context = ComputeNodeContext::for_test();
        let mut sources = vec![];
        for _ in 0..2 {
            let mut rng = rand::thread_rng();
            let i = rng.gen_range(1..=100000);
            let chunk = DataChunk::new(
                vec![Column::new(Arc::new(
                    array_nonnull! { I32Array, [i] }.into(),
                ))],
                1,
            );
            let chunks = vec![Some(chunk); 100];
            let fake_exchange_source = FakeExchangeSource::new(chunks);
            let fake_create_source = FakeCreateSource::new(fake_exchange_source);
            let source = fake_create_source
                .create_source(context.clone(), &ProstExchangeSource::default())
                .await
                .unwrap();
            sources.push(source);
        }

        let executor = Box::new(GenericExchangeExecutor::<ComputeNodeContext> {
            metrics: None,
            sources,
            context,
            schema: Schema {
                fields: vec![Field::unnamed(DataType::Int32)],
            },
            task_id: TaskId::default(),
            identity: "GenericExchangeExecutor2".to_string(),
        });

        let mut stream = executor.execute();
        let mut chunks: Vec<DataChunk> = vec![];
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            chunks.push(chunk);
            if chunks.len() == 100 {
                chunks.dedup();
                assert_ne!(chunks.len(), 1);
                chunks.clear();
            }
        }
    }
}
