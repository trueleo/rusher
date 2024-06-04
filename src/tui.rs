use std::{
    error::Error,
    io,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crossterm::event::KeyCode;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Style, Stylize},
    symbols,
    terminal::{Frame, Terminal, Viewport},
    text::{Line, Span, Text},
    widgets::{Block, Borders, LineGauge, Paragraph},
    TerminalOptions,
};

const LOGO: &str = "\
╔═══╗╔╗ ╔╗╔═══╗╔╗ ╔╗╔═══╗╔═══╗
║╔═╗║║║ ║║║╔═╗║║║ ║║║╔══╝║╔═╗║
║╚═╝║║║ ║║║╚══╗║╚═╝║║╚══╗║╚═╝║
║╔╗╔╝║║ ║║╚══╗║║╔═╗║║╔══╝║╔╗╔╝
║║║╚╗║╚═╝║║╚═╝║║║ ║║║╚══╗║║║╚╗
╚╝╚═╝╚═══╝╚═══╝╚╝ ╚╝╚═══╝╚╝╚═╝\
";

const BUNNY: &str = "  //
 ('>
 /rr
*\\))_";

const INFO_CELL_SIZE: usize = 15;

#[derive(Debug)]
enum Event {
    Input(crossterm::event::KeyEvent),
    Tick,
    Resize,
    Message(crate::tracing::Message),
}

#[derive(Debug, Default)]
struct ExecutorState {
    name: String,
    users: u64,
    max_users: u64,
    iterations: u64,
    total_iteration: Option<u64>,
    duration: Option<Duration>,
    total_duration: Option<Duration>,
    task_min_time: Duration,
    task_max_time: Duration,
    task_total_time: Duration,
}

pub struct Scenario {
    name: String,
    execs: Vec<ExecutorState>,
}

impl Scenario {
    pub fn new_from_scenario(scenario: &crate::logical::Scenario<'_>) -> Self {
        let name = scenario.name.clone();
        let execs = scenario
            .execution_provider
            .iter()
            .map(|exec| ExecutorState {
                name: exec.name().to_string(),
                ..Default::default()
            })
            .collect();

        Self { name, execs }
    }

    fn exec_names(&self) -> impl Iterator<Item = &str> {
        self.execs.iter().map(|x| &*x.name)
    }

    fn update(&mut self, message: &crate::tracing::Message) {
        match message {
            crate::tracing::Message::TaskTime {
                exec_name,
                duration,
                ..
            } => {
                if let Some(exec) = self.execs.iter_mut().find(|x| *x.name == **exec_name) {
                    exec.task_max_time = exec.task_max_time.max(*duration);
                    if exec.task_min_time == Duration::default() {
                        exec.task_min_time = *duration;
                    } else {
                        exec.task_min_time = exec.task_min_time.min(*duration);
                    }
                    exec.task_total_time += *duration;
                }
            }
            crate::tracing::Message::ExecutorUpdate {
                name,
                users,
                max_users,
                iterations,
                total_iteration,
                duration,
                total_duration,
            } => {
                if let Some(exec) = self.execs.iter_mut().find(|x| x.name == name.to_string()) {
                    exec.users = *users;
                    exec.max_users = *max_users;
                    exec.iterations = *iterations;
                    exec.duration = *duration;
                    exec.total_duration = *total_duration;
                    exec.total_iteration = *total_iteration;
                }
            }
            _ => {}
        }
    }
}

struct App {
    current_scenario: usize,
    current_exec: usize,
    scenarios: Vec<Scenario>,
}

impl App {
    fn current_scenario(&self) -> &Scenario {
        &self.scenarios[self.current_scenario]
    }

    fn current_exec(&self) -> &ExecutorState {
        &self.scenarios[self.current_scenario].execs[self.current_exec]
    }
}

