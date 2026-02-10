use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use serde::Deserialize;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_POINTS: usize = 600; // ~60 seconds of history
const URL: &str = "http://localhost:6333/collections/benchmark";

#[derive(Deserialize)]
struct ApiResponse {
    result: Option<CollectionResult>,
}

#[derive(Deserialize)]
struct CollectionResult {
    update_queue: UpdateQueue,
}

#[derive(Deserialize)]
struct UpdateQueue {
    length: u64,
}

#[derive(PartialEq)]
enum Status {
    WaitingForProcess,
    Online,
    Offline,
}

struct App {
    queue_lengths: Vec<(f64, f64)>,
    tick: f64,
    status: Status,
    max_y: f64,
}

impl App {
    fn new() -> Self {
        Self {
            queue_lengths: Vec::new(),
            tick: 0.0,
            status: Status::WaitingForProcess,
            max_y: 10.0,
        }
    }

    fn poll(&mut self) {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(500))
            .build();

        match agent.get(URL).call() {
            Ok(resp) => {
                if let Ok(api) = resp.into_json::<ApiResponse>() {
                    if let Some(result) = api.result {
                        let len = result.update_queue.length as f64;
                        self.status = Status::Online;
                        self.queue_lengths.push((self.tick, len));
                        if self.queue_lengths.len() > MAX_POINTS {
                            self.queue_lengths.remove(0);
                        }
                        if len > self.max_y {
                            self.max_y = len;
                        }
                    }
                }
            }
            Err(_) => {
                if self.queue_lengths.is_empty() {
                    self.status = Status::WaitingForProcess;
                } else {
                    self.status = Status::Offline;
                }
            }
        }
        self.tick += 1.0;
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new();
    let mut last_poll = Instant::now() - POLL_INTERVAL;

    loop {
        if last_poll.elapsed() >= POLL_INTERVAL {
            app.poll();
            last_poll = Instant::now();
        }

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc)
                {
                    break;
                }
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(10)])
        .split(f.area());

    let (status_text, status_style) = match &app.status {
        Status::WaitingForProcess => (
            "Waiting for Qdrant process...".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Status::Online => {
            let last_val = app
                .queue_lengths
                .last()
                .map(|(_, v)| *v as u64)
                .unwrap_or(0);
            (
                format!(
                    "Qdrant ONLINE | queue length: {} | samples: {}",
                    last_val,
                    app.queue_lengths.len()
                ),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        }
        Status::Offline => (
            "Qdrant process OFFLINE — waiting for reconnect...".to_string(),
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let status_paragraph = Paragraph::new(status_text)
        .style(status_style)
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status_paragraph, chunks[0]);

    draw_chart(f, app, chunks[1]);
}

fn draw_chart(f: &mut Frame, app: &App, area: Rect) {
    if app.queue_lengths.is_empty() {
        let msg = Paragraph::new("No data yet. Press 'q' to quit.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Update Queue Length"),
            );
        f.render_widget(msg, area);
        return;
    }

    let x_min = app
        .queue_lengths
        .first()
        .map(|(x, _)| *x)
        .unwrap_or(0.0);
    let x_max = app.queue_lengths.last().map(|(x, _)| *x).unwrap_or(1.0);
    let y_max = app.max_y.max(1.0);

    let x_labels = vec![
        Span::raw(format!("{:.1}s", x_min * 0.1)),
        Span::raw(format!("{:.1}s", ((x_min + x_max) / 2.0) * 0.1)),
        Span::raw(format!("{:.1}s", x_max * 0.1)),
    ];
    let y_labels = vec![
        Span::raw("0"),
        Span::raw(format!("{:.0}", y_max / 2.0)),
        Span::raw(format!("{:.0}", y_max)),
    ];

    let dataset = Dataset::default()
        .name("queue length")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&app.queue_lengths);

    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Update Queue Length (press 'q' to quit)"),
        )
        .x_axis(
            Axis::default()
                .title("Time")
                .style(Style::default().fg(Color::Gray))
                .bounds([x_min, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("Length")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, y_max])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}
