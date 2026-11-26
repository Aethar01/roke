use crate::app::{App, FormField, ModalRegions, Mode, TagModalRegions, TaskForm, TodoFilter, View};
use crate::model::{today_ymd, DueState, Priority, SortOrder, Status, Task};
use crate::theme::Theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn render(frame: &mut Frame, app: &mut App) {
    app.regions.clear();
    let area = frame.area();

    let content_area = if area.width >= 20 && area.height >= 8 {
        Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        )
    } else {
        area
    };

    let show_search = matches!(app.mode, Mode::Search) || !app.search_query.is_empty();
    let constraints = if show_search {
        vec![
            Constraint::Length(2), // Minimal header + hairline
            Constraint::Length(1), // Search line
            Constraint::Min(4),    // Body
            Constraint::Length(1), // Single-line footer
        ]
    } else {
        vec![
            Constraint::Length(2), // Minimal header + hairline
            Constraint::Min(4),    // Body
            Constraint::Length(1), // Single-line footer
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(content_area);

    render_header(frame, app, chunks[0]);

    let (body_area, footer_area) = if show_search {
        render_search_bar(frame, app, chunks[1]);
        (chunks[2], chunks[3])
    } else {
        (chunks[1], chunks[2])
    };

    match app.view {
        View::TodoList => render_todo_view(frame, app, body_area),
        View::Kanban => render_kanban_view(frame, app, body_area),
    }

    render_footer(frame, app, footer_area);

    if let Mode::TaskModal(form) = &app.mode {
        let form_clone = form.clone();
        render_task_modal(frame, app, &form_clone);
    } else if let Mode::TagFilter(sel) = app.mode {
        render_tag_filter_modal(frame, app, sel);
    } else if let Mode::NotesPreview { task_id, scroll } = app.mode {
        render_notes_preview_modal(frame, app, task_id, scroll);
    } else if matches!(app.mode, Mode::Help) {
        render_help_modal(frame, app);
    }
}

fn render_header(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let theme = app.theme.clone();
    let y = area.y;
    let mut x = area.x;

    let tab1_label = "Todos";
    let tab2_label = "Kanban";
    let add_label = "+ new";
    let help_label = "?";

    let active_tab_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let inactive_tab_style = Style::default().fg(theme.subtle);

    let tab1_w = (tab1_label.len() as u16).min(area.right().saturating_sub(x));
    if tab1_w > 0 {
        let rect = Rect::new(x, y, tab1_w, 1);
        app.regions.tab_todos = rect;
        frame.render_widget(
            Paragraph::new(Span::styled(
                tab1_label,
                if app.view == View::TodoList {
                    active_tab_style
                } else {
                    inactive_tab_style
                },
            )),
            rect,
        );
        x = x.saturating_add(tab1_w + 3);
    }

    if x < area.right() {
        let tab2_w = (tab2_label.len() as u16).min(area.right().saturating_sub(x));
        let rect = Rect::new(x, y, tab2_w, 1);
        app.regions.tab_kanban = rect;
        frame.render_widget(
            Paragraph::new(Span::styled(
                tab2_label,
                if app.view == View::Kanban {
                    active_tab_style
                } else {
                    inactive_tab_style
                },
            )),
            rect,
        );
        x = x.saturating_add(tab2_w + 2);
    }

    let help_w = help_label.len() as u16;
    let add_w = add_label.len() as u16;
    let open_count = app
        .store
        .tasks
        .iter()
        .filter(|t| t.status != Status::Done)
        .count();
    let total_count = app.store.tasks.len();
    let count_label = format!("{open_count} open / {total_count}");
    let count_w = count_label.len() as u16;

    let mut right_edge = area.right();
    if right_edge >= x + help_w {
        right_edge -= help_w;
        let rect = Rect::new(right_edge, y, help_w, 1);
        app.regions.help_btn = rect;
        frame.render_widget(
            Paragraph::new(Span::styled(help_label, Style::default().fg(theme.subtle))),
            rect,
        );
    }

    if right_edge > x + add_w + 1 {
        right_edge -= add_w + 2;
        let rect = Rect::new(right_edge, y, add_w, 1);
        app.regions.add_btn = rect;
        frame.render_widget(
            Paragraph::new(Span::styled(add_label, Style::default().fg(theme.add))),
            rect,
        );
    }

    if !app.sync_config.peers.is_empty() {
        let sync_label = if app.syncing { "↻ syncing" } else { "↻ sync" };
        let sync_w = sync_label.width() as u16;
        if right_edge > x + sync_w + 1 {
            right_edge -= sync_w + 2;
            let rect = Rect::new(right_edge, y, sync_w, 1);
            app.regions.sync_btn = rect;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    sync_label,
                    Style::default().fg(if app.syncing {
                        theme.accent
                    } else {
                        theme.subtle
                    }),
                )),
                rect,
            );
        }
    }

    if app.sort_order != SortOrder::Manual {
        let sort_label = format!("↕ {}", app.sort_order.label());
        let sort_w = sort_label.width() as u16;
        if right_edge > x + sort_w + 1 {
            right_edge -= sort_w + 2;
            let rect = Rect::new(right_edge, y, sort_w, 1);
            app.regions.sort_btn = rect;
            frame.render_widget(
                Paragraph::new(Span::styled(sort_label, Style::default().fg(theme.accent))),
                rect,
            );
        }
    }

    if right_edge > x + count_w + 1 {
        right_edge -= count_w + 2;
        let rect = Rect::new(right_edge, y, count_w, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(count_label, Style::default().fg(theme.muted))),
            rect,
        );
    }

    if area.height >= 2 {
        let rule = "─".repeat(area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(rule, Style::default().fg(theme.muted))),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = &app.theme;
    let active = matches!(app.mode, Mode::Search);
    let text = app.active_search_text();
    let prefix_style = if active {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };

    let avail_w = area.width.saturating_sub(2) as usize;
    let (visible, cursor_off) = if active {
        visible_text_slice(text, app.search_input.cursor, avail_w)
    } else {
        (text, 0)
    };

    let line = Line::from(vec![
        Span::styled("/ ", prefix_style),
        Span::styled(
            visible,
            Style::default().fg(if active { theme.text } else { theme.subtle }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if active {
        let cursor_x = area.x + 2 + (cursor_off as u16);
        if cursor_x < area.right() {
            frame.set_cursor_position(Position::new(cursor_x, area.y));
        }
    }
}

fn status_filter_row_width(app: &App) -> u16 {
    let mut w: u16 = 0;
    for (idx, filter) in TodoFilter::ALL.iter().enumerate() {
        let count = app
            .store
            .tasks
            .iter()
            .filter(|t| filter.matches(t.status) && app.matches_tag(t) && app.matches_search(t))
            .count();
        let label_w = (filter.label().len() + 1 + count.to_string().len()) as u16;
        if idx > 0 {
            w = w.saturating_add(3);
        }
        w = w.saturating_add(label_w);
    }
    w
}

fn tag_pills_total_width(app: &App, all_tags: &[String], show_all_pill: bool) -> u16 {
    let mut w: u16 = 0;
    let mut first = true;
    if show_all_pill {
        w = w.saturating_add(3); // "all"
        first = false;
    }
    for tag in all_tags {
        if !first {
            w = w.saturating_add(2);
        }
        w = w.saturating_add((tag.width() + 1) as u16); // "#tag"
        first = false;
    }
    if app.tag_filters.len() >= 2 {
        if !first {
            w = w.saturating_add(2);
        }
        w = w.saturating_add((app.tag_match_mode.label().len() + 2) as u16); // "[and]" / "[or]"
    }
    w
}

fn tag_pills_rows_needed(
    app: &App,
    all_tags: &[String],
    width: u16,
    show_all_pill: bool,
    max_rows: u16,
) -> u16 {
    if all_tags.is_empty() || width == 0 {
        return 0;
    }
    let mut rows: u16 = 1;
    let mut col: u16 = 0;

    let mut place = |item_w: u16| {
        if col > 0 && col.saturating_add(2).saturating_add(item_w) > width {
            rows = rows.saturating_add(1);
            col = item_w;
        } else if col > 0 {
            col = col.saturating_add(2).saturating_add(item_w);
        } else {
            col = item_w;
        }
    };

    if show_all_pill {
        place(3);
    }
    for tag in all_tags {
        place((tag.width() + 1) as u16);
    }
    if app.tag_filters.len() >= 2 {
        place((app.tag_match_mode.label().len() + 2) as u16);
    }
    rows.min(max_rows)
}

fn render_todo_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let all_tags = app.all_tags();
    let status_w = status_filter_row_width(app);
    let inline_tags_w = tag_pills_total_width(app, &all_tags, false);
    // Separator "   │  " takes 6 columns
    let tags_fit_inline =
        !all_tags.is_empty() && status_w.saturating_add(6).saturating_add(inline_tags_w) <= area.width;

    let max_tag_rows = if area.height >= 10 { 2 } else { 1 };
    let extra_tag_rows = if all_tags.is_empty() || tags_fit_inline {
        0
    } else {
        tag_pills_rows_needed(app, &all_tags, area.width, false, max_tag_rows)
    };

    let filter_h = 1 + extra_tag_rows;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(filter_h), Constraint::Min(2)])
        .split(area);

    render_filter_bar(frame, app, chunks[0], &all_tags, tags_fit_inline);
    render_todo_list(frame, app, chunks[1]);
}

fn render_filter_bar(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    all_tags: &[String],
    tags_fit_inline: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = app.theme.clone();
    let mut x = area.x;
    let y = area.y;

    for filter in TodoFilter::ALL {
        let count = app
            .store
            .tasks
            .iter()
            .filter(|t| filter.matches(t.status) && app.matches_tag(t) && app.matches_search(t))
            .count();
        let label = format!("{} {}", filter.label(), count);
        let w = label.len() as u16;
        if x + w > area.right() {
            break;
        }
        let rect = Rect::new(x, y, w, 1);
        app.regions.filter_pills.push((filter, rect));

        let is_active = app.todo_filter == filter;
        let line = Line::from(vec![
            Span::styled(
                filter.label(),
                if is_active {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(theme.subtle)
                },
            ),
            Span::styled(
                format!(" {count}"),
                Style::default().fg(if is_active { theme.subtle } else { theme.muted }),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), rect);
        x = x.saturating_add(w + 3);
    }

    if all_tags.is_empty() {
        return;
    }

    if tags_fit_inline && x + 3 < area.right() {
        frame.render_widget(
            Paragraph::new(Span::styled("│", Style::default().fg(theme.muted))),
            Rect::new(x, y, 1, 1),
        );
        let tag_area = Rect::new(x + 3, y, area.right().saturating_sub(x + 3), 1);
        render_tag_pills_wrapped(frame, app, all_tags, tag_area, false);
    } else if area.height >= 2 {
        let tag_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        render_tag_pills_wrapped(frame, app, all_tags, tag_area, false);
    }
}

/// Renders tag pills across one or more rows inside `area`.
/// If tags exceed `area`, appends a clickable `+N` button that opens the Tag Filter modal.
fn render_tag_pills_wrapped(
    frame: &mut Frame,
    app: &mut App,
    all_tags: &[String],
    area: Rect,
    show_all_pill: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = app.theme.clone();
    let mut x = area.x;
    let mut y = area.y;
    let last_y = area.bottom().saturating_sub(1);

    let show_mode = app.tag_filters.len() >= 2;
    let mode_label = format!("[{}]", app.tag_match_mode.label());
    let mode_w = if show_mode {
        mode_label.len() as u16
    } else {
        0
    };

    if show_all_pill {
        let label = "all";
        let w = label.len() as u16;
        if x + w <= area.right() {
            let rect = Rect::new(x, y, w, 1);
            app.regions.tag_pills.push((None, rect));
            let is_active = app.tag_filters.is_empty();
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    if is_active {
                        Style::default()
                            .fg(theme.tag_active)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        Style::default().fg(theme.muted)
                    },
                )),
                rect,
            );
            x = x.saturating_add(w + 2);
        }
    }

    let mut hidden_count: usize = 0;

    for (idx, tag) in all_tags.iter().enumerate() {
        let label = format!("#{tag}");
        let w = label.width() as u16;
        let remaining_after = all_tags.len().saturating_sub(idx + 1);

        // On the last row, reserve space for `+N` if more tags follow, plus `[and]` if active
        let mut reserve_right: u16 = 0;
        if y == last_y {
            if remaining_after > 0 {
                let overflow_preview = format!("+{remaining_after}");
                reserve_right = reserve_right.saturating_add(overflow_preview.len() as u16 + 2);
            }
            if show_mode {
                reserve_right = reserve_right.saturating_add(mode_w + 2);
            }
        }

        if x > area.x && x.saturating_add(w) > area.right().saturating_sub(reserve_right) {
            if y < last_y {
                y += 1;
                x = area.x;
            } else {
                hidden_count = all_tags.len() - idx;
                break;
            }
        }

        if x.saturating_add(w) > area.right().saturating_sub(reserve_right) && x == area.x && y == last_y {
            // Even at start of last row it doesn't fit with reserve; show overflow if tags remain
            if remaining_after > 0 || w > area.width {
                hidden_count = all_tags.len() - idx;
                break;
            }
        }

        let draw_w = w.min(area.right().saturating_sub(x));
        if draw_w == 0 {
            hidden_count = all_tags.len() - idx;
            break;
        }
        let rect = Rect::new(x, y, draw_w, 1);
        app.regions.tag_pills.push((Some(tag.clone()), rect));
        let is_active = app.is_tag_active(tag);
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                if is_active {
                    Style::default()
                        .fg(theme.tag_active)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(theme.muted)
                },
            )),
            rect,
        );
        x = x.saturating_add(draw_w + 2);
    }

    if hidden_count > 0 {
        let label = format!("+{hidden_count}");
        let w = label.len() as u16;
        if x + w > area.right() && y < last_y {
            y += 1;
            x = area.x;
        }
        if x + w <= area.right() {
            let rect = Rect::new(x, y, w, 1);
            app.regions.tag_overflow_btn = rect;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                rect,
            );
            x = x.saturating_add(w + 2);
        }
    }

    if show_mode {
        if x + mode_w > area.right() && y < last_y {
            y += 1;
            x = area.x;
        }
        if x + mode_w <= area.right() {
            let rect = Rect::new(x, y, mode_w, 1);
            app.regions.tag_mode_btn = rect;
            frame.render_widget(
                Paragraph::new(Span::styled(mode_label, Style::default().fg(theme.subtle))),
                rect,
            );
        }
    }
}

