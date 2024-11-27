use crate::prelude::async_trait;
use crate::prelude::thiserror::Error;

#[derive(Debug, Error)]
pub enum StepResult<StepInput, StepOutput>
where
    StepInput: std::fmt::Debug,
    StepOutput: std::fmt::Debug,
{
    Next(StepOutput),
    RestartPipeline(StepInput),
    Halt(#[from] anyhow::Error),
}

pub trait Step {
    type Input;
    type Output;

    fn handle(&self, input: Self::Input) -> Self::Output;
}

// Source will tick once the pipeline is ready for more data.
//
pub trait Source {
    type Output;

    fn tick(&self) -> Self::Output;
}

pub struct Pipeline<StepInput, StepOutput, Event, EventResult>
where
    StepInput: std::fmt::Debug,
    StepOutput: std::fmt::Debug,
{
    source: Box<dyn Source<Output = StepResult<StepInput, StepOutput>>>,
    steps: Vec<Box<dyn Step<Input = StepInput, Output = StepOutput>>>,
    handler: Vec<Box<dyn FnOnce(Event, EventResult)>>,
    // async fn handle_event(evt: Self::Event) -> anyhow::Result<()>;
}

impl<StepInput, StepOutput, Event, EventResult> Pipeline<StepInput, StepOutput, Event, EventResult>
where
    StepInput: std::fmt::Debug,
    StepOutput: std::fmt::Debug,
{
    async fn run(&self) -> anyhow::Result<StepOutput> {
        let mut next_input = self.source.tick();
        loop {
            for step in self.steps.iter() {
                match step.handle(next_input) {
                    Next(_) => unimplemented!(),
                    RestartPipeline(_) => unimplemented!(),
                    Halt(_) => unimplemented!(),
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use crate::pipeline::Source;

    #[tokio::test]
    async fn pipeline() {
        struct S {}

        impl Source for S {}
    }
}
