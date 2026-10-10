//! An interactive console for the simulation server. Every simulation,
//! machine, link and fault on screen is discovered through the HTTP API, so
//! worlds that gain or lose machines while running show up as they change.
#![cfg(unix)]
mod api;
mod model;
mod view;

use seedbox_api::SimulationId;
use seedbox_client::Client;

/// Run the console on the current terminal until the user quits. With `focus`,
/// open that simulation first; otherwise start in the catalogue.
pub async fn run(client: Client, focus: Option<SimulationId>) -> std::io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = api::drive(model::App::new(focus), client, &mut terminal).await;
    ratatui::try_restore()?;
    result
}
