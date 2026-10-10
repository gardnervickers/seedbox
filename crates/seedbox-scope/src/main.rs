//! `seedbox-scope`: browse a run's faults, link policies and traffic over
//! simulated time. Reads only the files in a run directory.
mod map;
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
    io::Write,
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
    /// Keep colors in --snapshot output as ANSI escapes.
    #[arg(long, requires = "snapshot")]
    color: bool,
    /// Show links as a policy table instead of the network map.
    #[arg(long)]
    table: bool,
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
    app.grid = args.table;
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
        let mut out = std::io::stdout().lock();
        for y in 0..buffer.area.height {
            let cells = (0..buffer.area.width).map(|x| &buffer[(x, y)]);
            let line = if args.color {
                ansi_line(cells)
            } else {
                cells
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            };
            // A closed pipe (`| head`) just ends the output.
            if writeln!(out, "{line}").is_err() {
                break;
            }
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
                    KeyCode::Char('v') => app.grid = !app.grid,
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

/// One buffer row with SGR escapes for its colors, bold and reverse video.
fn ansi_line<'a>(cells: impl Iterator<Item = &'a ratatui::buffer::Cell>) -> String {
    use ratatui::style::{Color, Modifier};
    fn sgr(color: Color, base: u8) -> String {
        let named = |i: u8| format!("{}", if i < 8 { base + i } else { base + 60 + i - 8 });
        match color {
            Color::Reset => format!("{}", base + 9),
            Color::Black => named(0),
            Color::Red => named(1),
            Color::Green => named(2),
            Color::Yellow => named(3),
            Color::Blue => named(4),
            Color::Magenta => named(5),
            Color::Cyan => named(6),
            Color::Gray => named(7),
            Color::DarkGray => named(8),
            Color::LightRed => named(9),
            Color::LightGreen => named(10),
            Color::LightYellow => named(11),
            Color::LightBlue => named(12),
            Color::LightMagenta => named(13),
            Color::LightCyan => named(14),
            Color::White => named(15),
            Color::Indexed(i) => format!("{};5;{i}", base + 8),
            Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        }
    }
    let mut out = String::new();
    let mut last = None;
    for cell in cells {
        let style = (cell.fg, cell.bg, cell.modifier);
        if last != Some(style) {
            let mut codes = vec!["0".to_owned(), sgr(cell.fg, 30), sgr(cell.bg, 40)];
            if cell.modifier.contains(Modifier::BOLD) {
                codes.push("1".into());
            }
            if cell.modifier.contains(Modifier::REVERSED) {
                codes.push("7".into());
            }
            out += &format!("\x1b[{}m", codes.join(";"));
            last = Some(style);
        }
        out += cell.symbol();
    }
    out + "\x1b[0m"
}
