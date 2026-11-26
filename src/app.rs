use crate::config::{normalize_device_tag, AppConfig, SyncConfig};
use crate::editor::editor_display_name;
use crate::model::{
    file_stamp, format_tags, now_millis, parse_tags, FileStamp, Priority, Status, Task, TaskStore,
};
use crate::sync::{merge_stores, SyncOutcome};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    TodoList,
    Kanban,
}

impl View {
    pub fn toggle(self) -> Self {
        match self {
            View::TodoList => View::Kanban,
            View::Kanban => View::TodoList,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoFilter {
    All,
    Todo,
    InProgress,
    Done,
}

impl TodoFilter {
    pub const ALL: [TodoFilter; 4] = [
        TodoFilter::All,
        TodoFilter::Todo,
        TodoFilter::InProgress,
        TodoFilter::Done,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TodoFilter::All => "All",
            TodoFilter::Todo => "To Do",
            TodoFilter::InProgress => "In Progress",
            TodoFilter::Done => "Done",
        }
    }

    pub fn matches(self, status: Status) -> bool {
        match self {
            TodoFilter::All => true,
            TodoFilter::Todo => status == Status::Todo,
            TodoFilter::InProgress => status == Status::InProgress,
            TodoFilter::Done => status == Status::Done,
        }
    }

    pub fn next(self) -> Self {
        match self {
            TodoFilter::All => TodoFilter::Todo,
            TodoFilter::Todo => TodoFilter::InProgress,
            TodoFilter::InProgress => TodoFilter::Done,
            TodoFilter::Done => TodoFilter::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            TodoFilter::All => TodoFilter::Done,
            TodoFilter::Todo => TodoFilter::All,
            TodoFilter::InProgress => TodoFilter::Todo,
            TodoFilter::Done => TodoFilter::InProgress,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Title,
    Description,
    Tags,
    Status,
    Priority,
}

impl FormField {
    pub fn next(self) -> Self {
        match self {
            FormField::Title => FormField::Description,
            FormField::Description => FormField::Tags,
            FormField::Tags => FormField::Status,
            FormField::Status => FormField::Priority,
            FormField::Priority => FormField::Title,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            FormField::Title => FormField::Priority,
            FormField::Description => FormField::Title,
            FormField::Tags => FormField::Description,
            FormField::Status => FormField::Tags,
            FormField::Priority => FormField::Status,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextInput {
    pub value: String,
    /// Cursor position measured in Unicode scalar values (chars).
    pub cursor: usize,
}

impl TextInput {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    pub fn insert_char(&mut self, ch: char) {
        let idx = self.byte_index(self.cursor);
        self.value.insert(idx, ch);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let len = self.value.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let len = self.value.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.value.chars().count();
    }
}

#[derive(Debug, Clone)]
pub struct TaskForm {
    pub editing_id: Option<u64>,
    pub title: TextInput,
    pub description: TextInput,
    pub tags: TextInput,
    pub status: Status,
    pub priority: Priority,
    pub active_field: FormField,
    pub error: Option<String>,
}

impl TaskForm {
    pub fn new_task(default_status: Status, default_tag: Option<&str>) -> Self {
        Self {
            editing_id: None,
            title: TextInput::new(""),
            description: TextInput::new(""),
            tags: TextInput::new(default_tag.unwrap_or("")),
            status: default_status,
            priority: Priority::Medium,
            active_field: FormField::Title,
            error: None,
        }
    }

    pub fn edit_task(task: &Task) -> Self {
        Self {
            editing_id: Some(task.id),
            title: TextInput::new(task.title.clone()),
            description: TextInput::new(task.description.clone()),
            tags: TextInput::new(format_tags(&task.tags)),
            status: task.status,
            priority: task.priority,
            active_field: FormField::Title,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagMatchMode {
    /// Task must have all selected tags (AND).
    All,
    /// Task must have at least one selected tag (OR).
    Any,
}

impl TagMatchMode {
    pub fn toggle(self) -> Self {
        match self {
            TagMatchMode::All => TagMatchMode::Any,
            TagMatchMode::Any => TagMatchMode::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TagMatchMode::All => "and",
            TagMatchMode::Any => "or",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingEditor {
    Task(u64),
    Modal,
}

#[derive(Debug, Clone)]
pub enum Mode {
    Normal,
    Search,
    TaskModal(TaskForm),
    TagFilter(usize),
    Help,
}

#[derive(Debug, Clone)]
pub struct DragState {
    pub task_id: u64,
    pub origin_status: Status,
    pub start_x: u16,
    pub start_y: u16,
    pub current_x: u16,
    pub current_y: u16,
    pub active: bool,
    pub hover_status: Option<Status>,
}

#[derive(Debug, Default, Clone)]
pub struct ModalRegions {
    pub area: Rect,
    pub title: Rect,
    pub description: Rect,
    pub tags: Rect,
    pub status: Rect,
    pub priority: Rect,
    pub save_btn: Rect,
    pub cancel_btn: Rect,
}

#[derive(Debug, Default, Clone)]
pub struct TagModalRegions {
    pub area: Rect,
    pub rows: Vec<(usize, String, Rect)>,
    pub mode_btn: Rect,
    pub clear_btn: Rect,
}

#[derive(Debug, Default, Clone)]
pub struct LayoutRegions {
    pub tab_todos: Rect,
    pub tab_kanban: Rect,
    pub sync_btn: Rect,
    pub add_btn: Rect,
    pub help_btn: Rect,
    pub filter_pills: Vec<(TodoFilter, Rect)>,
    /// None clears all tag filters; Some(tag) toggles that tag in tag_filters.
    pub tag_pills: Vec<(Option<String>, Rect)>,
    pub tag_mode_btn: Rect,
    pub tag_overflow_btn: Rect,
    pub todo_list_area: Rect,
    /// (task_id, full_row_rect, checkbox_rect)
    pub todo_rows: Vec<(u64, Rect, Rect)>,
    /// (status, full_column_rect)
    pub kanban_columns: Vec<(Status, Rect)>,
    /// (task_id, status, card_rect)
    pub kanban_cards: Vec<(u64, Status, Rect)>,
    pub modal: Option<ModalRegions>,
    pub tag_modal: Option<TagModalRegions>,
    pub help_modal: Option<Rect>,
}

impl LayoutRegions {
    pub fn clear(&mut self) {
        self.tab_todos = Rect::default();
        self.tab_kanban = Rect::default();
        self.sync_btn = Rect::default();
        self.add_btn = Rect::default();
        self.help_btn = Rect::default();
        self.filter_pills.clear();
        self.tag_pills.clear();
        self.tag_mode_btn = Rect::default();
        self.tag_overflow_btn = Rect::default();
        self.todo_list_area = Rect::default();
        self.todo_rows.clear();
        self.kanban_columns.clear();
        self.kanban_cards.clear();
        self.modal = None;
        self.tag_modal = None;
        self.help_modal = None;
    }
}

pub fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[derive(Debug, Clone)]
struct LastClick {
    task_id: u64,
    at: Instant,
}

pub struct App {
    pub store: TaskStore,
    pub storage_path: Option<PathBuf>,
    pub theme: Theme,
    pub editor_config: Option<String>,
    pub editor_label: String,
    pub sync_config: SyncConfig,
    pub sync_requested: bool,
    pub syncing: bool,
    pub pending_editor: Option<PendingEditor>,
    pub view: View,
    pub mode: Mode,
    pub todo_filter: TodoFilter,
    pub tag_filters: Vec<String>,
    pub tag_match_mode: TagMatchMode,
    pub todo_selected: usize,
    pub todo_scroll: usize,
    pub kanban_col: usize,
    pub kanban_selected: [usize; 3],
    pub kanban_scroll: [usize; 3],
    pub search_query: String,
    pub search_input: TextInput,
    pub pending_g: bool,
    pub undo_stack: Vec<TaskStore>,
    pub status_message: Option<String>,
    pub drag: Option<DragState>,
    pub regions: LayoutRegions,
    pub should_quit: bool,
    last_click: Option<LastClick>,
    last_file_stamp: Option<FileStamp>,
    dirty: bool,
}

const MAX_UNDO: usize = 50;

impl App {
    #[allow(dead_code)]
    pub fn new(store: TaskStore, storage_path: Option<PathBuf>) -> Self {
        Self::with_config(store, storage_path, AppConfig::default())
    }

    pub fn with_config(store: TaskStore, storage_path: Option<PathBuf>, config: AppConfig) -> Self {
        let theme = config.resolved_theme();
        let editor_label = editor_display_name(config.editor.as_deref());
        let last_file_stamp = storage_path.as_deref().and_then(file_stamp);
        Self {
            store,
            storage_path,
            theme,
            editor_config: config.editor,
            editor_label,
            sync_config: config.sync,
            sync_requested: false,
            syncing: false,
            pending_editor: None,
            view: View::TodoList,
            mode: Mode::Normal,
            todo_filter: TodoFilter::All,
            tag_filters: Vec::new(),
            tag_match_mode: TagMatchMode::All,
            todo_selected: 0,
            todo_scroll: 0,
            kanban_col: 0,
            kanban_selected: [0, 0, 0],
            kanban_scroll: [0, 0, 0],
            search_query: String::new(),
            search_input: TextInput::new(""),
            pending_g: false,
            undo_stack: Vec::new(),
            status_message: None,
            drag: None,
            regions: LayoutRegions::default(),
            should_quit: false,
            last_click: None,
            last_file_stamp,
            dirty: false,
        }
    }

    pub fn current_kanban_status(&self) -> Status {
        Status::from_index(self.kanban_col)
    }

    pub fn effective_tags(&self, task: &Task) -> Vec<String> {
        let mut tags = task.tags.clone();
        if let Some(origin) = &task.origin {
            if self.sync_config.should_inject_tag_for(origin) {
                let slug = normalize_device_tag(origin);
                if !slug.is_empty() && !tags.iter().any(|t| t == &slug) {
                    tags.push(slug);
                }
            }
        }
        tags
    }

    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .store
            .tasks
            .iter()
            .flat_map(|t| self.effective_tags(t))
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    fn matches_search(&self, task: &Task) -> bool {
        let q = self.active_search_text().trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        let tag_q = q.trim_start_matches('#');
        let eff_tags = self.effective_tags(task);
        task.title.to_lowercase().contains(&q)
            || task.description.to_lowercase().contains(&q)
            || (!tag_q.is_empty() && eff_tags.iter().any(|t| t.contains(tag_q)))
    }

    pub fn matches_tag(&self, task: &Task) -> bool {
        if self.tag_filters.is_empty() {
            return true;
        }
        let eff_tags = self.effective_tags(task);
        match self.tag_match_mode {
            TagMatchMode::All => self
                .tag_filters
                .iter()
                .all(|f| eff_tags.iter().any(|t| t == f)),
            TagMatchMode::Any => self
                .tag_filters
                .iter()
                .any(|f| eff_tags.iter().any(|t| t == f)),
        }
    }

    pub fn is_tag_active(&self, tag: &str) -> bool {
        self.tag_filters.iter().any(|t| t == tag)
    }

    pub fn active_search_text(&self) -> &str {
        match self.mode {
            Mode::Search => &self.search_input.value,
            _ => &self.search_query,
        }
    }

    pub fn filtered_todo_ids(&self) -> Vec<u64> {
        self.store
            .tasks
            .iter()
            .filter(|t| {
                self.todo_filter.matches(t.status)
                    && self.matches_tag(t)
                    && self.matches_search(t)
            })
            .map(|t| t.id)
            .collect()
    }

    pub fn column_task_ids(&self, status: Status) -> Vec<u64> {
        self.store
            .tasks
            .iter()
            .filter(|t| t.status == status && self.matches_tag(t) && self.matches_search(t))
            .map(|t| t.id)
            .collect()
    }

    pub fn selected_task_id(&self) -> Option<u64> {
        match self.view {
            View::TodoList => {
                let ids = self.filtered_todo_ids();
                ids.get(self.todo_selected).copied()
            }
            View::Kanban => {
                let status = self.current_kanban_status();
                let ids = self.column_task_ids(status);
                ids.get(self.kanban_selected[self.kanban_col]).copied()
            }
        }
    }

    pub fn clamp_selections(&mut self) {
        let existing_tags = self.all_tags();
        self.tag_filters
            .retain(|active_tag| existing_tags.iter().any(|t| t == active_tag));

        let todo_len = self.filtered_todo_ids().len();
        if todo_len == 0 {
            self.todo_selected = 0;
            self.todo_scroll = 0;
        } else if self.todo_selected >= todo_len {
            self.todo_selected = todo_len - 1;
        }

        for status in Status::ALL {
            let idx = status.index();
            let len = self.column_task_ids(status).len();
            if len == 0 {
                self.kanban_selected[idx] = 0;
                self.kanban_scroll[idx] = 0;
            } else if self.kanban_selected[idx] >= len {
                self.kanban_selected[idx] = len - 1;
            }
        }
    }

    fn select_task_by_id(&mut self, task_id: u64) {
        let todo_ids = self.filtered_todo_ids();
        if let Some(pos) = todo_ids.iter().position(|&id| id == task_id) {
            self.todo_selected = pos;
        }
        if let Some(task) = self.store.get(task_id) {
            let status = task.status;
            let col = status.index();
            let col_ids = self.column_task_ids(status);
            if let Some(pos) = col_ids.iter().position(|&id| id == task_id) {
                self.kanban_col = col;
                self.kanban_selected[col] = pos;
            }
        }
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(self.store.clone());
        if self.undo_stack.len() > MAX_UNDO {
            self.undo_stack.remove(0);
        }
    }

    pub fn persist(&mut self) {
        self.persist_inner(true);
    }

    fn persist_inner(&mut self, allow_auto_sync: bool) {
        self.dirty = true;
        if let Some(path) = self.storage_path.clone() {
            let current_stamp = file_stamp(&path);
            if current_stamp.is_some() && current_stamp != self.last_file_stamp {
                if let Ok(mut disk_store) = TaskStore::load_or_empty(&path) {
                    let local_name = self.sync_config.effective_device_name();
                    disk_store.ensure_sync_metadata(&local_name);
                    self.store =
                        merge_stores(&self.store, &disk_store, &local_name, &local_name);
                }
            }
            if let Err(err) = self.store.save(&path) {
                self.status_message = Some(format!("Save failed: {err}"));
                return;
            }
            self.last_file_stamp = file_stamp(&path);
            self.dirty = false;
        }
        if allow_auto_sync
            && self.sync_config.auto_sync_on_save
            && !self.sync_config.peers.is_empty()
        {
            self.sync_requested = true;
        }
    }

    /// Check whether `storage_path` was modified externally (e.g. by CLI, MCP, or another process)
    /// and reload/merge changes into the live TUI without losing selection.
    pub fn reload_if_changed(&mut self) -> bool {
        let Some(path) = self.storage_path.clone() else {
            return false;
        };
        let current_stamp = file_stamp(&path);
        if current_stamp.is_none() || current_stamp == self.last_file_stamp {
            return false;
        }
        let Ok(mut disk_store) = TaskStore::load_or_empty(&path) else {
            return false;
        };
        let local_name = self.sync_config.effective_device_name();
        disk_store.ensure_sync_metadata(&local_name);

        let selected_uid = self
            .selected_task_id()
            .and_then(|id| self.store.get(id))
            .map(|t| t.uid.clone());

        let next_store = if self.dirty {
            merge_stores(&self.store, &disk_store, &local_name, &local_name)
        } else {
            disk_store
        };

        let changed = next_store != self.store;
        self.store = next_store;
        self.last_file_stamp = current_stamp;
        self.clamp_selections();

        if let Some(uid) = selected_uid {
            if let Some(id) = self.store.tasks.iter().find(|t| t.uid == uid).map(|t| t.id) {
                self.select_task_by_id(id);
            }
        }

        if changed {
            self.set_message("Reloaded external changes");
        }
        changed
    }

    pub fn request_sync(&mut self) {
        if self.sync_config.peers.is_empty() {
            self.set_message("No SSH peers configured in config.toml");
            return;
        }
        if self.syncing {
            self.set_message("Sync already in progress…");
            return;
        }
        self.sync_requested = true;
        self.set_message("Syncing via SSH…");
    }

    pub fn apply_sync_outcome(&mut self, outcome: SyncOutcome) {
        let local_name = self.sync_config.effective_device_name();
        let selected_uid = self
            .selected_task_id()
            .and_then(|id| self.store.get(id))
            .map(|t| t.uid.clone());

        // Merge again against current local state to preserve edits made while SSH ran
        self.store = merge_stores(&self.store, &outcome.merged_store, &local_name, &local_name);
        self.persist_inner(false);
        self.clamp_selections();

        if let Some(uid) = selected_uid {
            if let Some(id) = self.store.tasks.iter().find(|t| t.uid == uid).map(|t| t.id) {
                self.select_task_by_id(id);
            }
        }

        if outcome.errors.is_empty() {
            let noun = if outcome.synced_peers == 1 {
                "peer"
            } else {
                "peers"
            };
            self.set_message(format!("Synced {} {noun}", outcome.synced_peers));
        } else {
            self.set_message(format!("Sync error: {}", outcome.errors[0]));
        }
    }

    fn set_message(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.dirty {
                self.persist();
            }
            self.should_quit = true;
            return;
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Search => self.handle_search_key(key),
            Mode::TaskModal(_) => self.handle_modal_key(key),
            Mode::TagFilter(_) => self.handle_tag_filter_key(key),
            Mode::Help => self.handle_help_key(key),
        }
    }

    fn handle_tag_filter_key(&mut self, key: KeyEvent) {
        let Mode::TagFilter(mut sel) = self.mode else {
            return;
        };
        let tags = self.all_tags();
        if tags.is_empty() {
            self.mode = Mode::Normal;
            return;
        }
        if sel >= tags.len() {
            sel = tags.len() - 1;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('t') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if sel + 1 < tags.len() {
                    sel += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                sel = sel.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                sel = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                sel = tags.len().saturating_sub(1);
            }
            KeyCode::Char(' ') => {
                self.toggle_tag_filter(tags[sel].clone());
            }
            KeyCode::Char('m') | KeyCode::Tab => {
                self.toggle_tag_match_mode();
            }
            KeyCode::Char('c') => {
                self.clear_tag_filters();
            }
            _ => {}
        }
        self.mode = Mode::TagFilter(sel);
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_query.clear();
                self.search_input = TextInput::new("");
                self.mode = Mode::Normal;
                self.clamp_selections();
                self.set_message("Search cleared");
            }
            KeyCode::Enter => {
                self.search_query = self.search_input.value.clone();
                self.mode = Mode::Normal;
                self.clamp_selections();
                if self.search_query.trim().is_empty() {
                    self.set_message("Search cleared");
                } else {
                    self.set_message(format!("Filter: /{}", self.search_query));
                }
            }
            KeyCode::Backspace => {
                self.search_input.backspace();
                self.clamp_selections();
            }
            KeyCode::Delete => {
                self.search_input.delete();
                self.clamp_selections();
            }
            KeyCode::Left => self.search_input.move_left(),
            KeyCode::Right => self.search_input.move_right(),
            KeyCode::Home => self.search_input.move_home(),
            KeyCode::End => self.search_input.move_end(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.insert_char(c);
                self.clamp_selections();
            }
            _ => {}
        }
    }

    pub fn open_notes_in_editor(&mut self) {
        if let Some(id) = self.selected_task_id() {
            self.pending_editor = Some(PendingEditor::Task(id));
        }
    }

    pub fn editor_initial_content(&self, target: PendingEditor) -> Option<(String, String)> {
        match target {
            PendingEditor::Task(id) => self
                .store
                .get(id)
                .map(|t| (t.description.clone(), t.title.clone())),
            PendingEditor::Modal => {
                if let Mode::TaskModal(form) = &self.mode {
                    let title = if form.title.value.trim().is_empty() {
                        "task".to_string()
                    } else {
                        form.title.value.clone()
                    };
                    Some((form.description.value.clone(), title))
                } else {
                    None
                }
            }
        }
    }

    pub fn apply_editor_result(
        &mut self,
        target: PendingEditor,
        result: Result<Option<String>, String>,
    ) {
        match result {
            Err(err) => {
                self.set_message(format!("Editor error: {err}"));
            }
            Ok(None) => {
                self.set_message("Notes unchanged");
            }
            Ok(Some(markdown)) => match target {
                PendingEditor::Task(id) => {
                    self.push_undo();
                    if let Some(task) = self.store.get_mut(id) {
                        task.description = markdown;
                        task.touch();
                        self.persist();
                        self.set_message("Notes updated");
                    }
                }
                PendingEditor::Modal => {
                    if let Mode::TaskModal(form) = &mut self.mode {
                        form.description = TextInput::new(markdown);
                        self.set_message("Notes updated in form");
                    }
                }
            },
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Mode::TaskModal(mut form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.mode = Mode::TaskModal(form);
            self.pending_editor = Some(PendingEditor::Modal);
            return;
        }

        let mut keep_open = true;
        let mut submit = false;

        match key.code {
            KeyCode::Esc => {
                keep_open = false;
            }
            KeyCode::Tab => {
                form.active_field = form.active_field.next();
            }
            KeyCode::BackTab => {
                form.active_field = form.active_field.prev();
            }
            KeyCode::Enter => {
                if form.active_field == FormField::Title && form.title.value.trim().is_empty() {
                    form.error = Some("Title cannot be empty".into());
                } else {
                    submit = true;
                }
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.active_field = form.active_field.next();
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.active_field = form.active_field.prev();
            }
            _ => match form.active_field {
                FormField::Title => match key.code {
                    KeyCode::Backspace => form.title.backspace(),
                    KeyCode::Delete => form.title.delete(),
                    KeyCode::Left => form.title.move_left(),
                    KeyCode::Right => form.title.move_right(),
                    KeyCode::Home => form.title.move_home(),
                    KeyCode::End => form.title.move_end(),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        form.title.insert_char(c);
                        form.error = None;
                    }
                    _ => {}
                },
                FormField::Description => {
                    if form.description.value.contains('\n') {
                        match key.code {
                            KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Char(_)
                                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                form.error = Some(format!(
                                    "Multi-line notes: press Ctrl-E to edit in {}",
                                    self.editor_label
                                ));
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Backspace => form.description.backspace(),
                            KeyCode::Delete => form.description.delete(),
                            KeyCode::Left => form.description.move_left(),
                            KeyCode::Right => form.description.move_right(),
                            KeyCode::Home => form.description.move_home(),
                            KeyCode::End => form.description.move_end(),
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                form.description.insert_char(c);
                            }
                            _ => {}
                        }
                    }
                }
                FormField::Tags => match key.code {
                    KeyCode::Backspace => form.tags.backspace(),
                    KeyCode::Delete => form.tags.delete(),
                    KeyCode::Left => form.tags.move_left(),
                    KeyCode::Right => form.tags.move_right(),
                    KeyCode::Home => form.tags.move_home(),
                    KeyCode::End => form.tags.move_end(),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        form.tags.insert_char(c);
                    }
                    _ => {}
                },
                FormField::Status => match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        form.status = form.status.prev().unwrap_or(Status::Done);
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                        form.status = form.status.cycle();
                    }
                    _ => {}
                },
                FormField::Priority => match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        form.priority = match form.priority {
                            Priority::Low => Priority::High,
                            Priority::Medium => Priority::Low,
                            Priority::High => Priority::Medium,
                        };
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                        form.priority = form.priority.cycle();
                    }
                    _ => {}
                },
            },
        }

        if submit {
            if let Err(msg) = self.commit_task_form(&form) {
                form.error = Some(msg);
                self.mode = Mode::TaskModal(form);
            }
        } else if keep_open {
            self.mode = Mode::TaskModal(form);
        }
    }

    fn commit_task_form(&mut self, form: &TaskForm) -> Result<(), String> {
        let title = form.title.value.trim().to_string();
        if title.is_empty() {
            return Err("Title cannot be empty".into());
        }
        let description = if form.description.value.trim().is_empty() {
            String::new()
        } else {
            form.description
                .value
                .replace("\r\n", "\n")
                .trim_end()
                .to_string()
        };
        let tags = parse_tags(&form.tags.value);
        self.push_undo();

        let target_id = if let Some(id) = form.editing_id {
            if let Some(task) = self.store.get_mut(id) {
                task.title = title;
                task.description = description;
                task.status = form.status;
                task.priority = form.priority;
                task.tags = tags;
                task.touch();
            }
            self.set_message("Task updated");
            id
        } else {
            let device = self.sync_config.effective_device_name();
            let id = self.store.add_with_origin(
                title,
                description,
                form.status,
                form.priority,
                tags,
                &device,
            );
            self.set_message("Task added");
            id
        };

        self.persist();
        self.clamp_selections();
        self.select_task_by_id(target_id);
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        if self.pending_g {
            self.pending_g = false;
            match key.code {
                KeyCode::Char('g') => {
                    self.jump_top();
                    return;
                }
                KeyCode::Char('t') => {
                    self.view = self.view.toggle();
                    self.clamp_selections();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => {
                if self.dirty {
                    self.persist();
                }
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
            }
            KeyCode::Tab => {
                self.view = self.view.toggle();
                self.clamp_selections();
            }
            KeyCode::Char('1') => {
                self.view = View::TodoList;
                self.clamp_selections();
            }
            KeyCode::Char('2') => {
                self.view = View::Kanban;
                self.clamp_selections();
            }
            KeyCode::Char('g') => {
                self.pending_g = true;
            }
            KeyCode::Char('G') => {
                self.jump_bottom();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection_down();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection_up();
            }
            KeyCode::Char('h') | KeyCode::Left => match self.view {
                View::Kanban => {
                    self.kanban_col = self.kanban_col.saturating_sub(1);
                    self.clamp_selections();
                }
                View::TodoList => {
                    self.todo_filter = self.todo_filter.prev();
                    self.todo_selected = 0;
                    self.todo_scroll = 0;
                    self.clamp_selections();
                }
            },
            KeyCode::Char('l') | KeyCode::Right => match self.view {
                View::Kanban => {
                    if self.kanban_col < 2 {
                        self.kanban_col += 1;
                    }
                    self.clamp_selections();
                }
                View::TodoList => {
                    self.todo_filter = self.todo_filter.next();
                    self.todo_selected = 0;
                    self.todo_scroll = 0;
                    self.clamp_selections();
                }
            },
            KeyCode::Char('H') | KeyCode::Char('<') => {
                self.move_selected_task_status(false);
            }
            KeyCode::Char('L') | KeyCode::Char('>') => {
                self.move_selected_task_status(true);
            }
            KeyCode::Char('J') => {
                self.reorder_selected_task(true);
            }
            KeyCode::Char('K') => {
                self.reorder_selected_task(false);
            }
            KeyCode::Char('a') | KeyCode::Char('o') => {
                self.open_add_modal();
            }
            KeyCode::Char('e') | KeyCode::Char('i') | KeyCode::Enter => {
                self.open_edit_modal();
            }
            KeyCode::Char('n') => {
                self.open_notes_in_editor();
            }
            KeyCode::Char(' ') => {
                self.toggle_or_advance_selected();
            }
            KeyCode::Char('s') => {
                self.cycle_selected_status();
            }
            KeyCode::Char('S') => {
                self.request_sync();
            }
            KeyCode::Char('p') => {
                self.cycle_selected_priority();
            }
            KeyCode::Char('t') => {
                self.open_tag_filter_modal();
            }
            KeyCode::Char('T') => {
                self.open_edit_tags_modal();
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.delete_selected_task();
            }
            KeyCode::Char('u') => {
                self.undo();
            }
            KeyCode::Char('f') if self.view == View::TodoList => {
                self.todo_filter = self.todo_filter.next();
                self.todo_selected = 0;
                self.todo_scroll = 0;
                self.clamp_selections();
            }
            KeyCode::Char('/') => {
                self.search_input = TextInput::new(self.search_query.clone());
                self.mode = Mode::Search;
            }
            KeyCode::Esc if !self.search_query.is_empty() => {
                self.search_query.clear();
                self.search_input = TextInput::new("");
                self.clamp_selections();
                self.set_message("Search cleared");
            }
            KeyCode::Esc if !self.tag_filters.is_empty() => {
                self.clear_tag_filters();
            }
            _ => {}
        }
    }

    pub fn open_tag_filter_modal(&mut self) {
        if self.all_tags().is_empty() {
            self.set_message("No tags yet — press T to tag a task");
            return;
        }
        self.mode = Mode::TagFilter(0);
    }

    pub fn toggle_tag_filter(&mut self, tag: String) {
        if let Some(pos) = self.tag_filters.iter().position(|t| t == &tag) {
            self.tag_filters.remove(pos);
        } else {
            self.tag_filters.push(tag);
        }
        self.reset_scroll_and_clamp_tags();
    }

    pub fn clear_tag_filters(&mut self) {
        self.tag_filters.clear();
        self.reset_scroll_and_clamp_tags();
    }

    pub fn toggle_tag_match_mode(&mut self) {
        self.tag_match_mode = self.tag_match_mode.toggle();
        self.reset_scroll_and_clamp_tags();
    }

    fn reset_scroll_and_clamp_tags(&mut self) {
        self.todo_selected = 0;
        self.todo_scroll = 0;
        self.kanban_selected = [0, 0, 0];
        self.kanban_scroll = [0, 0, 0];
        self.clamp_selections();
        if self.tag_filters.is_empty() {
            self.set_message("Showing all tags");
        } else {
            let joined = self
                .tag_filters
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(if self.tag_match_mode == TagMatchMode::All {
                    " & "
                } else {
                    " | "
                });
            self.set_message(format!("Tags: {joined}"));
        }
    }

    pub fn open_add_modal(&mut self) {
        let default_status = match self.view {
            View::Kanban => self.current_kanban_status(),
            View::TodoList => match self.todo_filter {
                TodoFilter::All | TodoFilter::Todo => Status::Todo,
                TodoFilter::InProgress => Status::InProgress,
                TodoFilter::Done => Status::Done,
            },
        };
        let default_tags = format_tags(&self.tag_filters);
        let default_tag = if default_tags.is_empty() {
            None
        } else {
            Some(default_tags.as_str())
        };
        self.mode = Mode::TaskModal(TaskForm::new_task(default_status, default_tag));
    }

    pub fn open_edit_modal(&mut self) {
        if let Some(id) = self.selected_task_id() {
            if let Some(task) = self.store.get(id).cloned() {
                self.mode = Mode::TaskModal(TaskForm::edit_task(&task));
            }
        }
    }

    pub fn open_edit_tags_modal(&mut self) {
        if let Some(id) = self.selected_task_id() {
            if let Some(task) = self.store.get(id).cloned() {
                let mut form = TaskForm::edit_task(&task);
                form.active_field = FormField::Tags;
                self.mode = Mode::TaskModal(form);
            }
        }
    }

    fn jump_top(&mut self) {
        match self.view {
            View::TodoList => {
                self.todo_selected = 0;
                self.todo_scroll = 0;
            }
            View::Kanban => {
                self.kanban_selected[self.kanban_col] = 0;
                self.kanban_scroll[self.kanban_col] = 0;
            }
        }
    }

    fn jump_bottom(&mut self) {
        match self.view {
            View::TodoList => {
                let len = self.filtered_todo_ids().len();
                if len > 0 {
                    self.todo_selected = len - 1;
                }
            }
            View::Kanban => {
                let len = self.column_task_ids(self.current_kanban_status()).len();
                if len > 0 {
                    self.kanban_selected[self.kanban_col] = len - 1;
                }
            }
        }
    }

    fn move_selection_down(&mut self) {
        match self.view {
            View::TodoList => {
                let len = self.filtered_todo_ids().len();
                if len > 0 && self.todo_selected + 1 < len {
                    self.todo_selected += 1;
                }
            }
            View::Kanban => {
                let len = self.column_task_ids(self.current_kanban_status()).len();
                let sel = &mut self.kanban_selected[self.kanban_col];
                if len > 0 && *sel + 1 < len {
                    *sel += 1;
                }
            }
        }
    }

    fn move_selection_up(&mut self) {
        match self.view {
            View::TodoList => {
                self.todo_selected = self.todo_selected.saturating_sub(1);
            }
            View::Kanban => {
                let sel = &mut self.kanban_selected[self.kanban_col];
                *sel = sel.saturating_sub(1);
            }
        }
    }

    pub fn toggle_task_done(&mut self, task_id: u64) {
        let Some(current_status) = self.store.get(task_id).map(|t| t.status) else {
            return;
        };
        let next_status = if current_status == Status::Done {
            Status::Todo
        } else {
            Status::Done
        };
        self.push_undo();
        if let Some(task) = self.store.get_mut(task_id) {
            task.status = next_status;
            task.touch();
        }
        self.persist();
        self.clamp_selections();
        if self.view == View::TodoList && self.todo_filter == TodoFilter::All {
            self.select_task_by_id(task_id);
        }
        self.set_message(format!("Marked as {}", next_status.label()));
    }

    fn toggle_or_advance_selected(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        match self.view {
            View::TodoList => self.toggle_task_done(id),
            View::Kanban => self.cycle_selected_status(),
        }
    }

    fn cycle_selected_status(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        let Some(cur) = self.store.get(id).map(|t| t.status) else {
            return;
        };
        let next = cur.cycle();
        self.move_task_to_status(id, next, true);
    }

    fn move_selected_task_status(&mut self, forward: bool) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        let Some(cur) = self.store.get(id).map(|t| t.status) else {
            return;
        };
        let target = if forward { cur.next() } else { cur.prev() };
        if let Some(next_status) = target {
            self.move_task_to_status(id, next_status, true);
        }
    }

    pub fn move_task_to_status(&mut self, task_id: u64, target: Status, follow: bool) {
        let Some(cur) = self.store.get(task_id).map(|t| t.status) else {
            return;
        };
        if cur == target {
            return;
        }
        self.push_undo();
        if let Some(task) = self.store.get_mut(task_id) {
            task.status = target;
            task.touch();
        }
        self.persist();
        self.clamp_selections();
        if follow {
            self.select_task_by_id(task_id);
        }
        self.set_message(format!("Moved to {}", target.label()));
    }

    fn reorder_selected_task(&mut self, down: bool) {
        let visible = match self.view {
            View::TodoList => self.filtered_todo_ids(),
            View::Kanban => self.column_task_ids(self.current_kanban_status()),
        };
        let sel_idx = match self.view {
            View::TodoList => self.todo_selected,
            View::Kanban => self.kanban_selected[self.kanban_col],
        };
        if visible.len() < 2 || sel_idx >= visible.len() {
            return;
        }
        let neighbor_idx = if down {
            if sel_idx + 1 >= visible.len() {
                return;
            }
            sel_idx + 1
        } else {
            if sel_idx == 0 {
                return;
            }
            sel_idx - 1
        };

        let id_a = visible[sel_idx];
        let id_b = visible[neighbor_idx];
        let Some(pos_a) = self.store.tasks.iter().position(|t| t.id == id_a) else {
            return;
        };
        let Some(pos_b) = self.store.tasks.iter().position(|t| t.id == id_b) else {
            return;
        };

        self.push_undo();
        self.store.tasks.swap(pos_a, pos_b);
        self.persist();

        match self.view {
            View::TodoList => self.todo_selected = neighbor_idx,
            View::Kanban => self.kanban_selected[self.kanban_col] = neighbor_idx,
        }
    }

    fn cycle_selected_priority(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        self.push_undo();
        let mut new_prio = Priority::Medium;
        if let Some(task) = self.store.get_mut(id) {
            task.priority = task.priority.cycle();
            task.touch();
            new_prio = task.priority;
        }
        self.persist();
        self.set_message(format!("Priority: {}", new_prio.label()));
    }

    fn delete_selected_task(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        self.push_undo();
        if let Some(removed) = self.store.remove(id) {
            self.persist();
            self.clamp_selections();
            self.set_message(format!("Deleted \"{}\" (press u to undo)", removed.title));
        }
    }

    fn reconcile_restored_store(current: &TaskStore, mut restored: TaskStore) -> TaskStore {
        let now = now_millis();
        let current_by_uid: std::collections::HashMap<&str, &Task> = current
            .tasks
            .iter()
            .filter(|t| !t.uid.is_empty())
            .map(|t| (t.uid.as_str(), t))
            .collect();

        // Keep tombstones from current for tasks not resurrected in `restored`
        for (uid, &ts) in &current.tombstones {
            restored
                .tombstones
                .entry(uid.clone())
                .and_modify(|cur| {
                    if ts > *cur {
                        *cur = ts;
                    }
                })
                .or_insert(ts);
        }

        let mut restored_uids = std::collections::HashSet::new();
        for task in &mut restored.tasks {
            if !task.uid.is_empty() {
                restored_uids.insert(task.uid.clone());
            }
            let tomb_ts = restored.tombstones.remove(&task.uid).unwrap_or(0);
            match current_by_uid.get(task.uid.as_str()) {
                None => {
                    // Resurrected task (e.g. undoing a delete): beat any tombstone
                    task.updated_at = now
                        .max(task.updated_at.saturating_add(1))
                        .max(tomb_ts.saturating_add(1));
                }
                Some(cur) => {
                    if **cur != *task {
                        task.updated_at = now
                            .max(cur.updated_at.saturating_add(1))
                            .max(task.updated_at.saturating_add(1))
                            .max(tomb_ts.saturating_add(1));
                    }
                }
            }
        }

        // Tasks present in `current` but absent in `restored` (e.g. undoing an add) need tombstones
        for cur in &current.tasks {
            if !cur.uid.is_empty() && !restored_uids.contains(&cur.uid) {
                let del_ts = now.max(cur.updated_at.saturating_add(1));
                restored
                    .tombstones
                    .entry(cur.uid.clone())
                    .and_modify(|ts| {
                        if del_ts > *ts {
                            *ts = del_ts;
                        }
                    })
                    .or_insert(del_ts);
            }
        }

        restored.next_id = restored.next_id.max(current.next_id);
        restored
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.store = Self::reconcile_restored_store(&self.store, prev);
            self.persist();
            self.clamp_selections();
            self.set_message("Undid last change");
        } else {
            self.set_message("Nothing to undo");
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let x = mouse.column;
        let y = mouse.row;

        match self.mode {
            Mode::Help => {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    if let Some(area) = self.regions.help_modal {
                        if !rect_contains(area, x, y) {
                            self.mode = Mode::Normal;
                        }
                    } else {
                        self.mode = Mode::Normal;
                    }
                }
                return;
            }
            Mode::TaskModal(_) => {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.handle_modal_click(x, y);
                }
                return;
            }
            Mode::TagFilter(sel) => {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.handle_tag_modal_click(x, y);
                    }
                    MouseEventKind::ScrollDown => {
                        let len = self.all_tags().len();
                        if len > 0 && sel + 1 < len {
                            self.mode = Mode::TagFilter(sel + 1);
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        self.mode = Mode::TagFilter(sel.saturating_sub(1));
                    }
                    _ => {}
                }
                return;
            }
            Mode::Search => {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.search_query = self.search_input.value.clone();
                    self.mode = Mode::Normal;
                } else {
                    return;
                }
            }
            Mode::Normal => {}
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_left_down(x, y),
            MouseEventKind::Drag(MouseButton::Left) => self.handle_left_drag(x, y),
            MouseEventKind::Up(MouseButton::Left) => self.handle_left_up(x, y),
            MouseEventKind::ScrollDown => self.handle_scroll(x, y, true),
            MouseEventKind::ScrollUp => self.handle_scroll(x, y, false),
            _ => {}
        }
    }

    fn handle_tag_modal_click(&mut self, x: u16, y: u16) {
        let Some(regions) = self.regions.tag_modal.clone() else {
            self.mode = Mode::Normal;
            return;
        };
        if !rect_contains(regions.area, x, y) {
            self.mode = Mode::Normal;
            return;
        }
        if rect_contains(regions.mode_btn, x, y) {
            self.toggle_tag_match_mode();
            return;
        }
        if rect_contains(regions.clear_btn, x, y) {
            self.clear_tag_filters();
            return;
        }
        for (idx, tag, rect) in regions.rows {
            if rect_contains(rect, x, y) {
                self.toggle_tag_filter(tag);
                self.mode = Mode::TagFilter(idx);
                return;
            }
        }
    }

    fn handle_modal_click(&mut self, x: u16, y: u16) {
        let Some(modal_regions) = self.regions.modal.clone() else {
            return;
        };
        if !rect_contains(modal_regions.area, x, y) {
            self.mode = Mode::Normal;
            return;
        }

        let Mode::TaskModal(mut form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };

        if rect_contains(modal_regions.cancel_btn, x, y) {
            return;
        }
        if rect_contains(modal_regions.save_btn, x, y) {
            if let Err(msg) = self.commit_task_form(&form) {
                form.error = Some(msg);
                self.mode = Mode::TaskModal(form);
            }
            return;
        }
        if rect_contains(modal_regions.title, x, y) {
            form.active_field = FormField::Title;
        } else if rect_contains(modal_regions.description, x, y) {
            form.active_field = FormField::Description;
        } else if rect_contains(modal_regions.tags, x, y) {
            form.active_field = FormField::Tags;
        } else if rect_contains(modal_regions.status, x, y) {
            form.active_field = FormField::Status;
            form.status = form.status.cycle();
        } else if rect_contains(modal_regions.priority, x, y) {
            form.active_field = FormField::Priority;
            form.priority = form.priority.cycle();
        }

        self.mode = Mode::TaskModal(form);
    }

    fn is_double_click(&mut self, task_id: u64) -> bool {
        let now = Instant::now();
        let is_double = self
            .last_click
            .as_ref()
            .is_some_and(|c| c.task_id == task_id && now.duration_since(c.at) < Duration::from_millis(400));
        if is_double {
            self.last_click = None;
        } else {
            self.last_click = Some(LastClick { task_id, at: now });
        }
        is_double
    }

    fn handle_left_down(&mut self, x: u16, y: u16) {
        self.pending_g = false;

        if rect_contains(self.regions.tab_todos, x, y) {
            self.view = View::TodoList;
            self.clamp_selections();
            return;
        }
        if rect_contains(self.regions.tab_kanban, x, y) {
            self.view = View::Kanban;
            self.clamp_selections();
            return;
        }
        if rect_contains(self.regions.sync_btn, x, y) {
            self.request_sync();
            return;
        }
        if rect_contains(self.regions.add_btn, x, y) {
            self.open_add_modal();
            return;
        }
        if rect_contains(self.regions.help_btn, x, y) {
            self.mode = Mode::Help;
            return;
        }

        if rect_contains(self.regions.tag_mode_btn, x, y) {
            self.toggle_tag_match_mode();
            return;
        }
        if rect_contains(self.regions.tag_overflow_btn, x, y) {
            self.open_tag_filter_modal();
            return;
        }

        let tag_pills = self.regions.tag_pills.clone();
        for (tag, rect) in tag_pills {
            if rect_contains(rect, x, y) {
                match tag {
                    Some(t) => self.toggle_tag_filter(t),
                    None => self.clear_tag_filters(),
                }
                return;
            }
        }

        match self.view {
            View::TodoList => {
                for &(filter, rect) in &self.regions.filter_pills {
                    if rect_contains(rect, x, y) {
                        self.todo_filter = filter;
                        self.todo_selected = 0;
                        self.todo_scroll = 0;
                        self.clamp_selections();
                        return;
                    }
                }

                let rows = self.regions.todo_rows.clone();
                for (task_id, row_rect, checkbox_rect) in rows {
                    if rect_contains(row_rect, x, y) {
                        self.select_task_by_id(task_id);
                        if rect_contains(checkbox_rect, x, y) {
                            self.last_click = None;
                            self.toggle_task_done(task_id);
                        } else if self.is_double_click(task_id) {
                            self.open_edit_modal();
                        }
                        return;
                    }
                }
            }
            View::Kanban => {
                let cards = self.regions.kanban_cards.clone();
                for (task_id, status, card_rect) in cards {
                    if rect_contains(card_rect, x, y) {
                        self.select_task_by_id(task_id);
                        self.drag = Some(DragState {
                            task_id,
                            origin_status: status,
                            start_x: x,
                            start_y: y,
                            current_x: x,
                            current_y: y,
                            active: false,
                            hover_status: Some(status),
                        });
                        return;
                    }
                }

                for &(status, col_rect) in &self.regions.kanban_columns {
                    if rect_contains(col_rect, x, y) {
                        self.kanban_col = status.index();
                        self.clamp_selections();
                        return;
                    }
                }
            }
        }
    }

    fn column_at(&self, x: u16, y: u16) -> Option<Status> {
        self.regions
            .kanban_columns
            .iter()
            .find(|(_, rect)| rect_contains(*rect, x, y))
            .map(|(status, _)| *status)
    }

    fn handle_left_drag(&mut self, x: u16, y: u16) {
        let hover = self.column_at(x, y);
        if let Some(drag) = self.drag.as_mut() {
            drag.current_x = x;
            drag.current_y = y;
            let dist = drag.start_x.abs_diff(x) + drag.start_y.abs_diff(y);
            if dist >= 2 || hover.is_some_and(|s| s != drag.origin_status) {
                drag.active = true;
            }
            drag.hover_status = hover;
        }
    }

    fn handle_left_up(&mut self, x: u16, y: u16) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.active {
            if let Some(target) = self.column_at(x, y) {
                if target != drag.origin_status {
                    self.move_task_to_status(drag.task_id, target, true);
                }
            }
            self.last_click = None;
        } else if self.is_double_click(drag.task_id) {
            self.select_task_by_id(drag.task_id);
            self.open_edit_modal();
        }
    }

