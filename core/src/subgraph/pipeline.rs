use std::{future::Future, marker::PhantomData, pin::Pin};

use graph::{
    blockchain::{block_stream::BlockStream, Blockchain},
    prelude::RunnerMetrics,
};

struct PipelineContext {
    metrics: RunnerMetrics,
}

impl PipelineContext {
    pub fn handle_event(evt: Event) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn block_stream<C: Blockchain>() -> anyhow::Result<Box<dyn BlockStream<C>>> {
        todo!()
    }
}

enum StreamPipelineEvent {
    ScanBlocksStarted,
}

// Maybe call this an action or something
enum Event {
    UpdateDeploymentMetric,
    StopIfPastMaxEndBlock,
    RebuildBlockStream,
    // StreamPipeline
    ScanBlocksStarted,
}

enum PipelineControl {
    Continue,
    Stop,
}

enum Step<E, F, P> {
    Event(E),
    Function(F),
    Pipeline(P),
}

type StepFn = Box<dyn FnOnce(PipelineContext) -> Pin<Box<dyn Future<Output = anyhow::Result<()>>>>>;

struct Pipeline<E> {
    init: Vec<Step<E, StepFn, Self>>,
    steps: Vec<Step<E, StepFn, Self>>,
}

fn stream_piepline() -> Pipeline<Event> {
    Pipeline {
        init: vec![Step::Event(Event::ScanBlocksStarted)],
        steps: vec![Step::Function(Box::new(move |ctx| {
            Box::pin(async move {
                ctx.metrics.subgraph.deployment_status.running();

                Ok(())
            })
        }))],
    }
}

fn runner_pipeline(stream_pipeline: Pipeline<Event>) -> Pipeline<Event> {
    Pipeline {
        init: vec![
            Step::Event(Event::UpdateDeploymentMetric),
            Step::Event(Event::StopIfPastMaxEndBlock),
        ],
        steps: vec![
            Step::Event(Event::UpdateDeploymentMetric),
            Step::Event(Event::StopIfPastMaxEndBlock),
            Step::Event(Event::RebuildBlockStream),
            Step::Function(Box::new(move |ctx| {
                Box::pin(async move {
                    ctx.metrics.subgraph.deployment_status.running();

                    Ok(())
                })
            })),
            Step::Pipeline(stream_pipeline),
        ],
    }
}
