use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use serde::Deserialize;
use serde_json::Value;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_POINTS: usize = 10000;
const DEFAULT_URL: &str = "http://localhost:6333/telemetry?details_level=100";

#[derive(Parser)]
#[command(about = "Monitor Qdrant telemetry in real-time")]
struct Cli {
    /// Skip loading access.json (use default localhost URL)
    #[arg(long)]
    skip_access: bool,

    /// Show update queue length plot
    #[arg(long)]
    queue: bool,

    /// Show search latency plot
    #[arg(long)]
    search: bool,

    /// Show segment points (plain vs indexed) plot
    #[arg(long)]
    points: bool,

    /// Show segments count plot
    #[arg(long)]
    segments: bool,
}

impl Cli {
    fn plot_flags(&self) -> PlotFlags {
        let any_set = self.queue || self.search || self.points || self.segments;
        if any_set {
            PlotFlags {
                queue: self.queue,
                search: self.search,
                points: self.points,
                segments: self.segments,
            }
        } else {
            // defaults: all except points
            PlotFlags {
                queue: true,
                search: true,
                points: false,
                segments: true,
            }
        }
    }
}

#[derive(Clone, Copy)]
struct PlotFlags {
    queue: bool,
    search: bool,
    points: bool,
    segments: bool,
}

impl PlotFlags {
    fn count(&self) -> u32 {
        self.queue as u32 + self.search as u32 + self.points as u32 + self.segments as u32
    }
}

#[derive(Deserialize)]
struct AccessConfig {
    url: String,
    api_key: String,
}

