use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::bulk::{BulkKind, BulkModal, BulkRunState, BulkTarget, ExportFormat};
use crate::exec::{despawn_command_display, reset_command_display};

use super::main_screen::centered_rect;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    if let Some(operation) = app.bulk_operation.as_ref() {
        render_operation(frame, operation);
    } else if let Some(modal) = app.bulk_modal.as_ref() {
        render_modal(frame, app, modal);
    }
}

fn render_modal(frame: &mut Frame<'_>, app: &App, modal: &BulkModal) {
    let area = centered_rect(94, 84, frame.area());
    frame.render_widget(Clear, area);
    let content_height = area.height.saturating_sub(5) as usize;
    let (title, mut lines) = match modal {
        BulkModal::Preview {
            kind,
            targets,
            confirm,
            scroll,
        } => {
            let mut lines = vec![
                warning_line(*kind),
                Line::from(format!("targets: {}", targets.len())),
            ];
            let end = (*scroll + content_height).min(targets.len());
            for (index, target) in targets.iter().enumerate().take(end).skip(*scroll) {
                lines.push(Line::from(format!(
                    "{:>3}. {}",
                    index + 1,
                    target_command(app, target, false)
                )));
            }
            if end < targets.len() {
                lines.push(Line::from(format!(
                    "… {} more (j/k scroll)",
                    targets.len() - end
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Type YES: {confirm}█")));
            lines.push(Line::from("Enter: execute sequentially  Esc: cancel"));
            (format!(" {} preview ", kind.title()), lines)
        }
        BulkModal::ExportFormat { selected } => {
            let mark = |format| if *selected == format { ">" } else { " " };
            (
                " export format ".to_owned(),
                vec![
                    Line::from(format!(
                        "Export every filtered result to {}:",
                        app.paths.report_dir.display()
                    )),
                    Line::from(format!("{} Markdown (.md)", mark(ExportFormat::Markdown))),
                    Line::from(format!("{} JSON (.json)", mark(ExportFormat::Json))),
                    Line::from("←/→ or j/k: select  Enter: export  Esc: cancel"),
                ],
            )
        }
        BulkModal::RenameEdit {
            targets, selected, ..
        } => {
            let start = selected.saturating_sub(content_height.saturating_sub(2) / 2);
            let end = (start + content_height).min(targets.len());
            let mut lines = vec![Line::from(
                "Edit proposals. Typing replaces the selected default; Enter previews all.",
            )];
            for (index, target) in targets.iter().enumerate().take(end).skip(start) {
                lines.push(Line::from(format!(
                    "{} {:>3}. {}/{} -> {}{}",
                    if index == *selected { ">" } else { " " },
                    index + 1,
                    target.team,
                    target.old,
                    target.new,
                    if index == *selected { "█" } else { "" }
                )));
            }
            lines.push(Line::from(
                "↑/↓/Tab: target  type: edit  Enter: preview  Esc:cancel",
            ));
            (" bulk rename proposals ".to_owned(), lines)
        }
        BulkModal::RenameConfirm {
            targets,
            confirm,
            scroll,
        } => {
            let end = (*scroll + content_height).min(targets.len());
            let mut lines = vec![Line::from(Span::styled(
                "⚠ rename.sh updates config and message history",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))];
            for (index, target) in targets.iter().enumerate().take(end).skip(*scroll) {
                lines.push(Line::from(format!(
                    "{:>3}. {}/{} -> {}",
                    index + 1,
                    target.team,
                    target.old,
                    target.new
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Type YES: {confirm}█")));
            lines.push(Line::from("Enter: execute sequentially  Esc: cancel"));
            (" bulk rename preview ".to_owned(), lines)
        }
    };
    if lines.is_empty() {
        lines.push(Line::from("(empty)"));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_operation(frame: &mut Frame<'_>, operation: &crate::bulk::BulkOperation) {
    let area = centered_rect(94, 84, frame.area());
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from(Span::styled(
        operation.progress_label(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    let visible = area.height.saturating_sub(8) as usize;
    let start = operation
        .results_cursor
        .saturating_add(1)
        .saturating_sub(visible);
    let end = (start + visible).min(operation.results.len());
    for result in operation.results.iter().take(end).skip(start) {
        lines.push(Line::from(vec![
            Span::styled(
                if result.success { "✓ " } else { "✗ " },
                Style::default().fg(if result.success {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
            Span::raw(format!(
                "{} — {}",
                result.target.label(),
                one_line(&result.detail, 72)
            )),
        ]));
    }
    match &operation.state {
        BulkRunState::Running => {
            if let Some(target) = operation.current_target() {
                lines.push(Line::from(format!(
                    "running: {}{}",
                    target.label(),
                    if operation.force_despawn {
                        " --force"
                    } else {
                        ""
                    }
                )));
            }
            lines.push(Line::from("Esc/q: cancel current command and abort batch"));
        }
        BulkRunState::AwaitDecision { detail } => {
            lines.push(Line::from(Span::styled(
                format!("failed: {}", one_line(detail, 76)),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(
                "c: continue with next target  a/Esc: abort batch",
            ));
        }
        BulkRunState::AwaitForce { detail } => {
            lines.push(Line::from(Span::styled(
                format!("graceful failed: {}", one_line(detail, 70)),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(Span::styled(
                "Force kills the recorded tmux pane/window and runs reset.sh.",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from("y/f: force despawn  n/Esc: abort"));
        }
        BulkRunState::Complete { aborted } => {
            lines.push(Line::from(if *aborted {
                "batch aborted; completed results are shown above"
            } else {
                "batch complete; results are shown above"
            }));
            lines.push(Line::from("j/k g/G: scroll results  Enter/Esc: close"));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" {} progress ", operation.kind.title()))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn warning_line(kind: BulkKind) -> Line<'static> {
    let text = match kind {
        BulkKind::MarkRead => "Preview only: inbox.sh marks each listed recipient's unread inbox.",
        BulkKind::Reset => "⚠ destructive: reset.sh removes every listed registration.",
        BulkKind::Rename => "⚠ rename.sh changes identity names and message history.",
        BulkKind::Despawn => "⚠ destructive: graceful despawn waits up to 30 seconds.",
    };
    Line::from(Span::styled(
        text,
        Style::default().fg(if kind == BulkKind::MarkRead {
            Color::Cyan
        } else {
            Color::Yellow
        }),
    ))
}

fn target_command(app: &App, target: &BulkTarget, force: bool) -> String {
    match target {
        BulkTarget::MarkRead(target) => format!(
            "{} {} {} --quiet  # {} filtered msgs",
            app.paths.scripts_dir.join("inbox.sh").display(),
            target.team,
            target.recipient,
            target.message_count
        ),
        BulkTarget::Reset(target) => format!(
            "{}/{}: {}",
            target.team,
            target.agent,
            reset_command_display(
                &app.paths.scripts_dir,
                &target.project,
                &target.agent_type,
                &target.agent,
            )
        ),
        BulkTarget::Rename(target) => format!(
            "{} {} {} {}",
            app.paths.scripts_dir.join("rename.sh").display(),
            target.team,
            target.old,
            target.new
        ),
        BulkTarget::Despawn(target) => despawn_command_display(
            &app.paths.scripts_dir,
            &target.team,
            &target.from,
            &target.name,
            force,
        ),
    }
}

fn one_line(value: &str, max: usize) -> String {
    let flat = value.replace(['\n', '\r', '\t'], " ");
    let mut chars = flat.chars();
    let output = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{output}…")
    } else {
        output
    }
}
