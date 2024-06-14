use std::collections::VecDeque;

use ordered_float::OrderedFloat;
use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols,
    text::{Line, Span, Text},
    widgets::{Axis, BarChart, Block, Borders, Chart, Dataset, Gauge, GraphType, Paragraph},
    Frame,
};

use crate::tracing::task_event::{
    metrics::{MetricType, MetricValue},
    MetricSetKey,
};

use super::{App, ExecutorState};

const LOGO: &str = "\
╔═══╗╔╗ ╔╗╔═══╗╔╗ ╔╗╔═══╗╔═══╗
║╔═╗║║║ ║║║╔═╗║║║ ║║║╔══╝║╔═╗║
║╚═╝║║║ ║║║╚══╗║╚═╝║║╚══╗║╚═╝║
║╔╗╔╝║║ ║║╚══╗║║╔═╗║║╔══╝║╔╗╔╝
║║║╚╗║╚═╝║║╚═╝║║║ ║║║╚══╗║║║╚╗
╚╝╚═╝╚═══╝╚═══╝╚╝ ╚╝╚═══╝╚╝╚═╝\
";
const INFO_CELL_SIZE: usize = 13;

struct Size {
    height: u16,
    width: u16,
}

fn logo() -> (Size, fn(&mut Frame, Rect)) {
    let f = |f: &mut Frame, rect: Rect| {
        f.render_widget(Paragraph::new(LOGO), rect);
    };

    (
        Size {
            height: 6,
            width: 7,
        },
        f,
    )
}

fn scenario_text(name: &str) -> (Size, impl FnOnce(&mut Frame, Rect) + '_) {
    let scenario_text = Line::from(vec!["Scenario - ".to_string().bold(), name.into()]);
    let width = scenario_text.width() as u16;
    let f = move |f: &mut Frame, rect: Rect| {
        f.render_widget(scenario_text, rect);
    };

    (Size { height: 1, width }, f)
}

fn executor_text<'a>(
    current_exec: usize,
    exec_names: impl Iterator<Item = &'a str>,
) -> (Size, impl FnOnce(&mut Frame, Rect) + 'a) {
    let mut executors_text = Text::from(Line::from("Executors: ".to_string().bold()));
    for (index, exec) in exec_names.enumerate() {
        let mut line = Line::from_iter([
            if index == current_exec {
                Span::from(symbols::DOT).bold()
            } else {
                Span::from(symbols::DOT)
            },
            Span::from(" "),
            Span::raw(exec),
        ]);

        if index == current_exec {
            line = line.light_green();
        }

        executors_text.push_line(line)
    }

    let width = executors_text.width() as u16;
    let height = executors_text.height() as u16;

    let f = move |f: &mut Frame, rect: Rect| {
        f.render_widget(executors_text, rect);
    };

    (Size { height, width }, f)
}

fn progress_bar(current: &ExecutorState) -> (Size, impl FnOnce(&mut Frame, Rect)) {
    let progress = if let Some(total_duration) = current.total_duration {
        let duration = &current.duration;
        Gauge::default()
            .label(format!("{duration:?}/{total_duration:?}"))
            .ratio((duration.as_secs_f64() / total_duration.as_secs_f64()).min(1f64))
    } else if let Some(total_iteration) = current.total_iteration {
        let iteration = current.iterations;
        Gauge::default()
            .label(format!("{iteration}/{total_iteration}"))
            .ratio((iteration as f64 / total_iteration as f64).min(1f64))
    } else {
        Gauge::default().label("?/???")
    }
    .gauge_style(Style::default().fg(Color::Green).bg(Color::Gray));

    let f = move |f: &mut Frame, rect: Rect| {
        f.render_widget(progress, rect);
    };

    (
        Size {
            height: 1,
            width: 60,
        },
        f,
    )
}