pub fn run(
    mut tracing_messages: crate::Receiver<crate::tracing::Message>,
    scenarios: Vec<Scenario>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    crossterm::terminal::enable_raw_mode()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(20),
        },
    )?;

    let (tx, rx) = mpsc::channel();

    let app = App {
        current_scenario: 0,
        scenarios,
        current_exec: 0,
    };

    input_handling(tx.clone());

    thread::spawn(move || loop {
        match tracing_messages.try_next() {
            Ok(Some(message)) => {
                let _ = tx.send(Event::Message(message));
            }
            Ok(None) => break,
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    });

    thread::scope(|s| {
        let handler = s.spawn(|| run_app(&mut terminal, app, rx));
        handler.join().unwrap()
    })?;

    let size = terminal.get_frame().size();
    terminal.set_cursor(size.width, size.height + size.y)?;
    crossterm::terminal::disable_raw_mode()?;

    Ok(())
}

fn input_handling(tx: mpsc::Sender<Event>) -> thread::JoinHandle<()> {
    let tick_rate = Duration::from_millis(400);
    thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            let timeout = tick_rate.saturating_sub(last_tick.elapsed());
            if crossterm::event::poll(timeout).unwrap() {
                match crossterm::event::read().unwrap() {
                    crossterm::event::Event::Key(key) => tx.send(Event::Input(key)).unwrap(),
                    crossterm::event::Event::Resize(_, _) => tx.send(Event::Resize).unwrap(),
                    _ => {}
                };
            }
            if last_tick.elapsed() >= tick_rate {
                if tx.send(Event::Tick).is_err() {
                    break;
                }
                last_tick = Instant::now();
            }
        }
    })
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    rx: mpsc::Receiver<Event>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut events: Vec<Event> = Vec::new();
    'a: loop {
        // Catch early if the receiver is ded.
        let event = rx.recv()?;
        events.push(event);
        // consume all events
        rx.try_iter().for_each(|x| events.push(x));
        for event in &events {
            match event {
                Event::Input(event) => match event.code {
                    KeyCode::Char('q') | KeyCode::Char('c')
                        if event.modifiers == crossterm::event::KeyModifiers::CONTROL =>
                    {
                        break 'a;
                    }
                    KeyCode::Up => {
                        app.current_exec =
                            (app.current_exec + 1).min(app.current_scenario().execs.len() - 1)
                    }
                    KeyCode::Down => app.current_exec = app.current_exec.saturating_sub(1),
                    _ => (),
                },
                Event::Resize => {
                    terminal.autoresize()?;
                }
                Event::Tick => {
                    terminal.draw(|f| ui(f, &app))?;
                }
                Event::Message(message) => match message {
                    crate::tracing::Message::ScenarioChanged { scenario_name } => {
                        app.current_scenario = app
                            .scenarios
                            .iter()
                            .position(|scenario| scenario.name == *scenario_name)
                            .unwrap();
                    }
                    crate::tracing::Message::End => {
                        // redraw for the last time
                        terminal.draw(|f| ui(f, &app))?;
                        break 'a;
                    }
                    _ => app.scenarios[app.current_scenario].update(message),
                },
            }
        }
        events.clear();
    }
    Ok(())
}