    fn handle_scroll(&mut self, x: u16, y: u16, down: bool) {
        match self.view {
            View::TodoList => {
                if rect_contains(self.regions.todo_list_area, x, y) {
                    if down {
                        self.move_selection_down();
                    } else {
                        self.move_selection_up();
                    }
                }
            }
            View::Kanban => {
                if let Some(status) = self.column_at(x, y) {
                    self.kanban_col = status.index();
                    if down {
                        self.move_selection_down();
                    } else {
                        self.move_selection_up();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_app() -> App {
        let mut store = TaskStore::new_empty();
        store.add(
            "Alpha".into(),
            "First task".into(),
            Status::Todo,
            Priority::High,
            vec!["work".into()],
        );
        store.add(
            "Beta".into(),
            "Second task".into(),
            Status::Todo,
            Priority::Medium,
            vec!["personal".into()],
        );
        store.add(
            "Gamma".into(),
            "Third task".into(),
            Status::InProgress,
            Priority::Low,
            vec!["work".into()],
        );
        store.add(
            "Delta".into(),
            "Fourth task".into(),
            Status::Done,
            Priority::Medium,
            Vec::new(),
        );
        App::new(store, None)
    }

    #[test]
    fn multi_tag_filtering_picker_and_mouse_pills() {
        let mut app = sample_app();
        // Add a second tag ("urgent") onto task 1 ("Alpha", already "work")
        app.handle_key(key(KeyCode::Char('T')));
        for ch in ", urgent".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.store.get(1).unwrap().tags, vec!["work", "urgent"]);

        // Open tag filter picker with 't' (sorted tags: personal, urgent, work)
        app.handle_key(key(KeyCode::Char('t')));
        assert!(matches!(app.mode, Mode::TagFilter(0)));

        // Select #urgent (index 1) and #work (index 2)
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));

        // Default match mode is AND: only task 1 has both #urgent and #work
        assert_eq!(app.tag_filters, vec!["urgent", "work"]);
        assert_eq!(app.tag_match_mode, TagMatchMode::All);
        assert_eq!(app.filtered_todo_ids(), vec![1]);

        // Adding a task while filtered pre-fills both active tags
        app.handle_key(key(KeyCode::Char('a')));
        for ch in "Ship patch".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.filtered_todo_ids(), vec![1, 5]);
        assert_eq!(app.store.get(5).unwrap().tags, vec!["urgent", "work"]);

        // Render and click the `and` -> `or` mode toggle button
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let mode_btn = app.regions.tag_mode_btn;
        assert!(mode_btn.width > 0);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: mode_btn.x,
            row: mode_btn.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tag_match_mode, TagMatchMode::Any);
        // OR now includes task 3 ("Gamma", #work) as well as 1 and 5
        assert_eq!(app.filtered_todo_ids(), vec![1, 3, 5]);

        // Clicking an active #tag pill removes just that tag
        let (_, urgent_rect) = app
            .regions
            .tag_pills
            .iter()
            .find(|(t, _)| t.as_deref() == Some("urgent"))
            .cloned()
            .expect("should render #urgent tag pill");
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: urgent_rect.x + 1,
            row: urgent_rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tag_filters, vec!["work"]);

