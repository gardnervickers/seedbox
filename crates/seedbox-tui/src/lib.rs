//! An interactive console for the simulation server. Every simulation,
//! machine, link and fault on screen is discovered through the HTTP API, so
//! worlds that gain or lose machines while running show up as they change.
#![cfg(unix)]
mod activity;
mod api;
mod fault_form;
mod model;
mod timebar;
mod timeline;
mod timeline_view;
mod view;

use seedbox_api::SimulationId;
use seedbox_client::Client;

pub use model::{Request, Update};
pub use timeline::{TimelineDraft, TimelineMetadata};

/// Console state without a terminal. Observations and input use the same model
/// as the interactive driver; persistence remains owned by this console.
pub struct Console(model::App);
impl Console {
    pub fn new(
        focus: Option<SimulationId>,
        timeline_directory: std::path::PathBuf,
    ) -> std::io::Result<Self> {
        Ok(Self(
            model::App::new(focus).with_timelines(timeline_directory)?,
        ))
    }
    pub fn apply(&mut self, update: Update) -> Vec<Request> {
        self.0.apply(update)
    }
    pub fn input(&mut self, event: crossterm::event::Event) -> Vec<Request> {
        self.0.input(event)
    }
    pub fn timelines(&self) -> &[TimelineDraft] {
        &self.0.timelines
    }
    pub fn quit_requested(&self) -> bool {
        self.0.quit
    }
}

/// Run the console on the current terminal until the user quits. With `focus`,
/// open that simulation first; otherwise start in the catalogue.
pub async fn run(
    client: Client,
    focus: Option<SimulationId>,
    timeline_directory: std::path::PathBuf,
) -> std::io::Result<()> {
    let app = Console::new(focus, timeline_directory)?.0;
    let mut terminal = ratatui::try_init()?;
    if let Err(error) = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
    {
        ratatui::try_restore()?;
        return Err(error);
    }
    let result = api::drive(app, client, &mut terminal).await;
    let mouse_cleanup =
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    let terminal_cleanup = ratatui::try_restore();
    result.and(mouse_cleanup).and(terminal_cleanup)
}
