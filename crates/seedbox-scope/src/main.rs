//! `seedbox-scope`: browse a run's faults, link policies and traffic over
//! simulated time. Reads only the files in a run directory.
mod model;
#[cfg(test)]
mod tests;
mod ui;

use clap::Parser;
use model::Run;
use ratatui::{
    backend::TestBackend,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    Terminal,
};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};
use ui::App;

/// Browse faults, link policies and traffic between machines in a seedbox run.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Run directory (or its output directory). [default: newest run in the state directory]
    run: Option<PathBuf>,
    /// State directory searched for the newest run. [default: $SEEDBOX_STATE or .seedbox]
    #[arg(long)]
    state: Option<PathBuf>,
    /// Start the cursor at this simulated time in nanoseconds.
    #[arg(long)]
    at_ns: Option<u64>,
    /// Keep the cursor at the end and reload while the run writes its files.
    #[arg(long)]
    follow: bool,
    /// Print one plain-text frame to stdout and exit, without a terminal.
    #[arg(long)]
    snapshot: bool,
    /// Frame width for --snapshot.
    #[arg(long, default_value_t = 140)]
    width: u16,
    /// Frame height for --snapshot.
    #[arg(long, default_value_t = 44)]
    height: u16,
}

fn newest_run(state: &Path) -> Result<PathBuf, String> {
    let runs = state.join("runs");
    let entries = std::fs::read_dir(&runs)
        .map_err(|e| format!("{}: {e}; pass a run directory", runs.display()))?;
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            let modified = e.metadata().ok()?.modified().ok()?;
            e.path().is_dir().then(|| (modified, e.path()))
        })
        .max()
        .map(|(_, p)| p)
        .ok_or_else(|| format!("{}: no runs; pass a run directory", runs.display()))
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let path = match args.run {
        Some(p) => p,
        None => {
            let state = args
                .state
                .or_else(|| std::env::var_os("SEEDBOX_STATE").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(".seedbox"));
            newest_run(&state)?
        }
    };
    let mut app = App::new(Run::load(&path)?, args.at_ns);
    app.follow = args.follow;
    if app.follow {
        app.at_ns = app.run.end_ns;
    }
    if args.snapshot {
        let mut terminal =
            Terminal::new(TestBackend::new(args.width, args.height)).map_err(|e| e.to_string())?;
        terminal
            .draw(|f| ui::draw(f, &app))
            .map_err(|e| e.to_string())?;
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            println!("{}", line.trim_end());
        }
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = interact(&mut terminal, &mut app, &path);
    ratatui::restore();
    result
}

fn interact(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    path: &Path,
) -> Result<(), String> {
    let mut last_reload = Instant::now();
    loop {
        terminal
            .draw(|f| ui::draw(f, app))
            .map_err(|e| e.to_string())?;
        if event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Right | KeyCode::Char('l') => app.next_stop(),
                    KeyCode::Left | KeyCode::Char('h') => app.previous_stop(),
                    KeyCode::Char(']') => app.nudge(true),
                    KeyCode::Char('[') => app.nudge(false),
                    KeyCode::Char('g') | KeyCode::Home => app.at_ns = 0,
                    KeyCode::Char('G') | KeyCode::End => app.at_ns = app.run.end_ns,
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => app.select(1),
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => app.select(-1),
                    KeyCode::Char('f') => app.follow = !app.follow,
                    KeyCode::Char('r') => reload(app, path),
                    _ => {}
                }
            }
        }
        if app.follow && last_reload.elapsed() >= Duration::from_secs(1) {
            reload(app, path);
            app.at_ns = app.run.end_ns;
            last_reload = Instant::now();
        }
    }
}

fn reload(app: &mut App, path: &Path) {
    match Run::load(path) {
        Ok(run) => {
            app.at_ns = app.at_ns.min(run.end_ns);
            app.selected = app.selected.min(run.machines.len().saturating_sub(1));
            app.run = run;
            app.error = None;
        }
        Err(e) => app.error = Some(e),
    }
}