        // Esc clears remaining tag filters
        app.handle_key(key(KeyCode::Esc));
        assert!(app.tag_filters.is_empty());
    }

    #[test]
    fn tag_filter_modal_scrolls_when_many_tags() {
        let mut store = TaskStore::new_empty();
        let tags: Vec<String> = (1..=20).map(|i| format!("tag{i:02}")).collect();
        store.add("Many tags".into(), String::new(), Status::Todo, Priority::Medium, tags);
        let mut app = App::new(store, None);

        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char('G'))); // Jump to last tag (#tag20, index 19)
        assert!(matches!(app.mode, Mode::TagFilter(19)));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let modal = app.regions.tag_modal.clone().expect("tag modal rendered");
        let last_row = modal
            .rows
            .iter()
            .find(|(_, t, _)| t == "tag20")
            .cloned()
            .expect("scrolled modal must render #tag20");
        assert_eq!(last_row.0, 19);

        // Clicking #tag20 toggles it and preserves real index 19
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: last_row.2.x + 1,
            row: last_row.2.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tag_filters, vec!["tag20"]);
        assert!(matches!(app.mode, Mode::TagFilter(19)));
    }

    #[test]
    fn narrow_terminal_wraps_tags_without_cutoff() {
        let mut store = TaskStore::new_empty();
        store.add(
            "Task A".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["work".into(), "backend".into(), "urgent".into()],
        );
        store.add(
            "Task B".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            vec!["personal".into(), "errands".into()],
        );
        let mut app = App::new(store, None);

        // Narrow 52-column terminal where status filters + 5 tags don't fit on one line
        let backend = TestBackend::new(52, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // All 5 tags should still be rendered as clickable pills via wrapping
        let rendered_tags: Vec<&str> = app
            .regions
            .tag_pills
            .iter()
            .filter_map(|(t, _)| t.as_deref())
            .collect();
        assert_eq!(
            rendered_tags,
            vec!["backend", "errands", "personal", "urgent", "work"]
        );
    }

    #[test]
    fn dynamic_device_tag_injection_defaults_off_and_works_when_enabled() {
        let mut store = TaskStore::new_empty();
        store.add_with_origin(
            "Local task".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "macbook",
        );
        store.add_with_origin(
            "Remote task".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["ops".into()],
            "vps",
        );

        // Default: inject_device_tag is false
        let cfg_off: AppConfig = toml::from_str(
            r#"
[sync]
device_name = "macbook"
"#,
        )
        .unwrap();
        let app_off = App::with_config(store.clone(), None, cfg_off);
        assert_eq!(app_off.all_tags(), vec!["ops"]);

        // Enabled: remote task dynamically receives #vps, local task does not get #macbook
        let cfg_on: AppConfig = toml::from_str(
            r#"
[sync]
device_name = "macbook"
inject_device_tag = true
"#,
        )
        .unwrap();
        let mut app_on = App::with_config(store, None, cfg_on);
        assert_eq!(app_on.all_tags(), vec!["ops", "vps"]);
        assert_eq!(app_on.store.get(2).unwrap().tags, vec!["ops"]); // stored tags untouched!

        app_on.toggle_tag_filter("vps".into());
        assert_eq!(app_on.filtered_todo_ids(), vec![2]);
    }

    #[test]
    fn vim_navigation_and_gg_g() {
        let mut app = sample_app();
        assert_eq!(app.selected_task_id(), Some(1));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_task_id(), Some(2));

        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.selected_task_id(), Some(4));

        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.selected_task_id(), Some(1));
    }

    #[test]
    fn kanban_column_navigation_and_card_moves() {
        let mut app = sample_app();
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.view, View::Kanban);
        assert_eq!(app.current_kanban_status(), Status::Todo);
        assert_eq!(app.selected_task_id(), Some(1));

        // Move task 1 from Todo -> InProgress with Shift+L
        app.handle_key(key(KeyCode::Char('L')));
        assert_eq!(app.store.get(1).unwrap().status, Status::InProgress);
        assert_eq!(app.current_kanban_status(), Status::InProgress);
        assert_eq!(app.selected_task_id(), Some(1));

        // Move task 1 back to Todo with Shift+H
        app.handle_key(key(KeyCode::Char('H')));
        assert_eq!(app.store.get(1).unwrap().status, Status::Todo);
        assert_eq!(app.current_kanban_status(), Status::Todo);
    }

    #[test]
    fn reorder_delete_and_undo() {
        let mut app = sample_app();
        assert_eq!(app.filtered_todo_ids(), vec![1, 2, 3, 4]);

        // Reorder task 1 down
        app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(app.filtered_todo_ids(), vec![2, 1, 3, 4]);
        assert_eq!(app.selected_task_id(), Some(1));

        // Delete task 1
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.filtered_todo_ids(), vec![2, 3, 4]);

        // Undo delete
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.filtered_todo_ids(), vec![2, 1, 3, 4]);
    }

    #[test]
    fn undo_bumps_lww_timestamps_and_survives_sync_merge() {
        let mut app = sample_app();
        let uid1 = app.store.get(1).unwrap().uid.clone();

        // Delete task 1, simulate remote receiving that tombstone
        app.handle_key(key(KeyCode::Char('d')));
        let remote_after_delete = app.store.clone();
        assert!(remote_after_delete.tombstones.contains_key(&uid1));

        // Undo delete locally, then merge with remote that still has the tombstone
        app.handle_key(key(KeyCode::Char('u')));
        let merged = merge_stores(&app.store, &remote_after_delete, "local", "peer");
        assert!(
            merged.tasks.iter().any(|t| t.uid == uid1),
            "resurrected task must beat prior tombstone on sync"
        );

        // Edit task 1 (toggle done), simulate remote receiving the edit
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.store.get(1).unwrap().status, Status::Done);
        let remote_after_edit = app.store.clone();

        // Undo edit locally, then merge with remote
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.store.get(1).unwrap().status, Status::Todo);
        let merged_edit = merge_stores(&app.store, &remote_after_edit, "local", "peer");
        assert_eq!(
            merged_edit
                .tasks
                .iter()
                .find(|t| t.uid == uid1)
                .unwrap()
                .status,
            Status::Todo
        );

        // Add a task, simulate remote receiving it, then undo add
        app.handle_key(key(KeyCode::Char('a')));
        for ch in "Temp task".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        let added_uid = app
            .store
            .tasks
            .iter()
            .find(|t| t.title == "Temp task")
            .unwrap()
            .uid
            .clone();
        let remote_with_added = app.store.clone();

        app.handle_key(key(KeyCode::Char('u')));
        let merged_add = merge_stores(&app.store, &remote_with_added, "local", "peer");
        assert!(
            !merged_add.tasks.iter().any(|t| t.uid == added_uid),
            "undone task addition must record tombstone so sync does not resurrect it"
        );
    }

    #[test]
    fn add_task_modal_and_search() {
        let mut app = sample_app();
        app.handle_key(key(KeyCode::Char('a')));
        for ch in "Buy milk".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.store.tasks.len(), 5);

        app.handle_key(key(KeyCode::Char('/')));
        for ch in "milk".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.filtered_todo_ids().len(), 1);
    }

    #[test]
    fn mouse_click_tabs_checkbox_and_kanban_drag_drop() {
        let mut app = sample_app();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Render Todo List view to populate hitboxes
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Click checkbox of first task
        let (_, _, cb_rect) = app.regions.todo_rows[0];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: cb_rect.x,
            row: cb_rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.store.get(1).unwrap().status, Status::Done);

        // Click Kanban tab
        let tab = app.regions.tab_kanban;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tab.x + 1,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.view, View::Kanban);

        // Render Kanban view to populate card & column hitboxes
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let (card_id, origin, card_rect) = app
            .regions
            .kanban_cards
            .iter()
            .copied()
            .find(|(_, s, _)| *s == Status::Todo)
            .expect("should have a Todo card");
        let (_, done_col_rect) = app
            .regions
            .kanban_columns
            .iter()
            .copied()
            .find(|(s, _)| *s == Status::Done)
            .unwrap();

        assert_eq!(origin, Status::Todo);

        // Drag card from Todo column to Done column
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: card_rect.x + 1,
            row: card_rect.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: done_col_rect.x + 2,
            row: done_col_rect.y + 3,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: done_col_rect.x + 2,
            row: done_col_rect.y + 3,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.store.get(card_id).unwrap().status, Status::Done);
    }

    #[test]
    fn render_small_terminal_without_panic() {
        let mut app = sample_app();
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.view = View::Kanban;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.open_add_modal();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.mode = Mode::Help;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn live_reload_and_merge_on_save_preserve_external_writes() {
        use crate::ops::{self, AddTaskInput};
        let dir = std::env::temp_dir().join(format!("roke_reload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("todos.jsonl");

        let mut initial = TaskStore::new_empty();
        initial.add_with_origin(
            "Existing".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "local",
        );
        initial.save(&path).unwrap();

        let cfg = AppConfig::default();
        let mut app = App::with_config(initial, Some(path.clone()), cfg.clone());
        assert!(!app.reload_if_changed());

        // External agent adds a task via ops
        let ext1 = ops::add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Added by agent".into(),
                priority: Priority::High,
                tags: vec!["agent".into()],
                ..AddTaskInput::default()
            },
        )
        .unwrap();

        assert!(app.reload_if_changed());
        assert_eq!(app.store.tasks.len(), 2);
        assert!(app.store.tasks.iter().any(|t| t.uid == ext1.uid));

        // Another external write happens right before a TUI edit (before reload tick)
        let ext2 = ops::add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Concurrent agent task".into(),
                ..AddTaskInput::default()
            },
        )
        .unwrap();

        // User toggles task 1 done in TUI; persist() must merge instead of clobbering ext2
        app.toggle_task_done(1);
        let on_disk = TaskStore::load_or_empty(&path).unwrap();
        assert_eq!(on_disk.tasks.len(), 3);
        assert!(on_disk.tasks.iter().any(|t| t.uid == ext2.uid));
        assert_eq!(on_disk.get(1).unwrap().status, Status::Done);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_editor_shortcuts_and_multiline_markdown_render() {
        let mut app = sample_app();

        // Press 'n' in Normal mode to request opening selected task notes in $EDITOR
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.pending_editor, Some(PendingEditor::Task(1)));
        let target = app.pending_editor.take().unwrap();
        let (initial, title) = app.editor_initial_content(target).unwrap();
        assert_eq!(initial, "First task");
        assert_eq!(title, "Alpha");

        app.apply_editor_result(
            target,
            Ok(Some("# Alpha Notes\n- [ ] Step 1\n- [x] Step 2".into())),
        );
        assert_eq!(
            app.store.get(1).unwrap().description,
            "# Alpha Notes\n- [ ] Step 1\n- [x] Step 2"
        );

        // Press 'e' then Ctrl-E inside modal to open modal notes in $EDITOR
        app.handle_key(key(KeyCode::Char('e')));
        assert!(matches!(app.mode, Mode::TaskModal(_)));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.pending_editor, Some(PendingEditor::Modal));
        let modal_target = app.pending_editor.take().unwrap();
        app.apply_editor_result(modal_target, Ok(Some("# Updated\nMore lines".into())));

        // Render modal + both views with multi-line Markdown without panic
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Focusing multi-line notes and pressing Backspace/Char must not corrupt the hidden note
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('x')));
        if let Mode::TaskModal(form) = &app.mode {
            assert_eq!(form.description.value, "# Updated\nMore lines");
            assert!(form.error.is_some());
        } else {
            panic!("expected modal to remain open");
        }

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.store.get(1).unwrap().description, "# Updated\nMore lines");
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.handle_key(key(KeyCode::Char('2')));
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }
}