fn markdown_preview(desc: &str) -> (String, usize) {
    let mut non_empty = desc.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = non_empty.next().unwrap_or("");
    let cleaned = first.trim_start_matches('#').trim();
    let preview = if cleaned.is_empty() { first } else { cleaned };
    let total_lines = desc.lines().count();
    let extra = total_lines.saturating_sub(1);
    (preview.to_string(), extra)
}

fn todo_card_height(task: &Task) -> u16 {
    if task.description.is_empty() {
        3
    } else {
        4
    }
}

fn kanban_card_height(task: &Task, card_width: u16) -> u16 {
    let inner_w = card_width.saturating_sub(4).max(1) as usize;
    let title_lines = task.title.width().div_ceil(inner_w).clamp(1, 2) as u16;
    let desc_lines = if task.description.is_empty() { 0 } else { 1 };
    2 + title_lines + desc_lines
}

fn adjust_scroll_with_gap(
    heights: &[u16],
    gap: u16,
    selected: usize,
    scroll: &mut usize,
    viewport_height: u16,
) {
    if heights.is_empty() {
        *scroll = 0;
        return;
    }
    if selected < *scroll {
        *scroll = selected;
        return;
    }
    let span_height = |start: usize, end: usize| -> u16 {
        let cards: u16 = heights[start..=end].iter().copied().sum();
        let gaps = (end.saturating_sub(start) as u16).saturating_mul(gap);
        cards.saturating_add(gaps)
    };
    while span_height(*scroll, selected) > viewport_height && *scroll < selected {
        *scroll += 1;
    }
}

