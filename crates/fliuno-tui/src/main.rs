mod activity;
mod backup;
mod editor;
mod health;
mod options;
mod pipe;
mod recovery;
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
    let result = launch();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fliuno: {}", ui::clean(&error));
            std::process::ExitCode::FAILURE
        }
    }
}

fn launch() -> Result<(), String> {
    let parse = || Options::parse(std::env::args_os().skip(1));
    match parse()? {
        Entry::Help => println!("{}", options::HELP),
        Entry::Version => println!("fliuno {}", env!("CARGO_PKG_VERSION")),
        Entry::InitConfig(path) => {
            fliuno_config::initialize(&path)
                .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
            println!("{}", path.display());
        }
        Entry::Doctor(options) => {
            let report = health::inspect(&options, None, None);
            print!("{}", ui::clean(&report.text));
            if !report.healthy {
                return Err("configuration needs attention".into());
            }
        }
        Entry::Setup(options) => with_terminal(|screen| {
            if ui::setup::run(screen, options)? {
                let Entry::Run(options) = parse().map_err(io::Error::other)? else {
                    return Err(io::Error::other("The saved model profile is unavailable"));
                };
                run(options, screen)?;
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?,
        Entry::Run(options) => {
            with_terminal(|screen| run(options, screen)).map_err(|error| error.to_string())?
        }
    }
    Ok(())
}

type Screen = Terminal<CrosstermBackend<io::Stdout>>;

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

fn with_terminal(use_screen: impl FnOnce(&mut Screen) -> io::Result<()>) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the workbench requires an interactive terminal",
        ));
    }
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
    use_screen(&mut screen)
}

fn run(options: Options, screen: &mut Screen) -> io::Result<()> {
    let mut app = ui::App::new(options.demo, options.theme);
    let worker = service::Worker::spawn(options)?;
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
                Event::Paste(mut text) => {
                    app.paste(&text);
                    zeroize::Zeroize::zeroize(&mut text);
                    redraw = true;
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        }
    }
    Ok(())
}