fn ui(f: &mut Frame, app: &App) {
    let current_scenario = app.current_scenario();
    let area = f.size();

    let scenario_text = Text::from(vec![Line::from(vec![
        "Scenario - ".to_string().bold(),
        current_scenario.name.to_string().into(),
    ])]);

    let mut executors_text = Text::from(Line::from("Executors: ".to_string().bold()));
    for (index, exec) in current_scenario.exec_names().enumerate() {
        executors_text.push_line(Line::from_iter([
            if index == app.current_exec {
                Span::from("* ").bold()
            } else {
                Span::from("* ")
            },
            Span::raw(exec),
        ]))
    }

    let current_exec = app.current_exec();
    let average_time = current_exec
        .task_total_time
        .checked_div(current_exec.iterations as u32)
        .unwrap_or_default();
    let max_time = current_exec.task_max_time;
    let min_time = current_exec.task_min_time;

    // No margins here. Margins are applied by children of the main area
    let [left_area, other_info] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(area);

    // Draw borders
    f.render_widget(Block::bordered().borders(Borders::RIGHT), left_area);

    // Left Area
    let [logo_area, scenario_area, executors_area] = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .vertical_margin(1)
    .horizontal_margin(2)
    .areas(left_area);

    let bunny_text = Text::from(BUNNY);
    let bunny_area = Rect::new(
        executors_area.x + executors_area.width - bunny_text.width() as u16,
        executors_area.y + executors_area.height - bunny_text.height() as u16,
        bunny_text.width() as u16,
        bunny_text.height() as u16,
    );

    if (executors_area.width as usize).saturating_sub(executors_text.width()) >= bunny_text.width()
        || (executors_area.height as usize).saturating_sub(executors_text.height())
            >= bunny_text.height()
    {
        f.render_widget(bunny_text, bunny_area);
    }

    f.render_widget(Paragraph::new(LOGO), logo_area);
    f.render_widget(Block::bordered().borders(Borders::BOTTOM), logo_area);
    f.render_widget(scenario_text, scenario_area);
    f.render_widget(executors_text, executors_area);

    let total_users_formatted = current_exec.users.to_string();
    let total_max_users_formatted = current_exec.max_users.to_string();
    let average_time_formatted = format!("{:?}", average_time);
    let max_time_formatted = format!("{:?}", max_time);
    let min_time_formatted = format!("{:?}", min_time);
    let total_iterations_completed_formattted = current_exec.iterations.to_string();

    let info_render = [
        ("users", Line::from_iter(value_span(&total_users_formatted))),
        (
            "max_users",
            Line::from_iter(value_span(&total_max_users_formatted)),
        ),
        (
            "iteration_time",
            Line::from_iter(
                key_value_span("avg", &average_time_formatted)
                    .into_iter()
                    .chain(key_value_span("max", &max_time_formatted))
                    .chain(key_value_span("min", &min_time_formatted)),
            ),
        ),
        (
            "iterations",
            Line::from_iter(key_value_span(
                "total",
                &total_iterations_completed_formattted,
            )),
        ),
    ];

    let key_size = info_render.iter().map(|(k, _)| k.len()).max().unwrap() + 3;
    let [mut progress_bar_area, other_info_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)])
            .margin(1)
            .horizontal_margin(2)
            .areas(other_info);

    progress_bar_area = progress_bar_area.inner(&Margin::new(2, 0));
    progress_bar_area.width = progress_bar_area.width.min(60);

    let progress = if let Some((total_duration, duration)) =
        current_exec.total_duration.zip(current_exec.duration)
    {
        LineGauge::default()
            .label(format!("{duration:?}/{total_duration:?}"))
            .ratio(duration.as_secs_f64() / total_duration.as_secs_f64())
    } else if let Some(total_iteration) = current_exec.total_iteration {
        let iteration = current_exec.iterations;
        LineGauge::default()
            .label(format!("{iteration}/{total_iteration}"))
            .ratio(iteration as f64 / total_iteration as f64)
    } else {
        LineGauge::default().label("?/???")
    }
    .gauge_style(Style::default().fg(Color::Green))
    .style(Style::default().fg(Color::Blue))
    .line_set(symbols::line::THICK);

    f.render_widget(progress, progress_bar_area);

    let other_info = Layout::vertical(Constraint::from_lengths(
        std::iter::repeat(1).take(info_render.len()),
    ))
    .vertical_margin(1)
    .horizontal_margin(2)
    .spacing(1)
    .split(other_info_area);

    for (i, (key, mut info)) in info_render.into_iter().enumerate() {
        let mut padded_key = format!("{:.<width$}", key, width = key_size);
        padded_key.push(':');
        info.spans.insert(0, Span::raw(padded_key));
        info.spans.insert(1, Span::raw(" "));

        f.render_widget(info, other_info[i]);
    }
}

fn padding(n: usize) -> String {
    String::from_iter(std::iter::repeat(' ').take(n))
}

fn key_value_span<'a>(key: &'a str, value: &'a str) -> [Span<'a>; 4] {
    [
        Span::raw(key).bold(),
        Span::raw("=").bold(),
        Span::raw(value).bold().blue(),
        Span::raw(padding(
            INFO_CELL_SIZE
                .saturating_sub(1 + key.len() + value.len())
                .max(1),
        )),
    ]
}

fn value_span(value: &str) -> [Span<'_>; 2] {
    [
        Span::raw(value).bold().blue(),
        Span::raw(padding(INFO_CELL_SIZE - value.len())),
    ]
}