fn render_todo_list(frame: &mut Frame, app: &mut App, area: Rect) {
    app.regions.todo_list_area = area;
    if area.height == 0 || area.width < 4 {
        return;
    }

    let theme = app.theme.clone();
    let ids = app.filtered_todo_ids();
    if ids.is_empty() {
        let msg = if app.active_search_text().trim().is_empty() && app.tag_filters.is_empty() {
            "No tasks yet. Press 'a' to add one."
        } else {
            "No matching tasks. Press Esc to clear filters."
        };
        frame.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(theme.muted)))
                .wrap(Wrap { trim: true }),
            Rect::new(area.x, area.y, area.width, area.height),
        );
        return;
    }

    let card_gap: u16 = 0;
    let heights: Vec<u16> = ids
        .iter()
        .map(|&id| app.store.get(id).map(todo_card_height).unwrap_or(3))
        .collect();
    adjust_scroll_with_gap(
        &heights,
        card_gap,
        app.todo_selected,
        &mut app.todo_scroll,
        area.height,
    );

    let show_status_tag = app.todo_filter == TodoFilter::All;
    let mut y = area.y;
    for (idx, &task_id) in ids.iter().enumerate().skip(app.todo_scroll) {
        if y >= area.bottom() {
            break;
        }
        let Some(task) = app.store.get(task_id).cloned() else {
            continue;
        };
        let desired_h = heights[idx];
        let remaining = area.bottom().saturating_sub(y);
        if remaining < 3 && y > area.y {
            break;
        }
        let h = desired_h.min(remaining);
        if h < 2 {
            break;
        }
        let card_rect = Rect::new(area.x, y, area.width, h);
        let checkbox_rect = Rect::new(
            card_rect.x + 1,
            card_rect.y + 1,
            3.min(card_rect.width.saturating_sub(2)),
            1,
        );
        app.regions
            .todo_rows
            .push((task_id, card_rect, checkbox_rect));

        let is_selected = idx == app.todo_selected;
        let eff_tags = app.effective_tags(&task);
        render_todo_card(
            frame,
            &theme,
            &task,
            &eff_tags,
            card_rect,
            is_selected,
            show_status_tag,
            &app.tag_filters,
        );
        y = y.saturating_add(h + card_gap);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_todo_card(
    frame: &mut Frame,
    theme: &Theme,
    task: &Task,
    tags: &[String],
    rect: Rect,
    selected: bool,
    show_status_tag: bool,
    active_tags: &[String],
) {
    let border_color = if selected { theme.accent } else { theme.muted };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    if task.status != Status::Done {
        block = block.title(Line::from(Span::styled(
            format!(" {} ", task.priority.label()),
            priority_tag_style(theme, task.priority, selected),
        )));
    }

    if show_status_tag && rect.width >= 24 {
        block = block.title(
            Line::from(Span::styled(
                format!(" {} ", task.status.label()),
                status_tag_style(theme, task.status, selected),
            ))
            .alignment(Alignment::Right),
        );
    }

    if !tags.is_empty() && rect.height >= 3 {
        let reserve_due = task.due.as_ref().map(|d| d.len() + 3).unwrap_or(0);
        let max_w = (rect.width.saturating_sub(4) as usize).saturating_sub(reserve_due);
        if let Some(line) = tags_border_line(theme, tags, active_tags, max_w) {
            block = block.title_bottom(line);
        }
    }

    if let Some(due_line) = due_border_line(theme, task) {
        if rect.height >= 3 && rect.width >= 18 {
            block = block.title_bottom(due_line.alignment(Alignment::Right));
        }
    }

    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let glyph = Span::styled(
        format!("{} ", task.status.glyph()),
        status_glyph_style(theme, task.status),
    );

    let title_style = if task.status == Status::Done {
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::CROSSED_OUT)
    } else if selected {
        Style::default()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.subtle)
    };

    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        glyph,
        Span::styled(task.title.clone(), title_style),
    ])];

    if inner.height >= 2 && !task.description.is_empty() {
        let (preview, extra_lines) = markdown_preview(&task.description);
        let mut spans = vec![
            Span::raw("   "),
            Span::styled(preview, Style::default().fg(theme.muted)),
        ];
        if let Some((done, total)) = task.checklist_progress() {
            let color = if done == total {
                theme.done
            } else {
                theme.accent
            };
            spans.push(Span::styled(
                format!("  [{done}/{total}]"),
                Style::default().fg(color),
            ));
        }
        if extra_lines > 0 {
            spans.push(Span::styled(
                format!("  [md +{extra_lines}]"),
                Style::default().fg(theme.subtle),
            ));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_kanban_view(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.height < 2 || area.width < 9 {
        return;
    }

    let theme = app.theme.clone();
    let all_tags = app.all_tags();
    let board_area = if !all_tags.is_empty() && area.height >= 5 {
        let max_tag_rows = if area.height >= 12 { 2 } else { 1 };
        let tag_rows = tag_pills_rows_needed(app, &all_tags, area.width, true, max_tag_rows);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(tag_rows), Constraint::Min(3)])
            .split(area);
        render_tag_pills_wrapped(frame, app, &all_tags, split[0], true);
        split[1]
    } else {
        area
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(board_area);

    for status in Status::ALL {
        let col_idx = status.index();
        let col_rect = cols[col_idx];
        app.regions.kanban_columns.push((status, col_rect));

        let inner = if col_idx > 0 && col_rect.width > 3 {
            let divider_block = Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(theme.muted));
            let b_inner = divider_block.inner(col_rect);
            frame.render_widget(divider_block, col_rect);
            Rect::new(
                b_inner.x + 1,
                b_inner.y,
                b_inner.width.saturating_sub(2),
                b_inner.height,
            )
        } else {
            Rect::new(
                col_rect.x,
                col_rect.y,
                col_rect.width.saturating_sub(1),
                col_rect.height,
            )
        };

        if inner.width < 4 || inner.height < 2 {
            continue;
        }

        let is_focused = app.kanban_col == col_idx;
        let is_drag_target = app
            .drag
            .as_ref()
            .is_some_and(|d| d.active && d.hover_status == Some(status) && d.origin_status != status);
        let is_same_col_reorder = app.drag.as_ref().is_some_and(|d| {
            d.active
                && d.hover_status == Some(status)
                && d.origin_status == status
                && app.sort_order == SortOrder::Manual
        });

        let ids = app.column_task_ids(status);
        let col_accent = status_color(&theme, status);

        let header_color = if is_drag_target {
            theme.drop
        } else if is_focused {
            theme.text
        } else {
            theme.subtle
        };

        let mut header_spans = vec![
            Span::styled(
                format!("{} ", status.glyph()),
                Style::default().fg(if is_drag_target {
                    theme.drop
                } else {
                    col_accent
                }),
            ),
            Span::styled(
                status.label(),
                Style::default()
                    .fg(header_color)
                    .add_modifier(if is_focused || is_drag_target {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(format!("  {}", ids.len()), Style::default().fg(theme.muted)),
        ];
        if is_drag_target {
            header_spans.push(Span::styled("  ↓ drop", Style::default().fg(theme.drop)));
        } else if is_same_col_reorder {
            header_spans.push(Span::styled("  ↕ reorder", Style::default().fg(theme.drag)));
        }

        frame.render_widget(
            Paragraph::new(Line::from(header_spans)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let rule_color = if is_drag_target {
            theme.drop
        } else if is_focused {
            col_accent
        } else {
            theme.muted
        };
        let rule = "─".repeat(inner.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(rule, Style::default().fg(rule_color))),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );

        let list_area = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(2),
        );
        if list_area.height == 0 {
            continue;
        }

        if ids.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("—", Style::default().fg(theme.muted))),
                Rect::new(list_area.x, list_area.y, list_area.width, 1),
            );
            continue;
        }

        let card_gap: u16 = 0;
        let heights: Vec<u16> = ids
            .iter()
            .map(|&id| {
                app.store
                    .get(id)
                    .map(|t| kanban_card_height(t, list_area.width))
                    .unwrap_or(3)
            })
            .collect();
        let selected = app.kanban_selected[col_idx];
        adjust_scroll_with_gap(
            &heights,
            card_gap,
            selected,
            &mut app.kanban_scroll[col_idx],
            list_area.height,
        );
        let scroll = app.kanban_scroll[col_idx];

        let mut y = list_area.y;
        for (idx, &task_id) in ids.iter().enumerate().skip(scroll) {
            if y >= list_area.bottom() {
                break;
            }
            let Some(task) = app.store.get(task_id).cloned() else {
                continue;
            };
            let desired_h = heights[idx];
            let remaining = list_area.bottom().saturating_sub(y);
            if remaining < 3 && y > list_area.y {
                break;
            }
            let h = desired_h.min(remaining);
            if h < 2 {
                break;
            }
            let card_rect = Rect::new(list_area.x, y, list_area.width, h);
            app.regions.kanban_cards.push((task_id, status, card_rect));

            let is_selected = is_focused && idx == selected;
            let is_being_dragged = app
                .drag
                .as_ref()
                .is_some_and(|d| d.active && d.task_id == task_id);

            let eff_tags = app.effective_tags(&task);
            render_kanban_card(
                frame,
                &theme,
                &task,
                &eff_tags,
                card_rect,
                is_selected,
                is_being_dragged,
                &app.tag_filters,
            );
            y = y.saturating_add(h + card_gap);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_kanban_card(
    frame: &mut Frame,
    theme: &Theme,
    task: &Task,
    tags: &[String],
    rect: Rect,
    selected: bool,
    dragging: bool,
    active_tags: &[String],
) {
    let border_color = if dragging {
        theme.drag
    } else if selected {
        theme.accent
    } else {
        theme.muted
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    if task.status != Status::Done {
        block = block.title(Line::from(Span::styled(
            format!(" {} ", task.priority.label()),
            priority_tag_style(theme, task.priority, selected),
        )));
    }

    if !tags.is_empty() && rect.height >= 3 {
        let reserve_due = task.due.as_ref().map(|d| d.len() + 3).unwrap_or(0);
        let max_w = (rect.width.saturating_sub(4) as usize).saturating_sub(reserve_due);
        if let Some(line) = tags_border_line(theme, tags, active_tags, max_w) {
            block = block.title_bottom(line);
        }
    }

    if let Some(due_line) = due_border_line(theme, task) {
        if rect.height >= 3 && rect.width >= 18 {
            block = block.title_bottom(due_line.alignment(Alignment::Right));
        }
    }

    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.width < 2 || inner.height == 0 {
        return;
    }

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    if padded.width == 0 {
        return;
    }

    let title_style = if dragging {
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC)
    } else if task.status == Status::Done {
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::CROSSED_OUT)
    } else if selected {
        Style::default()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.subtle)
    };

    let has_desc = !task.description.is_empty() && padded.height >= 2;
    let title_h = if has_desc {
        padded.height.saturating_sub(1)
    } else {
        padded.height
    };

    frame.render_widget(
        Paragraph::new(Span::styled(task.title.clone(), title_style)).wrap(Wrap { trim: true }),
        Rect::new(padded.x, padded.y, padded.width, title_h),
    );

    if has_desc {
        let (preview, extra_lines) = markdown_preview(&task.description);
        let mut spans = vec![Span::styled(preview, Style::default().fg(theme.muted))];
        if let Some((done, total)) = task.checklist_progress() {
            let color = if done == total {
                theme.done
            } else {
                theme.accent
            };
            spans.push(Span::styled(
                format!(" [{done}/{total}]"),
                Style::default().fg(color),
            ));
        }
        if extra_lines > 0 {
            spans.push(Span::styled(
                format!(" [md +{extra_lines}]"),
                Style::default().fg(theme.subtle),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(padded.x, padded.y + title_h, padded.width, 1),
        );
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = &app.theme;

    let left_text = if let Some(drag) = &app.drag {
        if drag.active {
            if drag.hover_status == Some(drag.origin_status) {
                "reordering…".to_string()
            } else {
                let dest = drag.hover_status.map(|s| s.label()).unwrap_or("...");
                format!("moving → {dest}")
            }
        } else {
            app.status_message.clone().unwrap_or_default()
        }
    } else if app.pending_g {
        "g…".to_string()
    } else if matches!(app.mode, Mode::Search) {
        "search · enter apply · esc clear".to_string()
    } else {
        app.status_message.clone().unwrap_or_default()
    };

    let hints = match app.view {
        View::TodoList => "j/k nav · space done · a add · t tags · tab kanban · ? help · q quit",
        View::Kanban => "hjkl nav · H/L move · a add · t tags · tab todos · ? help · q quit",
    };

    let left_w = left_text.width() as u16;
    let hints_w = hints.width() as u16;

    if !left_text.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                left_text.as_str(),
                Style::default().fg(if app.pending_g {
                    theme.accent
                } else {
                    theme.subtle
                }),
            )),
            Rect::new(area.x, area.y, left_w.min(area.width), 1),
        );
    }

    if area.width > left_w + hints_w + 2 {
        let rx = area.right() - hints_w;
        frame.render_widget(
            Paragraph::new(Span::styled(hints, Style::default().fg(theme.muted))),
            Rect::new(rx, area.y, hints_w, 1),
        );
    } else if left_text.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(hints, Style::default().fg(theme.muted))),
            area,
        );
    }
}

