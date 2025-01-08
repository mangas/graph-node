use std::time::Duration;

use crate::anyhow;
use crate::prelude::async_trait;
use crate::prelude::thiserror::Error;

#[derive(Debug)]
pub enum StepResult<StepInput: Send>
// where
// StepInput: std::fmt::Debug,
{
    Next(StepInput),
    // TODO: Figure out if we want this
    // RestartPipeline(StepInput),
    Restart(Option<Duration>),
    Halt(anyhow::Error),
}

#[async_trait]
pub trait EventHandler: Sync + Send {
    type Event: Send;
    type EventOutput: Send;

    async fn handle_event(&self, event: Self::Event) -> anyhow::Result<Self::EventOutput>;
}

#[async_trait]
pub trait Step {
    type Input: Send;
    type Output: Send;

    type Handler: EventHandler;

    async fn handle(&self, input: Self::Input, handler: &Self::Handler)
        -> StepResult<Self::Output>;
}

// Source will tick once the pipeline is ready for more data.
//
#[async_trait]
pub trait Source {
    type Output: Sync + Send;

    async fn tick(&self) -> StepResult<Self::Output>;
}

pub struct Pipeline<StepInput, EventHandler>
// where
// StepInput: std::fmt::Debug,
{
    source: Box<dyn Source<Output = StepInput>>,
    steps: Vec<Box<dyn Step<Input = StepInput, Output = StepInput, Handler = EventHandler>>>,
    handler: EventHandler,
    // async fn handle_event(evt: Self::Event) -> anyhow::Result<()>;
}

impl<StepInput, Handler> Pipeline<StepInput, Handler>
where
    Handler: EventHandler,
    StepInput: Send + Sync,
{
    async fn run(&self) -> anyhow::Result<StepInput> {
        loop {
            let mut next_input = match self.source.tick().await {
                StepResult::Next(next) => next,
                StepResult::Restart(Some(time_delta)) => {
                    tokio::time::sleep(time_delta).await;

                    continue;
                }
                StepResult::Restart(None) => continue,
                StepResult::Halt(error) => return Err(error),
            };

            for step in self.steps.iter() {
                match step.handle(next_input, &self.handler).await {
                    StepResult::Next(next) => next_input = next,
                    StepResult::Restart(Some(time_delta)) => {
                        tokio::time::sleep(time_delta).await;

                        break;
                    }
                    StepResult::Restart(None) => break,
                    StepResult::Halt(error) => return Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use tonic::async_trait;

    use crate::pipeline::{EventHandler, Pipeline, Source, Step, StepResult};

    #[tokio::test]
    async fn simple_pipeline() {
        struct NoopHandler;

        #[async_trait]
        impl EventHandler for NoopHandler {
            type Event = ();
            type EventOutput = ();

            async fn handle_event(&self, _event: Self::Event) -> anyhow::Result<Self::EventOutput> {
                Ok(())
            }
        }

        struct S;

        #[async_trait]
        impl Source for S {
            type Output = u64;

            async fn tick(&self) -> StepResult<Self::Output> {
                StepResult::Next(10)
            }
        }

        struct Step1;

        #[async_trait]
        impl Step for Step1 {
            type Input = u64;
            type Output = f64;
            type Handler = NoopHandler;

            async fn handle(
                &self,
                input: Self::Input,
                handler: &Self::Handler,
            ) -> StepResult<Self::Output> {
                todo!()
            }
        }

        let pipeline = Pipeline::<u64, NoopHandler> {
            source: Box::new(S {}),
            steps: todo!(),
            handler: NoopHandler {},
        };

        pipeline.run();
    }
}