fn other_info(current: &ExecutorState) -> (Size, impl FnOnce(&mut Frame, Rect) + '_) {
    let average_time = current
        .task_total_time
        .checked_div(current.iterations as u32)
        .unwrap_or_default();

    let total_users_formatted = current.users.to_string();
    let total_max_users_formatted = current.max_users.to_string();
    let average_time_formatted = format!("{:.2?}", average_time);
    let max_time_formatted = format!("{:.2?}", current.task_max_time);
    let min_time_formatted = format!("{:.2?}", current.task_min_time);
    let total_iterations_completed_formatted = current.iterations.to_string();
    let iteration_per_sec_formatted = format!(
        "{:.2} iter/sec",
        current.iterations as f64 / current.duration.as_secs_f64()
    );

    let stages_formatted = current.stages.map(|x| x.to_string());
    let stage_formatted = current.stage.map(|x| x.to_string());
    let stage_duration_formatted = current
        .stage_duration
        .map(|duration| format!("{:.2?}", duration));

    let mut info_render = Vec::default();

    if let Some(stages) = stages_formatted {
        let line = if let Some((stage, duration)) = stage_formatted.zip(stage_duration_formatted) {
            Line::from_iter(
                value_span(stage)
                    .into_iter()
                    .chain(key_value_span("total", stages))
                    .chain(key_value_span("duration", duration)),
            )
        } else {
            Line::from_iter(key_value_span("total", stages))
        };
        info_render.push(("current_stage", line))
    }

    info_render.extend([
        ("users", Line::from_iter(value_span(total_users_formatted))),
        (
            "max_users",
            Line::from_iter(value_span(total_max_users_formatted)),
        ),
        (
            "iteration_time",
            Line::from_iter(
                key_value_span("avg", average_time_formatted)
                    .into_iter()
                    .chain(key_value_span("max", max_time_formatted))
                    .chain(key_value_span("min", min_time_formatted)),
            ),
        ),
        (
            "iterations",
            Line::from_iter(
                key_value_span("total", total_iterations_completed_formatted)
                    .into_iter()
                    .chain(value_span(iteration_per_sec_formatted)),
            ),
        ),
    ]);

    let key_size = info_render.iter().map(|(k, _)| k.len()).max().unwrap() + 2;

    let mut paragraph = Text::default();

    for (i, (key, mut info)) in info_render.into_iter().enumerate() {
        if i != 0 {
            paragraph.lines.push(Line::default());
        }
        let padded_key = format!("{:.<width$}:", key, width = key_size);
        info.spans.insert(0, Span::raw(padded_key));
        info.spans.insert(1, Span::raw(" "));
        paragraph.lines.push(info)
    }

    let size = Size {
        height: paragraph.height() as u16,
        width: paragraph.width() as u16,
    };

    let f = move |f: &mut Frame, rect: Rect| {
        f.render_widget(paragraph, rect);
    };

    (size, f)
}

fn render_gauge<'a>(
    key: &MetricSetKey,
    value: impl Iterator<Item = &'a MetricValue>,
    f: &mut Frame,
    area: Rect,
) {
    let data_points: Vec<(f64, f64)> = value
        .enumerate()
        .map(|(x, y)| {
            let y = match y {
                MetricValue::Gauge(x) => *x,
                _ => 0.,
            };
            (x as f64, y)
        })
        .collect();

    let data = Dataset::default()
        .name("metrics")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .data(&data_points);

    // Create the X axis and define its properties
    let x_axis = Axis::default()
        .title("X Axis".red())
        .bounds([0.0, 15.0])
        .labels(vec!["0.0".into(), "8.0".into(), "15.0".into()]);

    // Create the Y axis and define its properties
    let min = data_points
        .iter()
        .map(|x| OrderedFloat(x.1))
        .min()
        .map(|x| x.0)
        .unwrap_or_default();
    let max = data_points
        .iter()
        .map(|x| OrderedFloat(x.1))
        .max()
        .map(|x| x.0)
        .unwrap_or(10.);

    let mid = (min + max) / 2.;

    let y_axis = Axis::default()
        .title("Y Axis".red())
        .bounds([min, max])
        .labels(vec![
            min.to_string().into(),
            mid.to_string().into(),
            max.to_string().into(),
        ]);

    let mut title = format!("{}_{:?}{{", key.name, key.metric_type.to_string());
    for attr in &key.attributes {
        title.push_str(&format!("{}={}", attr.0, attr.1));
        title.push(' ');
    }
    title.push('}');

    let chart = Chart::new(vec![data])
        .block(Block::new().title(title))
        .x_axis(x_axis)
        .y_axis(y_axis);

    f.render_widget(chart, area)
}