fn render_task_modal(frame: &mut Frame, app: &mut App, form: &TaskForm) {
    let theme = app.theme.clone();
    let area = centered_rect(58, 14, frame.area());
    frame.render_widget(Clear, area);

    let title = if form.editing_id.is_some() {
        " Edit Task "
    } else {
        " New Task "
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.muted))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.width < 6 || inner.height < 6 {
        return;
    }

    let pad = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Title
            Constraint::Length(2), // Notes
            Constraint::Length(2), // Tags
            Constraint::Length(2), // Status & Priority
            Constraint::Length(1), // Error / spacer
            Constraint::Length(1), // Actions
        ])
        .split(pad);

    let title_active = form.active_field == FormField::Title;
    let title_w = rows[0].width as usize;
    let (title_slice, title_cursor_off) =
        visible_text_slice(&form.title.value, form.title.cursor, title_w);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Title",
                field_label_style(&theme, title_active),
            )),
            Line::from(Span::styled(
                if title_slice.is_empty() && !title_active {
                    "—"
                } else {
                    title_slice
                },
                Style::default().fg(if title_slice.is_empty() {
                    theme.muted
                } else {
                    theme.text
                }),
            )),
        ]),
        rows[0],
    );

    let desc_active = form.active_field == FormField::Description;
    let desc_w = rows[1].width as usize;
    let desc_display = form.description.value.replace('\n', " ↵ ");
    let (desc_slice, desc_cursor_off) = if form.description.value.contains('\n') {
        let (preview, extra) = markdown_preview(&form.description.value);
        let summary = format!("{preview}  [md +{extra} · Ctrl-E]");
        (truncate_to_width(&summary, desc_w), 0)
    } else {
        let (s, off) = visible_text_slice(&desc_display, form.description.cursor, desc_w);
        (s.to_string(), off)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Notes", field_label_style(&theme, desc_active)),
                Span::styled(
                    format!("  (Ctrl-E: {})", app.editor_label),
                    Style::default().fg(theme.muted),
                ),
            ]),
            Line::from(Span::styled(
                if desc_slice.is_empty() && !desc_active {
                    "optional markdown".to_string()
                } else {
                    desc_slice
                },
                Style::default().fg(if form.description.value.is_empty() {
                    theme.muted
                } else {
                    theme.text
                }),
            )),
        ]),
        rows[1],
    );

    let tags_due_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(rows[2]);

    let tags_active = form.active_field == FormField::Tags;
    let tags_w = tags_due_cols[0].width.saturating_sub(1) as usize;
    let (tags_slice, tags_cursor_off) =
        visible_text_slice(&form.tags.value, form.tags.cursor, tags_w);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Tags",
                field_label_style(&theme, tags_active),
            )),
            Line::from(Span::styled(
                if tags_slice.is_empty() && !tags_active {
                    "work, personal"
                } else {
                    tags_slice
                },
                Style::default().fg(if tags_slice.is_empty() {
                    theme.muted
                } else {
                    theme.text
                }),
            )),
        ]),
        tags_due_cols[0],
    );

    let due_active = form.active_field == FormField::Due;
    let due_w = tags_due_cols[1].width as usize;
    let (due_slice, due_cursor_off) = visible_text_slice(&form.due.value, form.due.cursor, due_w);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled("Due", field_label_style(&theme, due_active))),
            Line::from(Span::styled(
                if due_slice.is_empty() && !due_active {
                    "YYYY-MM-DD"
                } else {
                    due_slice
                },
                Style::default().fg(if due_slice.is_empty() {
                    theme.muted
                } else {
                    theme.text
                }),
            )),
        ]),
        tags_due_cols[1],
    );

    let meta_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[3]);

    let status_active = form.active_field == FormField::Status;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Status",
                field_label_style(&theme, status_active),
            )),
            Line::from(vec![
                Span::styled(
                    format!("{} ", form.status.glyph()),
                    status_glyph_style(&theme, form.status),
                ),
                Span::styled(
                    form.status.label(),
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(if status_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]),
        ]),
        meta_cols[0],
    );

    let prio_active = form.active_field == FormField::Priority;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Priority",
                field_label_style(&theme, prio_active),
            )),
            Line::from(vec![
                Span::styled(
                    format!("{} ", form.priority.dot()),
                    priority_tag_style(&theme, form.priority, true),
                ),
                Span::styled(
                    form.priority.label(),
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(if prio_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]),
        ]),
        meta_cols[1],
    );

    if let Some(err) = &form.error {
        frame.render_widget(
            Paragraph::new(Span::styled(err.as_str(), Style::default().fg(theme.error))),
            rows[4],
        );
    }

    let save_label = "Save ↵";
    let cancel_label = "Cancel";
    let btn_y = rows[5].y;
    let save_w = (save_label.width() as u16).min(rows[5].width);
    let save_rect = Rect::new(rows[5].x, btn_y, save_w, 1);
    let cancel_x = rows[5].x.saturating_add(save_w + 3);
    let cancel_w =
        (cancel_label.width() as u16).min(rows[5].right().saturating_sub(cancel_x));
    let cancel_rect = Rect::new(cancel_x, btn_y, cancel_w, 1);

    frame.render_widget(
        Paragraph::new(Span::styled(
            save_label,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        save_rect,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(cancel_label, Style::default().fg(theme.muted))),
        cancel_rect,
    );

    let hint_label = "tab next";
    let hint_w = hint_label.len() as u16;
    if rows[5].right() > cancel_x + cancel_w + hint_w + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(hint_label, Style::default().fg(theme.muted))),
            Rect::new(rows[5].right() - hint_w, btn_y, hint_w, 1),
        );
    }

    if title_active {
        let cx = rows[0].x + (title_cursor_off as u16);
        let cy = rows[0].y + 1;
        if cx < rows[0].right() && cy < rows[0].bottom() {
            frame.set_cursor_position(Position::new(cx, cy));
        }
    } else if desc_active && !form.description.value.contains('\n') {
        let cx = rows[1].x + (desc_cursor_off as u16);
        let cy = rows[1].y + 1;
        if cx < rows[1].right() && cy < rows[1].bottom() {
            frame.set_cursor_position(Position::new(cx, cy));
        }
    } else if tags_active {
        let cx = tags_due_cols[0].x + (tags_cursor_off as u16);
        let cy = tags_due_cols[0].y + 1;
        if cx < tags_due_cols[0].right() && cy < tags_due_cols[0].bottom() {
            frame.set_cursor_position(Position::new(cx, cy));
        }
    } else if due_active {
        let cx = tags_due_cols[1].x + (due_cursor_off as u16);
        let cy = tags_due_cols[1].y + 1;
        if cx < tags_due_cols[1].right() && cy < tags_due_cols[1].bottom() {
            frame.set_cursor_position(Position::new(cx, cy));
        }
    }

    app.regions.modal = Some(ModalRegions {
        area,
        title: rows[0],
        description: rows[1],
        tags: tags_due_cols[0],
        due: tags_due_cols[1],
        status: meta_cols[0],
        priority: meta_cols[1],
        save_btn: save_rect,
        cancel_btn: cancel_rect,
    });
}

