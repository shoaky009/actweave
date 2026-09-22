//! Run with `cargo run -p automation --example dialogue`.
use automation::{Control, Error, action::*, flow::*, recognition::*};

#[derive(Default)]
struct Demo {
    clicked: bool,
}
impl Backend for Demo {
    async fn capture(&mut self, _: &Control) -> Result<Frame, Error> {
        Frame::new(
            1,
            1,
            if self.clicked {
                vec![0, 0, 0]
            } else {
                vec![255, 0, 0]
            },
        )
    }
    async fn input(&mut self, input: &Input, _: &Control) -> Result<(), Error> {
        println!("mock input: {input:?}");
        self.clicked = true;
        Ok(())
    }
    async fn release_all(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pipeline: Flow = serde_json::from_str(
        r#"{
      "entry":"observe", "poll_interval_ms":10, "max_operations":20,
      "nodes":{
        "observe":{
          "recognition":{"type":"direct_hit","roi":null},
          "next":["finished","continue"], "timeout_ms":1000
        },
        "continue":{
          "recognition":{"type":"color_match","roi":null,"lower":[255,0,0],"upper":[255,0,0],"min_ratio":1.0},
          "actions":[{"type":"click","target":{"type":"match","index":0}}],
          "next":["finished","continue"], "timeout_ms":1000
        },
        "finished":{
          "recognition":{"type":"color_match","roi":null,"lower":[0,0,0],"upper":[0,0,0],"min_ratio":1.0},
          "timeout_ms":1000
        }
      }
    }"#,
    )?;
    let mut backend = Demo::default();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let control = Control::default();
    let mut runner = Runner::new(pipeline, &recognizers, &actions)?;
    loop {
        let progress = runner
            .step(&mut backend, &mut recognizers, &mut actions, &control)
            .await?;
        println!("{}", serde_json::to_string(progress)?);
        if progress.status != Status::Running {
            break;
        }
    }
    Ok(())
}