fn render_histogram(
    key: &MetricSetKey,
    value: impl Iterator<Item = &'a MetricValue>,
    f: &mut Frame,
    area: Rect,
) {
    let value = value.last().unwrap();
    let MetricValue::Histogram(((p50, p90, p95, p99), sum)) = value else {
        unreachable!()
    };
    let data = [("p50", p50), ("p90", p90), ("p95", p95), ("p99", p99)];
    let barchart = BarChart::default()
        .block(Block::bordered().title("BarChart"))
        .bar_width(1)
        .bar_style(Style::new().yellow().on_red())
        .value_style(Style::new().red().bold())
        .label_style(Style::new().white())
        .data(&[data])
        .data(BarGroup::default().bars(&[Bar::default().value(10), Bar::default().value(20)]))
        .max(4);
}

fn render_metrics<'a>(
    metrics: impl std::iter::ExactSizeIterator<Item = (&'a MetricSetKey, &'a VecDeque<MetricValue>)>,
    rect: Rect,
    f: &mut Frame,
) {
    let layout = Layout::vertical((0..metrics.len()).map(|_| Constraint::Length(10))).split(rect);
    for (metric, rect) in metrics.zip(layout.iter()) {
        if metric.0.metric_type == MetricType::Gauge {
            render_gauge(metric.0, metric.1.iter(), f, *rect)
        }
    }
}

pub fn ui(f: &mut Frame, app: &App) {
    let area = f.size();

    let (logo_size, logo_render) = logo();
    let (scenario_size, scenario_render) = scenario_text(&app.current_scenario().name);
    let (executor_size, executor_render) =
        executor_text(app.current_exec, app.current_scenario().exec_names());
    let (progress_size, progress_render) = progress_bar(app.current_exec());
    let (info_size, info_render) = other_info(app.current_exec());

    let left_width = logo_size
        .width
        .max(scenario_size.width)
        .max(executor_size.width)
        .max(progress_size.width)
        .max(info_size.width)
        + 4;

    // No margins here. Margins are applied by children of the main area
    let [left_area, metric_area] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Min(0)]).areas(area);

    // Draw borders
    f.render_widget(Block::bordered().borders(Borders::RIGHT), left_area);

    let left_height = 1
        + logo_size.height
        + 1
        + scenario_size.height
        + executor_size.height
        + 1
        + progress_size.height
        + 1
        + info_size.height
        + 1;

    if left_height > left_area.height {
        // cant render the whole thing
        f.render_widget(
            Text::raw("Too Small").red().bold().centered(),
            Layout::vertical([Constraint::Length(1)])
                .flex(Flex::Center)
                .split(left_area)[0],
        )
    } else {
        // Left Area
        let [logo_area, scenario_area, executors_area, _, progress_area, _, info_area] =
            Layout::vertical([
                Constraint::Length(logo_size.height + 1),
                Constraint::Length(scenario_size.height),
                Constraint::Length(executor_size.height),
                Constraint::Length(1),
                Constraint::Length(progress_size.height),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .vertical_margin(1)
            .horizontal_margin(2)
            .areas(left_area);

        f.render_widget(Block::bordered().borders(Borders::BOTTOM), logo_area);

        logo_render(f, logo_area);
        scenario_render(f, scenario_area);
        progress_render(f, progress_area);
        executor_render(f, executors_area);
        info_render(f, info_area);
        render_metrics(app.current_exec().metrics.iter(), metric_area, f)
    }
}

fn padding(n: usize) -> String {
    String::from_iter(std::iter::repeat(' ').take(n))
}

fn key_value_span(key: &'static str, value: String) -> [Span<'static>; 4] {
    let size = 1 + key.len() + value.len();
    [
        Span::raw(key).green(),
        Span::raw("=").green(),
        Span::raw(value),
        Span::raw(padding(INFO_CELL_SIZE.saturating_sub(size).max(1))),
    ]
}

fn value_span(value: String) -> [Span<'static>; 2] {
    let size = value.len();
    [
        Span::raw(value).light_blue(),
        Span::raw(padding(INFO_CELL_SIZE.saturating_sub(size).max(1))),
    ]
}