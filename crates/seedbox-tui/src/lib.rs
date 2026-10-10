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

/// Run the console on the current terminal until the user quits. With `focus`,
/// open that simulation first; otherwise start in the catalogue.
pub async fn run(
    client: Client,
    focus: Option<SimulationId>,
    timeline_directory: std::path::PathBuf,
) -> std::io::Result<()> {
    let app = model::App::new(focus).with_timelines(timeline_directory)?;
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
