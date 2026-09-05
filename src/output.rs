use crate::{
    contracts::{Event, EventSink},
    usage::Usage,
};
use anyhow::Result;

#[derive(Clone, Copy)]
pub enum Mode {
    Json,
    Text,
}

pub struct PrintSink(pub Mode);
impl EventSink for PrintSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        match (self.0, event) {
            (Mode::Json, Event::Session(entry) | Event::Message(entry)) => println!("{entry}"),
            (Mode::Text, Event::FinalAnswer(message)) => {
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        if let Some(text) = block["text"].as_str() {
                            println!("{text}");
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn report_usage(usage: &Usage) {
    let summary = usage.summary();
    eprintln!("DANSO_USAGE={summary}");
    eprintln!("PIRI_USAGE={summary}");
}
