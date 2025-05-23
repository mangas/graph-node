use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use prometheus::Counter;
use prometheus::IntGauge;

use super::stopwatch::StopwatchMetrics;
use super::MetricsRegistry;
use crate::blockchain::block_stream::BlockStreamMetrics;
use crate::components::store::DeploymentLocator;
use crate::prelude::{Gauge, Histogram, HostMetrics};

pub struct SubgraphInstanceMetrics {
    pub block_trigger_count: Box<Histogram>,
    pub block_processing_duration: Box<Histogram>,
    pub block_ops_transaction_duration: Box<Histogram>,
    pub firehose_connection_errors: Counter,
    pub stopwatch: StopwatchMetrics,
    pub deployment_status: DeploymentStatusMetric,
    pub deployment_synced: DeploymentSyncedMetric,

    trigger_processing_duration: Box<Histogram>,
    blocks_processed_secs: Box<Counter>,
    blocks_processed_count: Box<Counter>,
}

impl SubgraphInstanceMetrics {
    pub fn new(
        registry: Arc<MetricsRegistry>,
        subgraph_hash: &str,
        stopwatch: StopwatchMetrics,
        deployment_status: DeploymentStatusMetric,
    ) -> Self {
        let block_trigger_count = registry
            .new_deployment_histogram(
                "deployment_block_trigger_count",
                "Measures the number of triggers in each block for a subgraph deployment",
                subgraph_hash,
                vec![1.0, 5.0, 10.0, 20.0, 50.0],
            )
            .expect("failed to create `deployment_block_trigger_count` histogram");
        let trigger_processing_duration = registry
            .new_deployment_histogram(
                "deployment_trigger_processing_duration",
                "Measures duration of trigger processing for a subgraph deployment",
                subgraph_hash,
                vec![0.01, 0.05, 0.1, 0.5, 1.5, 5.0, 10.0, 30.0, 120.0],
            )
            .expect("failed to create `deployment_trigger_processing_duration` histogram");
        let block_processing_duration = registry
            .new_deployment_histogram(
                "deployment_block_processing_duration",
                "Measures duration of block processing for a subgraph deployment",
                subgraph_hash,
                vec![0.05, 0.2, 0.7, 1.5, 4.0, 10.0, 60.0, 120.0, 240.0],
            )
            .expect("failed to create `deployment_block_processing_duration` histogram");
        let block_ops_transaction_duration = registry
            .new_deployment_histogram(
                "deployment_transact_block_operations_duration",
                "Measures duration of commiting all the entity operations in a block and updating the subgraph pointer",
                subgraph_hash,
                vec![0.01, 0.05, 0.1, 0.3, 0.7, 2.0],
            )
            .expect("failed to create `deployment_transact_block_operations_duration_{}");

        let firehose_connection_errors = registry
            .new_deployment_counter(
                "firehose_connection_errors",
                "Measures connections when trying to obtain a firehose connection",
                subgraph_hash,
            )
            .expect("failed to create firehose_connection_errors counter");

        let labels = HashMap::from_iter([
            ("deployment".to_string(), subgraph_hash.to_string()),
            ("shard".to_string(), stopwatch.shard().to_string()),
        ]);
        let blocks_processed_secs = registry
            .new_counter_with_labels(
                "deployment_blocks_processed_secs",
                "Measures the time spent processing blocks",
                labels.clone(),
            )
            .expect("failed to create blocks_processed_secs gauge");
        let blocks_processed_count = registry
            .new_counter_with_labels(
                "deployment_blocks_processed_count",
                "Measures the number of blocks processed",
                labels,
            )
            .expect("failed to create blocks_processed_count counter");

        let deployment_synced = DeploymentSyncedMetric::register(&registry, subgraph_hash);

        Self {
            block_trigger_count,
            block_processing_duration,
            block_ops_transaction_duration,
            firehose_connection_errors,
            stopwatch,
            deployment_status,
            deployment_synced,
            trigger_processing_duration,
            blocks_processed_secs,
            blocks_processed_count,
        }
    }

    pub fn observe_trigger_processing_duration(&self, duration: f64) {
        self.trigger_processing_duration.observe(duration);
    }

    pub fn observe_block_processed(&self, duration: Duration, block_done: bool) {
        self.blocks_processed_secs.inc_by(duration.as_secs_f64());
        if block_done {
            self.blocks_processed_count.inc();
        }
    }