fn load_access_config() -> Option<AccessConfig> {
    let path = Path::new("access.json");
    if path.exists() {
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

#[derive(PartialEq)]
enum Status {
    WaitingForProcess,
    Online,
    Offline,
}

#[derive(Clone, Copy, PartialEq)]
enum OptKind {
    None,      // Green  — no optimizing entries
    MergeOnly, // Red    — only merge
    IndexOnly, // Yellow — only indexing (or other non-merge)
    Both,      // Magenta — merge + indexing
}

struct App {
    // queue plot
    queue_lengths: Vec<(f64, f64)>,
    max_y_queue: f64,
    // search plot
    search_latencies: Vec<(f64, f64)>,
    filtered_search_latencies: Vec<(f64, f64)>,
    max_y_search: f64,
    max_y_filtered_search: f64,
    // points plot
    plain_points: Vec<(f64, f64)>,
    indexed_points: Vec<(f64, f64)>,
    max_y_points: f64,
    // segments plot
    total_segments: Vec<(f64, f64)>,
    plain_segments: Vec<(f64, f64)>,
    nonplain_segments: Vec<(f64, f64)>,
    pending_optimizations: Vec<(f64, f64)>,
    opt_statuses: Vec<(f64, OptKind)>,
    max_y_segments: f64,

    tick: f64,
    status: Status,
    recording: bool,
    last_json: Option<Value>,
    save_msg: Option<(String, Instant)>,
    url: String,
    api_key: Option<String>,
    is_remote: bool,
    plots: PlotFlags,
}

impl App {
    fn new(skip_access: bool, plots: PlotFlags) -> Self {
        let (url, api_key, is_remote) = if skip_access {
            (DEFAULT_URL.to_string(), None, false)
        } else {
            match load_access_config() {
                Some(config) => {
                    let url = format!(
                        "{}/telemetry?details_level=100",
                        config.url.trim_end_matches('/')
                    );
                    (url, Some(config.api_key), true)
                }
                None => (DEFAULT_URL.to_string(), None, false),
            }
        };

        Self {
            queue_lengths: Vec::new(),
            max_y_queue: 10.0,
            search_latencies: Vec::new(),
            filtered_search_latencies: Vec::new(),
            max_y_search: 100.0,
            max_y_filtered_search: 100.0,
            plain_points: Vec::new(),
            indexed_points: Vec::new(),
            max_y_points: 100.0,
            total_segments: Vec::new(),
            plain_segments: Vec::new(),
            nonplain_segments: Vec::new(),
            pending_optimizations: Vec::new(),
            opt_statuses: Vec::new(),
            max_y_segments: 10.0,
            tick: 0.0,
            status: Status::WaitingForProcess,
            recording: false,
            last_json: None,
            save_msg: None,
            url,
            api_key,
            is_remote,
            plots,
        }
    }

    fn reset(&mut self) {
        self.queue_lengths.clear();
        self.search_latencies.clear();
        self.filtered_search_latencies.clear();
        self.plain_points.clear();
        self.indexed_points.clear();
        self.total_segments.clear();
        self.plain_segments.clear();
        self.nonplain_segments.clear();
        self.pending_optimizations.clear();
        self.opt_statuses.clear();
        self.tick = 0.0;
        self.status = Status::WaitingForProcess;
        self.max_y_queue = 10.0;
        self.max_y_search = 100.0;
        self.max_y_filtered_search = 100.0;
        self.max_y_points = 100.0;
        self.max_y_segments = 10.0;
        self.recording = false;
        self.last_json = None;
        self.save_msg = None;
    }

    fn drop_first_half(&mut self) {
        fn drain_half<T>(v: &mut Vec<T>) {
            let mid = v.len() / 2;
            if mid > 0 {
                v.drain(..mid);
            }
        }
        drain_half(&mut self.queue_lengths);
        drain_half(&mut self.search_latencies);
        drain_half(&mut self.filtered_search_latencies);
        drain_half(&mut self.plain_points);
        drain_half(&mut self.indexed_points);
        drain_half(&mut self.total_segments);
        drain_half(&mut self.plain_segments);
        drain_half(&mut self.nonplain_segments);
        drain_half(&mut self.pending_optimizations);
        drain_half(&mut self.opt_statuses);
    }

    fn save_json(&mut self) {
        if let Some(json) = &self.last_json {
            let filename = format!("telemetry_{}.json", Local::now().format("%Y%m%d_%H%M%S"));
            match serde_json::to_string_pretty(json) {
                Ok(content) => match fs::write(&filename, content) {
                    Ok(_) => {
                        self.save_msg = Some((format!("Saved: {filename}"), Instant::now()));
                    }
                    Err(e) => {
                        self.save_msg = Some((format!("Save error: {e}"), Instant::now()));
                    }
                },
                Err(e) => {
                    self.save_msg = Some((format!("JSON error: {e}"), Instant::now()));
                }
            }
        } else {
            self.save_msg = Some(("No data to save yet".to_string(), Instant::now()));
        }
    }

    fn push_bounded(vec: &mut Vec<(f64, f64)>, val: (f64, f64)) {
        vec.push(val);
        if vec.len() > MAX_POINTS {
            vec.remove(0);
        }
    }

    fn apply_poll_result(&mut self, result: Result<Value, ()>) {
        match result {
            Ok(json) => {
                if let Some(len) = find_update_queue_length(&json) {
                    self.status = Status::Online;
                    self.last_json = Some(json);
                    if !self.recording && len > 0.0 {
                        self.recording = true;
                    }
                    let json_ref = self.last_json.as_ref().unwrap();

                    // queue
                    Self::push_bounded(&mut self.queue_lengths, (self.tick, len));
                    if len > self.max_y_queue {
                        self.max_y_queue = len;
                    }

                    // search
                    if let Some(latency) = find_query_batch_avg(json_ref) {
                        Self::push_bounded(&mut self.search_latencies, (self.tick, latency));
                        if latency > self.max_y_search {
                            self.max_y_search = latency;
                        }
                    }
                    if let Some(flt) = find_filtered_plain_sum(json_ref) {
                        Self::push_bounded(
                            &mut self.filtered_search_latencies,
                            (self.tick, flt),
                        );
                        if flt > self.max_y_filtered_search {
                            self.max_y_filtered_search = flt;
                        }
                    }

                    // points
                    let (plain, indexed) = find_segment_points(json_ref);
                    Self::push_bounded(&mut self.plain_points, (self.tick, plain));
                    Self::push_bounded(&mut self.indexed_points, (self.tick, indexed));
                    let points_max = plain.max(indexed);
                    if points_max > self.max_y_points {
                        self.max_y_points = points_max;
                    }

                    // segments
                    let (total, plain_seg, nonplain_seg) = count_segments(json_ref);
                    let pending = count_pending_optimizations(json_ref);
                    Self::push_bounded(&mut self.total_segments, (self.tick, total));
                    Self::push_bounded(&mut self.plain_segments, (self.tick, plain_seg));
                    Self::push_bounded(&mut self.nonplain_segments, (self.tick, nonplain_seg));
                    Self::push_bounded(
                        &mut self.pending_optimizations,
                        (self.tick, pending),
                    );
                    let opt_kind = detect_optimization_kind(json_ref);
                    self.opt_statuses.push((self.tick, opt_kind));
                    if self.opt_statuses.len() > MAX_POINTS {
                        self.opt_statuses.remove(0);
                    }
                    let seg_max = total.max(pending);
                    if seg_max > self.max_y_segments {
                        self.max_y_segments = seg_max;
                    }
                }
            }
            Err(_) => {
                if self.tick == 0.0 {
                    self.status = Status::WaitingForProcess;
                } else {
                    self.status = Status::Offline;
                }
            }
        }
        self.tick += 1.0;
    }
}

/// Extract avg_duration_micros from /qdrant.Points/QueryBatch in grpc responses.
fn find_query_batch_avg(value: &Value) -> Option<f64> {
    value
        .pointer("/result/requests/grpc/responses/~1qdrant.Points~1QueryBatch/avg_duration_micros")
        .and_then(|v| v.as_f64())
}

/// Sum filtered_plain.avg_duration_micros across all segments.
fn find_filtered_plain_sum(value: &Value) -> Option<f64> {
    let mut sum = 0.0;
    let mut found = false;
    for_each_segment(value, |seg| {
        if let Some(searches) = seg.get("vector_index_searches").and_then(|v| v.as_array()) {
            for entry in searches {
                if let Some(avg) = entry
                    .pointer("/filtered_plain/avg_duration_micros")
                    .and_then(|v| v.as_f64())
                {
                    sum += avg;
                    found = true;
                }
            }
        }
    });
    if found {
        Some(sum)
    } else {
        None
    }
}

/// Sum num_points across segments, split by plain vs non-plain index type.
/// Returns (plain_points, indexed_points).
fn find_segment_points(value: &Value) -> (f64, f64) {
    let mut plain = 0u64;
    let mut indexed = 0u64;
    for_each_segment(value, |seg| {
        let num_points = seg
            .pointer("/info/num_points")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if is_plain_segment(seg) {
            plain += num_points;
        } else {
            indexed += num_points;
        }
    });
    (plain as f64, indexed as f64)
}

/// Count segments: (total, plain, non-plain).
fn count_segments(value: &Value) -> (f64, f64, f64) {
    let mut total = 0u64;
    let mut plain = 0u64;
    let mut nonplain = 0u64;
    for_each_segment(value, |seg| {
        total += 1;
        if is_plain_segment(seg) {
            plain += 1;
        } else {
            nonplain += 1;
        }
    });
    (total as f64, plain as f64, nonplain as f64)
}

/// Count optimization log entries where status != "done".
fn count_pending_optimizations(value: &Value) -> f64 {
    let mut count = 0u64;
    if let Some(collections) = value.pointer("/result/collections/collections") {
        if let Some(arr) = collections.as_array() {
            for col in arr {
                if let Some(shards) = col.get("shards").and_then(|s| s.as_array()) {
                    for shard in shards {
                        if let Some(log) = shard
                            .pointer("/local/optimizations/log")
                            .and_then(|v| v.as_array())
                        {
                            for entry in log {
                                let status = entry
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if status != "done" {
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    count as f64
}

/// Inspect optimization log entries with status=="optimizing" and classify by name.
fn detect_optimization_kind(value: &Value) -> OptKind {
    let mut has_merge = false;
    let mut has_other = false;
    if let Some(collections) = value.pointer("/result/collections/collections") {
        if let Some(arr) = collections.as_array() {
            for col in arr {
                if let Some(shards) = col.get("shards").and_then(|s| s.as_array()) {
                    for shard in shards {
                        if let Some(log) = shard
                            .pointer("/local/optimizations/log")
                            .and_then(|v| v.as_array())
                        {
                            for entry in log {
                                let status = entry
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if status == "optimizing" {
                                    let name = entry
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if name == "merge" {
                                        has_merge = true;
                                    } else {
                                        has_other = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    match (has_merge, has_other) {
        (true, true) => OptKind::Both,
        (true, false) => OptKind::MergeOnly,
        (false, true) => OptKind::IndexOnly,
        (false, false) => OptKind::None,
    }
}

fn is_plain_segment(seg: &Value) -> bool {
    seg.pointer("/config/vector_data")
        .and_then(|vd| vd.as_object())
        .and_then(|map| map.values().next())
        .and_then(|v| v.pointer("/index/type"))
        .and_then(|v| v.as_str())
        == Some("plain")
}

fn for_each_segment(value: &Value, mut f: impl FnMut(&Value)) {
    if let Some(collections) = value.pointer("/result/collections/collections") {
        if let Some(arr) = collections.as_array() {
            for col in arr {
                if let Some(shards) = col.get("shards").and_then(|s| s.as_array()) {
                    for shard in shards {
                        if let Some(segments) =
                            shard.pointer("/local/segments").and_then(|s| s.as_array())
                        {
                            for seg in segments {
                                f(seg);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Recursively search the JSON for the first "update_queue" object and return its "length".
fn find_update_queue_length(value: &Value) -> Option<f64> {
    match value {
        Value::Object(map) => {
            if let Some(queue) = map.get("update_queue") {
                if let Some(len) = queue.get("length").and_then(|v| v.as_f64()) {
                    return Some(len);
                }
            }
            for v in map.values() {
                if let Some(len) = find_update_queue_length(v) {
                    return Some(len);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(len) = find_update_queue_length(v) {
                    return Some(len);
                }
            }
            None
        }
        _ => None,
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let plots = cli.plot_flags();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new(cli.skip_access, plots);

    // Run HTTP polling in a background thread so the UI stays responsive.
    let poll_url = app.url.clone();
    let poll_api_key = app.api_key.clone();
    let poll_result: Arc<Mutex<Option<Result<Value, ()>>>> = Arc::new(Mutex::new(None));
    let poll_result_writer = Arc::clone(&poll_result);
    let poll_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let poll_running_thread = Arc::clone(&poll_running);

    let poll_thread = std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(500))
            .build();
        while poll_running_thread.load(std::sync::atomic::Ordering::Relaxed) {
            let req = agent.get(&poll_url);
            let req = match &poll_api_key {
                Some(key) => req.set("api-key", key),
                None => req,
            };
            let result = match req.call() {
                Ok(resp) => match resp.into_json::<Value>() {
                    Ok(json) => Some(Ok(json)),
                    Err(_) => Some(Err(())),
                },
                Err(_) => Some(Err(())),
            };
            if let Ok(mut lock) = poll_result_writer.lock() {
                *lock = result;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });

    loop {
        // Check for new poll results from the background thread.
        if let Ok(mut lock) = poll_result.lock() {
            if let Some(result) = lock.take() {
                app.apply_poll_result(result);
            }
        }

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => app.reset(),
                        KeyCode::Char('s') => app.save_json(),
                        KeyCode::Char('d') => app.drop_first_half(),
                        _ => {}
                    }
                }
            }
        }
    }

    // Signal the background thread to stop and wait for it.
    poll_running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = poll_thread.join();

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let plots = app.plots;
    let n = plots.count();

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(3)];
    for _ in 0..n {
        constraints.push(Constraint::Ratio(1, n));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    // Show save message for 3 seconds if present
    let save_info = app.save_msg.as_ref().and_then(|(msg, when)| {
        if when.elapsed() < Duration::from_secs(3) {
            Some(format!(" | {msg}"))
        } else {
            None
        }
    });

    let mode = if app.is_remote { "REMOTE" } else { "LOCAL" };

    let (status_text, status_style) = match &app.status {
        Status::WaitingForProcess => (
            format!(
                "[{mode}] Waiting for Qdrant process...{}",
                save_info.unwrap_or_default()
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Status::Online => {
            let last_queue = app
                .queue_lengths
                .last()
                .map(|(_, v)| *v as u64)
                .unwrap_or(0);
            let last_search = app
                .search_latencies
                .last()
                .map(|(_, v)| format!("{:.0}us", v))
                .unwrap_or_else(|| "n/a".to_string());
            let recording = if app.recording {
                format!(" | samples: {}", app.queue_lengths.len())
            } else {
                " | waiting for non-zero".to_string()
            };
            (
                format!(
                    "[{mode}] Qdrant ONLINE | queue: {last_queue} | search: {last_search}{recording}{}",
                    save_info.unwrap_or_default()
                ),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        }
        Status::Offline => (
            format!(
                "[{mode}] Qdrant process OFFLINE — waiting for reconnect...{}",
                save_info.unwrap_or_default()
            ),
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let status_paragraph = Paragraph::new(status_text)
        .style(status_style)
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status_paragraph, chunks[0]);

    let mut idx = 1;

    if plots.queue {
        draw_chart(
            f,
            &app.queue_lengths,
            app.max_y_queue,
            "Update Queue Length (q: quit | r: reset | s: save json)",
            "queue length",
            "Length",
            Color::Cyan,
            chunks[idx],
        );
        idx += 1;
    }
    if plots.search {
        draw_search_chart(f, app, chunks[idx]);
        idx += 1;
    }
    if plots.points {
        draw_points_chart(f, app, chunks[idx]);
        idx += 1;
    }
    if plots.segments {
        draw_segments_chart(f, app, chunks[idx]);
        let _ = idx;
    }
}

fn draw_chart(
    f: &mut Frame,
    data: &[(f64, f64)],
    max_y: f64,
    title: &str,
    dataset_name: &str,
    y_title: &str,
    color: Color,
    area: Rect,
) {
    if data.is_empty() {
        let msg = Paragraph::new("No data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(title.to_string()));
        f.render_widget(msg, area);
        return;
    }

    let x_min = data.first().map(|(x, _)| *x).unwrap_or(0.0);
    let x_max = data.last().map(|(x, _)| *x).unwrap_or(1.0);
    let y_max = max_y.max(1.0);

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
        .name(dataset_name)
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(data);

    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
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
                .title(y_title)
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, y_max])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

fn draw_search_chart(f: &mut Frame, app: &App, area: Rect) {
    let title = "Search Avg Latency (us)";
    if app.search_latencies.is_empty() && app.filtered_search_latencies.is_empty() {
        let msg = Paragraph::new("No data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title),
            );
        f.render_widget(msg, area);
        return;
    }

    let all_data = app
        .search_latencies
        .iter()
        .chain(app.filtered_search_latencies.iter());
    let x_min = all_data.clone().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
    let x_max = all_data.map(|(x, _)| *x).fold(0.0f64, f64::max);
    let y_max = app.max_y_search.max(app.max_y_filtered_search).max(1.0);

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

    let ds_query = Dataset::default()
        .name("QueryBatch")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Magenta))
        .data(&app.search_latencies);

    let ds_filtered = Dataset::default()
        .name("filtered_plain")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Blue))
        .data(&app.filtered_search_latencies);

    let chart = Chart::new(vec![ds_query, ds_filtered])
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(
            Axis::default()
                .title("Time")
                .style(Style::default().fg(Color::Gray))
                .bounds([x_min, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("us")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, y_max])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

fn draw_points_chart(f: &mut Frame, app: &App, area: Rect) {
    let title = "Segment Points (plain vs indexed)";
    if app.plain_points.is_empty() && app.indexed_points.is_empty() {
        let msg = Paragraph::new("No data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(msg, area);
        return;
    }

    let all_data = app.plain_points.iter().chain(app.indexed_points.iter());
    let x_min = all_data.clone().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
    let x_max = all_data.map(|(x, _)| *x).fold(0.0f64, f64::max);
    let y_max = app.max_y_points.max(1.0);

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

    let ds_plain = Dataset::default()
        .name("plain")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Yellow))
        .data(&app.plain_points);

    let ds_indexed = Dataset::default()
        .name("indexed")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Green))
        .data(&app.indexed_points);

    let chart = Chart::new(vec![ds_plain, ds_indexed])
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(
            Axis::default()
                .title("Time")
                .style(Style::default().fg(Color::Gray))
                .bounds([x_min, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("Points")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, y_max])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

fn draw_segments_chart(f: &mut Frame, app: &App, area: Rect) {
    let title = "Segments & Pending Optimizations";
    if app.total_segments.is_empty() {
        let msg = Paragraph::new("No data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(msg, area);
        return;
    }

    // Split: chart gets most space, 1 row at the bottom for optimization status
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(area);
    let chart_area = chunks[0];
    let opt_area = chunks[1];

    let x_min = app.total_segments.first().map(|(x, _)| *x).unwrap_or(0.0);
    let x_max = app.total_segments.last().map(|(x, _)| *x).unwrap_or(1.0);
    let y_max = app.max_y_segments.max(1.0);

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

    let ds_total = Dataset::default()
        .name("total")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::White))
        .data(&app.total_segments);

    let ds_plain = Dataset::default()
        .name("plain")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Yellow))
        .data(&app.plain_segments);

    let ds_nonplain = Dataset::default()
        .name("non-plain")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Green))
        .data(&app.nonplain_segments);

    let ds_pending = Dataset::default()
        .name("pending-opt")
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Red))
        .data(&app.pending_optimizations);

    let chart = Chart::new(vec![ds_total, ds_plain, ds_nonplain, ds_pending])
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(
            Axis::default()
                .title("Time")
                .style(Style::default().fg(Color::Gray))
                .bounds([x_min, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("Segments")
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, y_max])
                .labels(y_labels),
        );
    f.render_widget(chart, chart_area);

    // Draw optimization status bar
    draw_opt_status_bar(f, app, opt_area, x_min, x_max);
}

fn opt_kind_color(kind: OptKind) -> Color {
    match kind {
        OptKind::None => Color::Green,
        OptKind::MergeOnly => Color::Red,
        OptKind::IndexOnly => Color::Yellow,
        OptKind::Both => Color::Magenta,
    }
}

fn draw_opt_status_bar(f: &mut Frame, app: &App, area: Rect, x_min: f64, x_max: f64) {
    if app.opt_statuses.is_empty() {
        return;
    }

    // "opt: █merge █index █both █idle  " — colored legend
    let dim = Style::default().fg(Color::DarkGray);
    let legend_spans: Vec<Span> = vec![
        Span::styled("opt: ", dim.add_modifier(Modifier::BOLD)),
        Span::styled("█", Style::default().fg(Color::Red)),
        Span::styled("merge ", dim),
        Span::styled("█", Style::default().fg(Color::Yellow)),
        Span::styled("index ", dim),
        Span::styled("█", Style::default().fg(Color::Magenta)),
        Span::styled("both ", dim),
        Span::styled("█", Style::default().fg(Color::Green)),
        Span::styled("idle ", dim),
    ];
    let label_width: usize = legend_spans.iter().map(|s| s.width()).sum();
    let bar_width = (area.width as usize).saturating_sub(label_width);
    if bar_width == 0 {
        return;
    }

    let x_range = (x_max - x_min).max(1.0);

    let mut spans: Vec<Span> = Vec::with_capacity(bar_width + legend_spans.len());
    spans.extend(legend_spans);

    for col in 0..bar_width {
        let t = x_min + (col as f64 / bar_width as f64) * x_range;
        let kind = find_nearest_opt_status(&app.opt_statuses, t);
        spans.push(Span::styled("█", Style::default().fg(opt_kind_color(kind))));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

fn find_nearest_opt_status(statuses: &[(f64, OptKind)], t: f64) -> OptKind {
    match statuses.binary_search_by(|(tick, _)| tick.partial_cmp(&t).unwrap()) {
        Ok(idx) => statuses[idx].1,
        Err(idx) => {
            if idx == 0 {
                statuses[0].1
            } else if idx >= statuses.len() {
                statuses[statuses.len() - 1].1
            } else {
                let prev = statuses[idx - 1];
                let next = statuses[idx];
                if (t - prev.0).abs() <= (next.0 - t).abs() {
                    prev.1
                } else {
                    next.1
                }
            }
        }
    }
}