fn render_tag_filter_modal(frame: &mut Frame, app: &mut App, selected: usize) {
    let all_tags = app.all_tags();
    if all_tags.is_empty() {
        return;
    }

    let theme = app.theme.clone();
    let desired_h = (all_tags.len() as u16 + 4).clamp(6, 16);
    let area = centered_rect(42, desired_h, frame.area());
    frame.render_widget(Clear, area);

    let mode_text = format!(" match: {} ", app.tag_match_mode.label());
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.muted))
        .title(Span::styled(
            " Filter Tags ",
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ))
        .title(
            Line::from(Span::styled(mode_text, Style::default().fg(theme.accent)))
                .alignment(Alignment::Right),
        );
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.width < 6 || inner.height < 2 {
        return;
    }

    let list_h = inner.height.saturating_sub(1);
    let visible_rows = list_h as usize;
    let footer_y = inner.bottom().saturating_sub(1);
    let mut rows_regions = Vec::new();
    let clamped_sel = selected.min(all_tags.len().saturating_sub(1));
    let scroll = if visible_rows == 0 || clamped_sel < visible_rows {
        0
    } else {
        (clamped_sel + 1 - visible_rows).min(all_tags.len().saturating_sub(visible_rows))
    };

    for (row_offset, (idx, tag)) in all_tags
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible_rows)
        .enumerate()
    {
        let y = inner.y + row_offset as u16;
        let row_rect = Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1);
        let is_cursor = idx == clamped_sel;
        let is_active = app.is_tag_active(tag);
        let count = app
            .store
            .tasks
            .iter()
            .filter(|t| app.effective_tags(t).iter().any(|x| x == tag))
            .count();

        let pointer = if is_cursor {
            Span::styled("▸ ", Style::default().fg(theme.accent))
        } else {
            Span::raw("  ")
        };
        let check = if is_active {
            Span::styled("✓ ", Style::default().fg(theme.tag_active))
        } else {
            Span::styled("○ ", Style::default().fg(theme.muted))
        };
        let label_style = if is_active || is_cursor {
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.subtle)
        };

        let left_line = Line::from(vec![
            pointer,
            check,
            Span::styled(format!("#{tag}"), label_style),
        ]);
        frame.render_widget(Paragraph::new(left_line), row_rect);

        let count_str = count.to_string();
        let cw = count_str.len() as u16;
        if row_rect.width > cw + 6 {
            frame.render_widget(
                Paragraph::new(Span::styled(count_str, Style::default().fg(theme.muted))),
                Rect::new(row_rect.right() - cw, y, cw, 1),
            );
        }

        rows_regions.push((idx, tag.clone(), row_rect));
    }

    let mode_label = format!("m {}", app.tag_match_mode.label());
    let clear_label = "c clear";
    let mode_w = mode_label.len() as u16;
    let clear_w = clear_label.len() as u16;
    let x = inner.x + 1;
    let mode_rect = Rect::new(x, footer_y, mode_w.min(inner.width.saturating_sub(2)), 1);
    let clear_x = x.saturating_add(mode_w + 2);
    let clear_rect = Rect::new(
        clear_x,
        footer_y,
        clear_w.min(inner.right().saturating_sub(clear_x)),
        1,
    );

    frame.render_widget(
        Paragraph::new(Span::styled(mode_label, Style::default().fg(theme.accent))),
        mode_rect,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(clear_label, Style::default().fg(theme.subtle))),
        clear_rect,
    );

    let hint = "space toggle";
    let hint_w = hint.len() as u16;
    if inner.right().saturating_sub(1) > clear_x + clear_w + hint_w + 1 {
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(theme.muted))),
            Rect::new(inner.right().saturating_sub(1 + hint_w), footer_y, hint_w, 1),
        );
    }

    app.regions.tag_modal = Some(TagModalRegions {
        area,
        rows: rows_regions,
        mode_btn: mode_rect,
        clear_btn: clear_rect,
    });
}