    pub fn unregister(&self, registry: Arc<MetricsRegistry>) {
        registry.unregister(self.block_processing_duration.clone());
        registry.unregister(self.block_trigger_count.clone());
        registry.unregister(self.trigger_processing_duration.clone());
        registry.unregister(self.block_ops_transaction_duration.clone());
        registry.unregister(Box::new(self.deployment_synced.inner.clone()));
    }
}

#[derive(Debug)]
pub struct SubgraphCountMetric {
    pub running_count: Box<Gauge>,
    pub deployment_count: Box<Gauge>,
}

impl SubgraphCountMetric {
    pub fn new(registry: Arc<MetricsRegistry>) -> Self {
        let running_count = registry
            .new_gauge(
                "deployment_running_count",
                "Counts the number of deployments currently being indexed by the graph-node.",
                HashMap::new(),
            )
            .expect("failed to create `deployment_count` gauge");
        let deployment_count = registry
            .new_gauge(
                "deployment_count",
                "Counts the number of deployments currently deployed to the graph-node.",
                HashMap::new(),
            )
            .expect("failed to create `deployment_count` gauge");
        Self {
            running_count,
            deployment_count,
        }
    }
}

pub struct RunnerMetrics {
    /// Sensors to measure the execution of the subgraph instance
    pub subgraph: Arc<SubgraphInstanceMetrics>,
    /// Sensors to measure the execution of the subgraph's runtime hosts
    pub host: Arc<HostMetrics>,
    /// Sensors to measure the BlockStream metrics
    pub stream: Arc<BlockStreamMetrics>,
}

/// Reports the current indexing status of a deployment.
#[derive(Clone)]
pub struct DeploymentStatusMetric {
    inner: IntGauge,
}

impl DeploymentStatusMetric {
    const STATUS_STARTING: i64 = 1;
    const STATUS_RUNNING: i64 = 2;
    const STATUS_STOPPED: i64 = 3;
    const STATUS_FAILED: i64 = 4;

    /// Registers the metric.
    pub fn register(registry: &MetricsRegistry, deployment: &DeploymentLocator) -> Self {
        let deployment_status = registry
            .new_int_gauge(
                "deployment_status",
                "Indicates the current indexing status of a deployment.\n\
                 Possible values:\n\
                 1 - graph-node is preparing to start indexing;\n\
                 2 - deployment is being indexed;\n\
                 3 - indexing is stopped by request;\n\
                 4 - indexing failed;",
                [("deployment", deployment.hash.as_str())],
            )
            .expect("failed to register `deployment_status` gauge");

        Self {
            inner: deployment_status,
        }
    }

    /// Records that the graph-node is preparing to start indexing.
    pub fn starting(&self) {
        self.inner.set(Self::STATUS_STARTING);
    }

    /// Records that the deployment is being indexed.
    pub fn running(&self) {
        self.inner.set(Self::STATUS_RUNNING);
    }

    /// Records that the indexing is stopped by request.
    pub fn stopped(&self) {
        self.inner.set(Self::STATUS_STOPPED);
    }

    /// Records that the indexing failed.
    pub fn failed(&self) {
        self.inner.set(Self::STATUS_FAILED);
    }
}

/// Indicates whether a deployment has reached the chain head since it was deployed.
pub struct DeploymentSyncedMetric {
    inner: IntGauge,

    // If, for some reason, a deployment reports that it is synced, and then reports that it is not
    // synced during an execution, this prevents the metric from reverting to the not synced state.
    previously_synced: std::sync::OnceLock<()>,
}

impl DeploymentSyncedMetric {
    const NOT_SYNCED: i64 = 0;
    const SYNCED: i64 = 1;

    /// Registers the metric.
    pub fn register(registry: &MetricsRegistry, deployment_hash: &str) -> Self {
        let metric = registry
            .new_int_gauge(
                "deployment_synced",
                "Indicates whether a deployment has reached the chain head since it was deployed.\n\
                 Possible values:\n\
                 0 - deployment is not synced;\n\
                 1 - deployment is synced;",
                [("deployment", deployment_hash)],
            )
            .expect("failed to register `deployment_synced` gauge");

        Self {
            inner: metric,
            previously_synced: std::sync::OnceLock::new(),
        }
    }

    /// Records the current sync status of the deployment.
    /// Will ignore all values after the first `true` is received.
    pub fn record(&self, synced: bool) {
        if self.previously_synced.get().is_some() {
            return;
        }

        if synced {
            self.inner.set(Self::SYNCED);
            let _ = self.previously_synced.set(());
            return;
        }

        self.inner.set(Self::NOT_SYNCED);
    }
}
