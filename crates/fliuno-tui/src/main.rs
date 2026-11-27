mod editor;
mod options;
mod service;
mod task_browser;
mod ui;

use crossterm::{
    cursor::Show,
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use options::{Entry, Options};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal},
    time::Duration,
};

fn main() -> std::process::ExitCode {
    let result = Options::parse(std::env::args_os().skip(1)).and_then(|entry| {
        match entry {
            Entry::Help => println!("{}", options::HELP),
            Entry::Version => println!("fliuno {}", env!("CARGO_PKG_VERSION")),
            Entry::Run(options) => run(options).map_err(|error| error.to_string())?,
        }
        Ok(())
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fliuno: {}", ui::clean(&error));
            std::process::ExitCode::FAILURE
        }
    }
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    let _ = terminal::disable_raw_mode();
}

fn run(options: Options) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the workbench requires an interactive terminal",
        ));
    }
    let demo = options.demo;
    let theme = options.theme;
    let worker = service::Worker::spawn(options)?;
    let prior = std::panic::take_hook();
    let terminal_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == terminal_thread {
            restore();
        }
        prior(info);
    }));
    terminal::enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut screen = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = ui::App::new(demo, theme);
    let mut redraw = true;
    'running: loop {
        if app.drain(&worker) || redraw {
            screen.draw(|frame| app.draw(frame))?;
            redraw = false;
        }
        for queued in 0..64 {
            let timeout = if queued == 0 {
                Duration::from_millis(70)
            } else {
                Duration::ZERO
            };
            if !event::poll(timeout)? {
                break;
            }
            match event::read()? {
                Event::Key(key) => {
                    if app.key(key, &worker) {
                        break 'running;
                    }
                    redraw = true;
                }
                Event::Paste(text) => {
                    app.paste(&text);
                    redraw = true;
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        }
    }
    Ok(())
}