fn render_notes_preview_modal(frame: &mut Frame, app: &mut App, task_id: u64, scroll: u16) {
    let Some(task) = app.store.get(task_id).cloned() else {
        app.mode = Mode::Normal;
        return;
    };
    let theme = app.theme.clone();
    let eff_tags = app.effective_tags(&task);
    let area = centered_rect(72, 22, frame.area());
    frame.render_widget(Clear, area);
    app.regions.notes_modal = Some(area);

    let title_label = format!(" {} ", truncate_to_width(&task.title, area.width.saturating_sub(24) as usize));
    let meta_badge = format!(" {} {} · {} ", task.status.glyph(), task.status.label(), task.priority.label());
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            title_label,
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ))
        .title(
            Line::from(Span::styled(
                meta_badge,
                Style::default().fg(status_color(&theme, task.status)),
            ))
            .alignment(Alignment::Right),
        );
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.width < 6 || inner.height < 3 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !eff_tags.is_empty() || task.due.is_some() || task.checklist_progress().is_some() {
        let mut meta_spans: Vec<Span<'static>> = Vec::new();
        for (i, tag) in eff_tags.iter().enumerate() {
            if i > 0 {
                meta_spans.push(Span::raw(" "));
            }
            meta_spans.push(Span::styled(
                format!("#{tag}"),
                Style::default().fg(theme.accent),
            ));
        }
        if let Some(due) = &task.due {
            if !meta_spans.is_empty() {
                meta_spans.push(Span::styled("  ·  ", Style::default().fg(theme.muted)));
            }
            let today = today_ymd();
            let color = match task.due_state(&today) {
                Some(DueState::Overdue) => theme.error,
                Some(DueState::Today) => theme.in_progress,
                _ => theme.subtle,
            };
            meta_spans.push(Span::styled(
                format!("Due {due}"),
                Style::default().fg(color),
            ));
        }
        if let Some((done, total)) = task.checklist_progress() {
            if !meta_spans.is_empty() {
                meta_spans.push(Span::styled("  ·  ", Style::default().fg(theme.muted)));
            }
            meta_spans.push(Span::styled(
                format!("Checklist {done}/{total}"),
                Style::default().fg(if done == total {
                    theme.done
                } else {
                    theme.subtle
                }),
            ));
        }
        lines.push(Line::from(meta_spans));
        lines.push(Line::raw(""));
    }

    if task.description.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            format!("No notes yet. Press 'n' to write Markdown in {}.", app.editor_label),
            Style::default().fg(theme.muted),
        )));
    } else {
        let mut in_code_fence = false;
        for raw_line in task.description.lines() {
            let trimmed = raw_line.trim_start();
            if trimmed.starts_with("```") {
                in_code_fence = !in_code_fence;
                lines.push(Line::from(Span::styled(
                    raw_line.to_string(),
                    Style::default().fg(theme.muted),
                )));
                continue;
            }
            if in_code_fence {
                lines.push(Line::from(Span::styled(
                    raw_line.to_string(),
                    Style::default().fg(theme.subtle),
                )));
                continue;
            }
            if let Some(heading) = trimmed.strip_prefix("# ") {
                lines.push(Line::from(Span::styled(
                    heading.to_string(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )));
            } else if let Some(heading) = trimmed
                .strip_prefix("## ")
                .or_else(|| trimmed.strip_prefix("### "))
            {
                lines.push(Line::from(Span::styled(
                    heading.to_string(),
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD),
                )));
            } else if let Some(rest) = trimmed
                .strip_prefix("- [x] ")
                .or_else(|| trimmed.strip_prefix("- [X] "))
                .or_else(|| trimmed.strip_prefix("* [x] "))
                .or_else(|| trimmed.strip_prefix("* [X] "))
            {
                let indent = &raw_line[..raw_line.len() - trimmed.len()];
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled("✓ ", Style::default().fg(theme.done)),
                    Span::styled(
                        rest.to_string(),
                        Style::default()
                            .fg(theme.muted)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]));
            } else if let Some(rest) = trimmed
                .strip_prefix("- [ ] ")
                .or_else(|| trimmed.strip_prefix("* [ ] "))
            {
                let indent = &raw_line[..raw_line.len() - trimmed.len()];
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled("○ ", Style::default().fg(theme.todo)),
                    Span::styled(rest.to_string(), Style::default().fg(theme.text)),
                ]));
            } else if let Some(rest) = trimmed.strip_prefix("> ") {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.muted)),
                    Span::styled(
                        rest.to_string(),
                        Style::default()
                            .fg(theme.subtle)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            } else if let Some(rest) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                let indent = &raw_line[..raw_line.len() - trimmed.len()];
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled("• ", Style::default().fg(theme.accent)),
                    Span::styled(rest.to_string(), Style::default().fg(theme.text)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    raw_line.to_string(),
                    Style::default().fg(theme.text),
                )));
            }
        }
    }

    let body_h = inner.height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(body_h);
    let clamped_scroll = scroll.min(max_scroll);
    if clamped_scroll != scroll {
        app.mode = Mode::NotesPreview {
            task_id,
            scroll: clamped_scroll,
        };
    }

    let body_rect = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        body_h,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((clamped_scroll, 0)),
        body_rect,
    );

    let footer_y = inner.bottom().saturating_sub(1);
    let hint = format!(
        "j/k scroll · n edit in {} · e edit task · esc close",
        app.editor_label
    );
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(theme.muted))),
        Rect::new(inner.x + 1, footer_y, inner.width.saturating_sub(2), 1),
    );
}

fn render_help_modal(frame: &mut Frame, app: &mut App) {
    let theme = app.theme.clone();
    let area = centered_rect(58, 27, frame.area());
    frame.render_widget(Clear, area);
    app.regions.help_modal = Some(area);

    let lines = vec![
        help_section(&theme, "Navigate"),
        help_row(&theme, "j / k  ↑ / ↓", "Move down / up"),
        help_row(&theme, "h / l  ← / →", "Column (Kanban) or filter (Todos)"),
        help_row(&theme, "gg / G", "First / last task"),
        help_row(&theme, "Tab  gt  1  2", "Switch Todos / Kanban"),
        Line::raw(""),
        help_section(&theme, "Tasks"),
        help_row(&theme, "a / o", "New task"),
        help_row(&theme, "e / i / Enter", "Edit task"),
        help_row(&theme, "v", "Preview task & Markdown notes"),
        help_row(
            &theme,
            "n",
            format!("Edit Markdown notes in {}", app.editor_label),
        ),
        help_row(&theme, "Space", "Toggle done / cycle status"),
        help_row(&theme, "H / L  < / >", "Move column left / right"),
        help_row(&theme, "J / K", "Reorder down / up"),
        help_row(&theme, "s / p", "Cycle status / priority"),
        help_row(&theme, "O", "Cycle sort (manual/priority/due/updated)"),
        help_row(&theme, "t / T", "Filter tags / edit tags"),
        help_row(&theme, "S", "Sync via SSH"),
        help_row(&theme, "d / x", "Delete task"),
        help_row(&theme, "A", "Archive all Done tasks"),
        help_row(&theme, "u / Ctrl-R", "Undo / Redo"),
        help_row(&theme, "/   Esc", "Search   Clear filter"),
        Line::raw(""),
        help_section(&theme, "Mouse"),
        help_row(&theme, "Click", "Tabs, filters, #tags, status, cards"),
        help_row(&theme, "Double-click", "Edit task"),
        help_row(&theme, "Drag card", "Reorder or move between Kanban columns"),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.muted))
        .title(Span::styled(
            " Shortcuts ",
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        ),
    );
}

fn help_section(theme: &Theme, title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD),
    ))
}

fn help_row(theme: &Theme, keys: &'static str, desc: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{keys:<16}"), Style::default().fg(theme.accent)),
        Span::styled(desc.into(), Style::default().fg(theme.subtle)),
    ])
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > max_width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

fn visible_text_slice(text: &str, cursor: usize, width: usize) -> (&str, usize) {
    if width == 0 {
        return ("", 0);
    }
    let chars: Vec<(usize, usize)> = text
        .char_indices()
        .map(|(byte_idx, ch)| (byte_idx, ch.width().unwrap_or(0)))
        .collect();
    let clamped_cursor = cursor.min(chars.len());
    let max_cursor_col = width.saturating_sub(1);

    // Choose the earliest start char index so the cursor fits within `max_cursor_col` cells
    let mut start = clamped_cursor;
    let mut cursor_col = 0usize;
    while start > 0 {
        let cw = chars[start - 1].1;
        if cursor_col + cw > max_cursor_col {
            break;
        }
        cursor_col += cw;
        start -= 1;
    }

    let start_byte = chars.get(start).map(|(b, _)| *b).unwrap_or(text.len());
    let mut end = start;
    let mut used = 0usize;
    while end < chars.len() {
        let cw = chars[end].1;
        if used + cw > width {
            break;
        }
        used += cw;
        end += 1;
    }
    let end_byte = chars.get(end).map(|(b, _)| *b).unwrap_or(text.len());
    (&text[start_byte..end_byte], cursor_col)
}

fn centered_rect(max_width: u16, max_height: u16, area: Rect) -> Rect {
    let w = max_width
        .min(area.width.saturating_sub(2))
        .max(10)
        .min(area.width);
    let h = max_height
        .min(area.height.saturating_sub(2))
        .max(6)
        .min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn field_label_style(theme: &Theme, active: bool) -> Style {
    if active {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    }
}

fn status_color(theme: &Theme, status: Status) -> ratatui::style::Color {
    match status {
        Status::Todo => theme.todo,
        Status::InProgress => theme.in_progress,
        Status::Done => theme.done,
    }
}

fn status_glyph_style(theme: &Theme, status: Status) -> Style {
    Style::default().fg(status_color(theme, status))
}

fn status_tag_style(theme: &Theme, status: Status, selected: bool) -> Style {
    if selected {
        Style::default().fg(status_color(theme, status))
    } else {
        Style::default().fg(theme.muted)
    }
}

fn tags_border_line<'a>(
    theme: &Theme,
    tags: &'a [String],
    active_tags: &'a [String],
    max_width: usize,
) -> Option<Line<'a>> {
    if tags.is_empty() || max_width < 4 {
        return None;
    }

    let mut spans = vec![Span::raw(" ")];
    let mut used: usize = 2; // leading + trailing space

    for (idx, tag) in tags.iter().enumerate() {
        let label = format!("#{tag}");
        let item_w = label.width() + usize::from(idx > 0);
        let remaining_after = tags.len().saturating_sub(idx + 1);
        let reserve = if remaining_after > 0 {
            format!(" +{remaining_after}").len()
        } else {
            0
        };

        if used + item_w + reserve > max_width {
            let hidden = tags.len() - idx;
            let overflow = if idx > 0 {
                format!(" +{hidden}")
            } else {
                format!("+{hidden}")
            };
            if used + overflow.len() <= max_width {
                spans.push(Span::styled(overflow, Style::default().fg(theme.muted)));
            }
            break;
        }

        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        let is_active = active_tags.iter().any(|a| a == tag);
        spans.push(Span::styled(
            label,
            Style::default().fg(if is_active {
                theme.tag_active
            } else {
                theme.tag
            }),
        ));
        used += item_w;
    }

    if spans.len() == 1 {
        return None;
    }
    spans.push(Span::raw(" "));
    Some(Line::from(spans))
}

fn due_border_line(theme: &Theme, task: &Task) -> Option<Line<'static>> {
    let due = task.due.as_deref()?;
    let today = today_ymd();
    let style = match task.due_state(&today) {
        Some(DueState::Overdue) => Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
        Some(DueState::Today) => Style::default().fg(theme.in_progress),
        _ => Style::default().fg(theme.muted),
    };
    Some(Line::from(Span::styled(format!(" {due} "), style)))
}

fn priority_tag_style(theme: &Theme, priority: Priority, selected: bool) -> Style {
    let base = match priority {
        Priority::Low => Style::default().fg(theme.priority_low),
        Priority::Medium => Style::default().fg(theme.priority_medium),
        Priority::High => Style::default().fg(theme.priority_high),
    };
    if selected && priority == Priority::High {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_text_slice_handles_wide_unicode_chars() {
        // Each CJK char is 2 display cells wide
        let text = "日本語タスク";
        let (slice, cursor_col) = visible_text_slice(text, 6, 8);
        assert!(slice.width() <= 8);
        assert!(cursor_col < 8);
        assert_eq!(slice, "タスク");
        assert_eq!(cursor_col, 6);

        assert_eq!(truncate_to_width("日本語", 5), "日本");
    }
}
