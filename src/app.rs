use crate::config::{normalize_device_tag, AppConfig, SyncConfig};
use crate::editor::{editor_display_name, TaskDocument};
use crate::model::{
    append_tasks_to_archive, archive_path_for, file_stamp, format_tags, matches_search_query,
    now_millis, parse_due_spec, parse_quick_add, parse_repeat, parse_tags, sort_task_refs,
    today_ymd, DueState, FileLock, FileStamp, Priority, SortOrder, Status, Task, TaskStore,
};
use crate::sync::{
    absorb_syncthing_conflicts, merge_stores, sync_state_dir_for, SyncBatchOutcome, SyncJob,
    SyncOutcome, SyncSlot,
};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardScope {
    Project,
    All,
    Global,
}

impl BoardScope {
    pub fn cycle(self) -> Self {
        match self {
            BoardScope::Project => BoardScope::All,
            BoardScope::All => BoardScope::Global,
            BoardScope::Global => BoardScope::Project,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSource {
    Project,
    Global,
}

/// Holds cleanly separated project and global stores + sync configs when `roke` is opened
/// inside a repository with `.roke.jsonl` or `.roke.toml`, enabling live `b` switching
/// between `project`, `all` (merged view), and `global` without cross-contaminating files or sync peers.
pub struct DualBoard {
    pub project_name: String,
    pub scope: BoardScope,
    pub project_path: PathBuf,
    pub global_path: PathBuf,
    pub project_store: TaskStore,
    pub global_store: TaskStore,
    pub project_stamp: Option<FileStamp>,
    pub global_stamp: Option<FileStamp>,
    pub project_sync_config: SyncConfig,
    pub global_sync_config: SyncConfig,
    pub uid_owner: HashMap<String, BoardSource>,
}

impl DualBoard {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_name: String,
        scope: BoardScope,
        project_path: PathBuf,
        global_path: PathBuf,
        project_store: TaskStore,
        global_store: TaskStore,
        project_sync_config: SyncConfig,
        global_sync_config: SyncConfig,
    ) -> Self {
        let project_stamp = file_stamp(&project_path);
        let global_stamp = file_stamp(&global_path);
        let mut uid_owner = HashMap::new();
        for t in &project_store.tasks {
            uid_owner.insert(t.uid.clone(), BoardSource::Project);
        }
        for uid in project_store.tombstones.keys() {
            uid_owner.insert(uid.clone(), BoardSource::Project);
        }
        for t in &global_store.tasks {
            uid_owner.insert(t.uid.clone(), BoardSource::Global);
        }
        for uid in global_store.tombstones.keys() {
            uid_owner.entry(uid.clone()).or_insert(BoardSource::Global);
        }
        Self {
            project_name,
            scope,
            project_path,
            global_path,
            project_store,
            global_store,
            project_stamp,
            global_stamp,
            project_sync_config,
            global_sync_config,
            uid_owner,
        }
    }

    pub fn scope_label(&self) -> String {
        match self.scope {
            BoardScope::Project => self.project_name.clone(),
            BoardScope::All => format!("{}+global", self.project_name),
            BoardScope::Global => "global".to_string(),
        }
    }

    pub fn compose_view(&mut self) -> TaskStore {
        for t in &self.project_store.tasks {
            self.uid_owner.insert(t.uid.clone(), BoardSource::Project);
        }
        for t in &self.global_store.tasks {
            self.uid_owner.insert(t.uid.clone(), BoardSource::Global);
        }
        match self.scope {
            BoardScope::Project => self.project_store.clone(),
            BoardScope::Global => self.global_store.clone(),
            BoardScope::All => {
                let mut tasks = Vec::with_capacity(
                    self.project_store.tasks.len() + self.global_store.tasks.len(),
                );
                let mut used_ids = std::collections::HashSet::new();
                let mut next_id = self
                    .project_store
                    .next_id
                    .max(self.global_store.next_id)
                    .max(1);

                for t in &self.project_store.tasks {
                    let mut task = t.clone();
                    if task.id == 0 || !used_ids.insert(task.id) {
                        task.id = next_id;
                        next_id += 1;
                        used_ids.insert(task.id);
                    } else if task.id >= next_id {
                        next_id = task.id + 1;
                    }
                    tasks.push(task);
                }
                for t in &self.global_store.tasks {
                    let mut task = t.clone();
                    if task.id == 0 || !used_ids.insert(task.id) {
                        task.id = next_id;
                        next_id += 1;
                        used_ids.insert(task.id);
                    } else if task.id >= next_id {
                        next_id = task.id + 1;
                    }
                    tasks.push(task);
                }

                let mut tombstones = self.project_store.tombstones.clone();
                for (uid, &ts) in &self.global_store.tombstones {
                    tombstones
                        .entry(uid.clone())
                        .and_modify(|cur| *cur = (*cur).max(ts))
                        .or_insert(ts);
                }

                TaskStore {
                    next_id,
                    tasks,
                    tombstones,
                }
            }
        }
    }

    /// Split a mutated `All`-scope unified store back into isolated project and global stores,
    /// preserving each store's original on-disk numeric task IDs.
    fn split_all_view(&mut self, view: &TaskStore) -> (TaskStore, TaskStore) {
        let proj_ids: HashMap<&str, u64> = self
            .project_store
            .tasks
            .iter()
            .map(|t| (t.uid.as_str(), t.id))
            .collect();
        let glob_ids: HashMap<&str, u64> = self
            .global_store
            .tasks
            .iter()
            .map(|t| (t.uid.as_str(), t.id))
            .collect();

        let mut next_proj = TaskStore {
            next_id: self.project_store.next_id.max(1),
            tasks: Vec::new(),
            tombstones: HashMap::new(),
        };
        let mut next_glob = TaskStore {
            next_id: self.global_store.next_id.max(1),
            tasks: Vec::new(),
            tombstones: HashMap::new(),
        };
        let mut proj_used = std::collections::HashSet::new();
        let mut glob_used = std::collections::HashSet::new();

        for t in &view.tasks {
            let owner = *self
                .uid_owner
                .entry(t.uid.clone())
                .or_insert(BoardSource::Project);
            let mut task = t.clone();
            match owner {
                BoardSource::Project => {
                    let orig_id = proj_ids.get(t.uid.as_str()).copied().unwrap_or(0);
                    if orig_id > 0 && proj_used.insert(orig_id) {
                        task.id = orig_id;
                    } else {
                        task.id = next_proj.next_id;
                        next_proj.next_id += 1;
                        proj_used.insert(task.id);
                    }
                    if task.id >= next_proj.next_id {
                        next_proj.next_id = task.id + 1;
                    }
                    next_proj.tasks.push(task);
                }
                BoardSource::Global => {
                    let orig_id = glob_ids.get(t.uid.as_str()).copied().unwrap_or(0);
                    if orig_id > 0 && glob_used.insert(orig_id) {
                        task.id = orig_id;
                    } else {
                        task.id = next_glob.next_id;
                        next_glob.next_id += 1;
                        glob_used.insert(task.id);
                    }
                    if task.id >= next_glob.next_id {
                        next_glob.next_id = task.id + 1;
                    }
                    next_glob.tasks.push(task);
                }
            }
        }

        for (uid, &ts) in &view.tombstones {
            match self.uid_owner.get(uid).copied() {
                Some(BoardSource::Global) => {
                    next_glob.tombstones.insert(uid.clone(), ts);
                }
                _ => {
                    next_proj.tombstones.insert(uid.clone(), ts);
                }
            }
        }

        (next_proj, next_glob)
    }
}

fn persist_store_file(
    store: &mut TaskStore,
    path: &Path,
    last_stamp: &mut Option<FileStamp>,
    device_name: &str,
) -> Result<(), String> {
    let _lock = FileLock::acquire(path).map_err(|e| e.to_string())?;
    let current_stamp = file_stamp(path);
    if current_stamp.is_some() && current_stamp != *last_stamp {
        if let Ok(mut disk_store) = TaskStore::load_or_empty(path) {
            disk_store.ensure_sync_metadata(device_name);
            *store = merge_stores(store, &disk_store, device_name, device_name);
        }
    }
    store.save(path).map_err(|e| e.to_string())?;
    *last_stamp = file_stamp(path);
    Ok(())
}

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
    Due,
    Status,
    Priority,
    Repeat,
}

impl FormField {
    pub fn next(self) -> Self {
        match self {
            FormField::Title => FormField::Description,
            FormField::Description => FormField::Tags,
            FormField::Tags => FormField::Due,
            FormField::Due => FormField::Status,
            FormField::Status => FormField::Priority,
            FormField::Priority => FormField::Repeat,
            FormField::Repeat => FormField::Title,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            FormField::Title => FormField::Repeat,
            FormField::Description => FormField::Title,
            FormField::Tags => FormField::Description,
            FormField::Due => FormField::Tags,
            FormField::Status => FormField::Due,
            FormField::Priority => FormField::Status,
            FormField::Repeat => FormField::Priority,
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

    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    pub fn delete_word_backward(&mut self) {
        let end_cursor = self.cursor;
        self.move_word_left();
        if self.cursor < end_cursor {
            let start = self.byte_index(self.cursor);
            let end = self.byte_index(end_cursor);
            self.value.replace_range(start..end, "");
        }
    }

    pub fn clear_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index(self.cursor);
        self.value.replace_range(0..end, "");
        self.cursor = 0;
    }

    pub fn clear_to_end(&mut self) {
        let start = self.byte_index(self.cursor);
        if start < self.value.len() {
            self.value.truncate(start);
        }
    }

    /// Handle standard single-line editing & readline shortcuts.
    /// Returns `true` if the text value was modified.
    pub fn handle_key(&mut self, key: KeyEvent, allow_ctrl_e: bool) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let before = self.value.clone();
        match key.code {
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if ctrl => self.move_word_left(),
            KeyCode::Right if ctrl => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            KeyCode::Char('a') if ctrl => self.move_home(),
            KeyCode::Char('e') if ctrl && allow_ctrl_e => self.move_end(),
            KeyCode::Char('w') if ctrl => self.delete_word_backward(),
            KeyCode::Char('u') if ctrl => self.clear_to_start(),
            KeyCode::Char('k') if ctrl => self.clear_to_end(),
            KeyCode::Char('b') if alt => self.move_word_left(),
            KeyCode::Char('f') if alt => self.move_word_right(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            _ => {}
        }
        self.value != before
    }
}

#[derive(Debug, Clone)]
pub struct TaskForm {
    pub editing_id: Option<u64>,
    pub title: TextInput,
    pub description: TextInput,
    pub tags: TextInput,
    pub due: TextInput,
    pub repeat: TextInput,
    pub status: Status,
    pub priority: Priority,
    pub start: Option<String>,
    pub start_time: Option<String>,
    pub reminders: Vec<String>,
    pub parent_uid: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub percent_complete: Option<u8>,
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
            due: TextInput::new(""),
            repeat: TextInput::new(""),
            status: default_status,
            priority: Priority::Medium,
            start: None,
            start_time: None,
            reminders: Vec::new(),
            parent_uid: None,
            location: None,
            url: None,
            percent_complete: None,
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
            due: TextInput::new(task.due_display().unwrap_or_default()),
            repeat: TextInput::new(task.repeat.clone().unwrap_or_default()),
            status: task.status,
            priority: task.priority,
            start: task.start.clone(),
            start_time: task.start_time.clone(),
            reminders: task.reminders.clone(),
            parent_uid: task.parent_uid.clone(),
            location: task.location.clone(),
            url: task.url.clone(),
            percent_complete: task.percent_complete,
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
#[allow(clippy::large_enum_variant)]
pub enum Mode {
    Normal,
    Search,
    QuickAdd {
        input: TextInput,
        error: Option<String>,
    },
    TaskModal(TaskForm),
    TagFilter(usize),
    NotesPreview {
        task_id: u64,
        scroll: u16,
        selected_check: usize,
    },
    Archive {
        tasks: Vec<Task>,
        selected: usize,
    },
    Help(u16),
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
    pub hover_before_id: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct ModalRegions {
    pub area: Rect,
    pub title: Rect,
    pub description: Rect,
    pub tags: Rect,
    pub due: Rect,
    pub status: Rect,
    pub priority: Rect,
    pub repeat: Rect,
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
    pub board_btn: Rect,
    pub due_btn: Rect,
    pub sort_btn: Rect,
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
    pub notes_modal: Option<Rect>,
    pub notes_check_rows: Vec<(usize, Rect)>,
    pub archive_modal: Option<Rect>,
    pub archive_rows: Vec<(usize, Rect)>,
    pub help_modal: Option<Rect>,
}

impl LayoutRegions {
    pub fn clear(&mut self) {
        self.tab_todos = Rect::default();
        self.tab_kanban = Rect::default();
        self.board_btn = Rect::default();
        self.due_btn = Rect::default();
        self.sort_btn = Rect::default();
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
        self.notes_modal = None;
        self.notes_check_rows.clear();
        self.archive_modal = None;
        self.archive_rows.clear();
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

/// Extract the first `http://` or `https://` URL from plain text or Markdown links.
pub fn extract_first_url(text: &str) -> Option<String> {
    for prefix in ["https://", "http://"] {
        if let Some(start) = text.find(prefix) {
            let slice = &text[start..];
            let end = slice
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`')
                })
                .unwrap_or(slice.len());
            let cleaned = slice[..end].trim_end_matches(['.', ',', ';', ':']);
            if cleaned.len() > prefix.len() {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
struct LastClick {
    task_id: u64,
    at: Instant,
}

pub struct App {
    pub store: TaskStore,
    pub dual_board: Option<DualBoard>,
    pub storage_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub local_config_path: Option<PathBuf>,
    isolate_sync_peers: bool,
    pub theme: Theme,
    pub editor_config: Option<String>,
    pub editor_label: String,
    pub sync_config: SyncConfig,
    pub sync_requested: bool,
    pub syncing: bool,
    pub pending_editor: Option<PendingEditor>,
    pub pending_yank: Option<String>,
    pub pending_open_url: Option<String>,
    pub view: View,
    pub mode: Mode,
    pub todo_filter: TodoFilter,
    pub sort_order: SortOrder,
    pub due_focus: bool,
    pub marked_ids: Vec<u64>,
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
    pub redo_stack: Vec<TaskStore>,
    pub status_message: Option<String>,
    status_message_at: Option<Instant>,
    pub drag: Option<DragState>,
    pub regions: LayoutRegions,
    pub should_quit: bool,
    last_click: Option<LastClick>,
    last_file_stamp: Option<FileStamp>,
    last_config_stamp: Option<FileStamp>,
    last_local_config_stamp: Option<FileStamp>,
    dirty: bool,
}

const MAX_UNDO: usize = 50;
const STATUS_TOAST_DURATION: Duration = Duration::from_secs(4);

impl App {
    #[allow(dead_code)]
    pub fn new(store: TaskStore, storage_path: Option<PathBuf>) -> Self {
        Self::with_config(store, storage_path, AppConfig::default())
    }

    pub fn with_config(store: TaskStore, storage_path: Option<PathBuf>, config: AppConfig) -> Self {
        Self::with_config_and_path(store, storage_path, None, config)
    }

    pub fn with_config_and_path(
        store: TaskStore,
        storage_path: Option<PathBuf>,
        config_path: Option<PathBuf>,
        config: AppConfig,
    ) -> Self {
        Self::with_config_and_paths(store, storage_path, config_path, None, false, config)
    }

    pub fn with_config_and_paths(
        store: TaskStore,
        storage_path: Option<PathBuf>,
        config_path: Option<PathBuf>,
        local_config_path: Option<PathBuf>,
        isolate_sync_peers: bool,
        config: AppConfig,
    ) -> Self {
        let theme = config.resolved_theme();
        let editor_label = editor_display_name(config.editor.as_deref());
        let last_file_stamp = storage_path.as_deref().and_then(file_stamp);
        let last_config_stamp = config_path.as_deref().and_then(file_stamp);
        let last_local_config_stamp = local_config_path.as_deref().and_then(file_stamp);
        let initial_view = match config
            .default_view
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("kanban") | Some("board") => View::Kanban,
            _ => View::TodoList,
        };
        let initial_filter = match config
            .default_filter
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("todo") | Some("to-do") | Some("to_do") => TodoFilter::Todo,
            Some("in-progress") | Some("in_progress") | Some("doing") | Some("wip") => {
                TodoFilter::InProgress
            }
            Some("done") | Some("completed") => TodoFilter::Done,
            _ => TodoFilter::All,
        };
        let initial_sort = config
            .default_sort
            .as_deref()
            .and_then(|s| SortOrder::parse(s).ok())
            .unwrap_or(SortOrder::Manual);
        Self {
            store,
            dual_board: None,
            storage_path,
            config_path,
            local_config_path,
            isolate_sync_peers,
            theme,
            editor_config: config.editor,
            editor_label,
            sync_config: config.sync,
            sync_requested: false,
            syncing: false,
            pending_editor: None,
            pending_yank: None,
            pending_open_url: None,
            view: initial_view,
            mode: Mode::Normal,
            todo_filter: initial_filter,
            sort_order: initial_sort,
            due_focus: false,
            marked_ids: Vec::new(),
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
            redo_stack: Vec::new(),
            status_message: None,
            status_message_at: None,
            drag: None,
            regions: LayoutRegions::default(),
            should_quit: false,
            last_click: None,
            last_file_stamp,
            last_config_stamp,
            last_local_config_stamp,
            dirty: false,
        }
    }

    /// Attach a [`DualBoard`] so the TUI can switch live between `project`, `all` (project + global),
    /// and `global` via `b` or clicking the header board pill.
    pub fn attach_dual_board(&mut self, mut dual: DualBoard) {
        self.store = dual.compose_view();
        self.dual_board = Some(dual);
        self.clamp_selections();
    }

    pub fn board_scope_label(&self) -> Option<String> {
        self.dual_board.as_ref().map(DualBoard::scope_label)
    }

    pub fn has_any_sync_peers(&self) -> bool {
        if let Some(db) = &self.dual_board {
            match db.scope {
                BoardScope::Project => !db.project_sync_config.peers.is_empty(),
                BoardScope::Global => !db.global_sync_config.peers.is_empty(),
                BoardScope::All => {
                    !db.project_sync_config.peers.is_empty()
                        || !db.global_sync_config.peers.is_empty()
                }
            }
        } else {
            !self.sync_config.peers.is_empty()
        }
    }

    pub fn cycle_board_scope(&mut self) {
        let Some(mut db) = self.dual_board.take() else {
            return;
        };
        // Flush any in-memory view state back into backing stores before switching
        match db.scope {
            BoardScope::Project => db.project_store = self.store.clone(),
            BoardScope::Global => db.global_store = self.store.clone(),
            BoardScope::All => {
                let (p, g) = db.split_all_view(&self.store);
                db.project_store = p;
                db.global_store = g;
            }
        }
        db.scope = db.scope.cycle();
        let next_store = db.compose_view();
        let label = db.scope_label();
        self.dual_board = Some(db);
        self.store = next_store;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.marked_ids.clear();
        self.todo_selected = 0;
        self.todo_scroll = 0;
        self.kanban_selected = [0, 0, 0];
        self.kanban_scroll = [0, 0, 0];
        self.clamp_selections();
        self.set_message(format!("Board: {label}"));
    }

    pub fn current_kanban_status(&self) -> Status {
        Status::from_index(self.kanban_col)
    }

    pub fn effective_tags(&self, task: &Task) -> Vec<String> {
        let mut tags = task.tags.clone();
        if let Some(db) = &self.dual_board {
            let owner = db
                .uid_owner
                .get(&task.uid)
                .copied()
                .unwrap_or(match db.scope {
                    BoardScope::Global => BoardSource::Global,
                    BoardScope::Project | BoardScope::All => BoardSource::Project,
                });
            if db.scope == BoardScope::All {
                let board_tag = match owner {
                    BoardSource::Global => "global".to_string(),
                    BoardSource::Project => db.project_name.clone(),
                };
                if !board_tag.is_empty() && !tags.iter().any(|t| t == &board_tag) {
                    tags.push(board_tag);
                }
            }
            let sync_cfg = match owner {
                BoardSource::Global => &db.global_sync_config,
                BoardSource::Project => &db.project_sync_config,
            };
            if let Some(origin) = &task.origin {
                if sync_cfg.should_inject_tag_for(origin) {
                    let slug = normalize_device_tag(origin);
                    if !slug.is_empty() && !tags.iter().any(|t| t == &slug) {
                        tags.push(slug);
                    }
                }
            }
            return tags;
        }
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

    pub fn matches_search(&self, task: &Task) -> bool {
        let q = self.active_search_text();
        if q.trim().is_empty() {
            return true;
        }
        let eff_tags = self.effective_tags(task);
        matches_search_query(task, &eff_tags, q)
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

    pub fn matches_due_focus(&self, task: &Task) -> bool {
        if !self.due_focus {
            return true;
        }
        if task.status == Status::Done {
            return false;
        }
        let today = today_ymd();
        matches!(
            task.due_state(&today),
            Some(DueState::Overdue | DueState::Today)
        )
    }

    pub fn filtered_todo_ids(&self) -> Vec<u64> {
        let mut refs: Vec<&Task> = self
            .store
            .tasks
            .iter()
            .filter(|t| {
                self.todo_filter.matches(t.status)
                    && self.matches_due_focus(t)
                    && self.matches_tag(t)
                    && self.matches_search(t)
            })
            .collect();
        sort_task_refs(&mut refs, self.sort_order);
        refs.into_iter().map(|t| t.id).collect()
    }

    pub fn column_task_ids(&self, status: Status) -> Vec<u64> {
        let mut refs: Vec<&Task> = self
            .store
            .tasks
            .iter()
            .filter(|t| {
                t.status == status
                    && self.matches_due_focus(t)
                    && self.matches_tag(t)
                    && self.matches_search(t)
            })
            .collect();
        sort_task_refs(&mut refs, self.sort_order);
        refs.into_iter().map(|t| t.id).collect()
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

    pub fn is_marked(&self, task_id: u64) -> bool {
        self.marked_ids.contains(&task_id)
    }

    pub fn toggle_mark_selected(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        if let Some(pos) = self.marked_ids.iter().position(|&m| m == id) {
            self.marked_ids.remove(pos);
        } else {
            self.marked_ids.push(id);
        }
        self.move_selection_down();
        if self.marked_ids.is_empty() {
            self.set_message("Marks cleared");
        } else {
            self.set_message(format!("{} marked", self.marked_ids.len()));
        }
    }

    pub fn toggle_mark_all_visible(&mut self) {
        if !self.marked_ids.is_empty() {
            self.marked_ids.clear();
            self.set_message("Marks cleared");
            return;
        }
        let visible = match self.view {
            View::TodoList => self.filtered_todo_ids(),
            View::Kanban => self.column_task_ids(self.current_kanban_status()),
        };
        if visible.is_empty() {
            return;
        }
        self.marked_ids = visible;
        self.set_message(format!("Marked {} tasks", self.marked_ids.len()));
    }

    pub fn clamp_selections(&mut self) {
        self.marked_ids
            .retain(|&id| self.store.tasks.iter().any(|t| t.id == id));
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
        self.redo_stack.clear();
    }

    pub fn persist(&mut self) {
        self.persist_inner(true);
    }

    fn persist_inner(&mut self, allow_auto_sync: bool) {
        self.dirty = true;
        if let Some(mut db) = self.dual_board.take() {
            let proj_device = db.project_sync_config.effective_device_name();
            let glob_device = db.global_sync_config.effective_device_name();
            let mut auto_sync = false;
            match db.scope {
                BoardScope::Project => {
                    db.project_store = self.store.clone();
                    for t in &db.project_store.tasks {
                        db.uid_owner.insert(t.uid.clone(), BoardSource::Project);
                    }
                    if let Err(err) = persist_store_file(
                        &mut db.project_store,
                        &db.project_path,
                        &mut db.project_stamp,
                        &proj_device,
                    ) {
                        self.dual_board = Some(db);
                        self.set_message(format!("Save failed: {err}"));
                        return;
                    }
                    self.store = db.project_store.clone();
                    auto_sync = db.project_sync_config.auto_sync_on_save
                        && !db.project_sync_config.peers.is_empty();
                }
                BoardScope::Global => {
                    db.global_store = self.store.clone();
                    for t in &db.global_store.tasks {
                        db.uid_owner.insert(t.uid.clone(), BoardSource::Global);
                    }
                    if let Err(err) = persist_store_file(
                        &mut db.global_store,
                        &db.global_path,
                        &mut db.global_stamp,
                        &glob_device,
                    ) {
                        self.dual_board = Some(db);
                        self.set_message(format!("Save failed: {err}"));
                        return;
                    }
                    self.store = db.global_store.clone();
                    auto_sync = db.global_sync_config.auto_sync_on_save
                        && !db.global_sync_config.peers.is_empty();
                }
                BoardScope::All => {
                    let (mut next_proj, mut next_glob) = db.split_all_view(&self.store);
                    let proj_changed = next_proj != db.project_store;
                    let glob_changed = next_glob != db.global_store;
                    if proj_changed {
                        if let Err(err) = persist_store_file(
                            &mut next_proj,
                            &db.project_path,
                            &mut db.project_stamp,
                            &proj_device,
                        ) {
                            self.dual_board = Some(db);
                            self.set_message(format!("Save failed: {err}"));
                            return;
                        }
                        db.project_store = next_proj;
                        if db.project_sync_config.auto_sync_on_save
                            && !db.project_sync_config.peers.is_empty()
                        {
                            auto_sync = true;
                        }
                    }
                    if glob_changed {
                        if let Err(err) = persist_store_file(
                            &mut next_glob,
                            &db.global_path,
                            &mut db.global_stamp,
                            &glob_device,
                        ) {
                            self.dual_board = Some(db);
                            self.set_message(format!("Save failed: {err}"));
                            return;
                        }
                        db.global_store = next_glob;
                        if db.global_sync_config.auto_sync_on_save
                            && !db.global_sync_config.peers.is_empty()
                        {
                            auto_sync = true;
                        }
                    }
                }
            }
            self.dual_board = Some(db);
            self.dirty = false;
            if allow_auto_sync && auto_sync {
                self.sync_requested = true;
            }
            return;
        }

        if let Some(path) = self.storage_path.clone() {
            let local_name = self.sync_config.effective_device_name();
            if let Err(err) = persist_store_file(
                &mut self.store,
                &path,
                &mut self.last_file_stamp,
                &local_name,
            ) {
                self.set_message(format!("Save failed: {err}"));
                return;
            }
            self.dirty = false;
        }
        if allow_auto_sync
            && self.sync_config.auto_sync_on_save
            && !self.sync_config.peers.is_empty()
        {
            self.sync_requested = true;
        }
    }

    /// Check whether storage file(s) were modified externally (e.g. by CLI, MCP, or Syncthing)
    /// and reload/merge changes into the live TUI without losing selection.
    pub fn reload_if_changed(&mut self) -> bool {
        if let Some(mut db) = self.dual_board.take() {
            let selected_uid = self
                .selected_task_id()
                .and_then(|id| self.store.get(id))
                .map(|t| t.uid.clone());

            let proj_dev = db.project_sync_config.effective_device_name();
            let glob_dev = db.global_sync_config.effective_device_name();
            let mut any_changed = false;

            let p_absorbed = absorb_syncthing_conflicts(&db.project_path, &proj_dev).unwrap_or(0);
            let p_stamp = file_stamp(&db.project_path);
            if p_absorbed > 0 || (p_stamp.is_some() && p_stamp != db.project_stamp) {
                if let Ok(mut disk) = TaskStore::load_or_empty(&db.project_path) {
                    disk.ensure_sync_metadata(&proj_dev);
                    if disk != db.project_store {
                        db.project_store = disk;
                        any_changed = true;
                    }
                    db.project_stamp = p_stamp;
                }
            }

            let g_absorbed = absorb_syncthing_conflicts(&db.global_path, &glob_dev).unwrap_or(0);
            let g_stamp = file_stamp(&db.global_path);
            if g_absorbed > 0 || (g_stamp.is_some() && g_stamp != db.global_stamp) {
                if let Ok(mut disk) = TaskStore::load_or_empty(&db.global_path) {
                    disk.ensure_sync_metadata(&glob_dev);
                    if disk != db.global_store {
                        db.global_store = disk;
                        any_changed = true;
                    }
                    db.global_stamp = g_stamp;
                }
            }

            if any_changed {
                self.store = db.compose_view();
            }
            self.dual_board = Some(db);
            if any_changed {
                self.clamp_selections();
                if let Some(uid) = selected_uid {
                    if let Some(id) = self.store.tasks.iter().find(|t| t.uid == uid).map(|t| t.id) {
                        self.select_task_by_id(id);
                    }
                }
                self.set_message("Reloaded external changes");
            }
            return any_changed;
        }

        let Some(path) = self.storage_path.clone() else {
            return false;
        };
        let local_name = self.sync_config.effective_device_name();
        let absorbed = absorb_syncthing_conflicts(&path, &local_name).unwrap_or(0);
        let current_stamp = file_stamp(&path);
        if absorbed == 0 && (current_stamp.is_none() || current_stamp == self.last_file_stamp) {
            return false;
        }
        let Ok(mut disk_store) = TaskStore::load_or_empty(&path) else {
            return false;
        };
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

    /// Watch `config.toml` (and project-local `.roke.toml` if present) and hot-reload settings on change.
    pub fn reload_config_if_changed(&mut self) -> bool {
        let Some(path) = self.config_path.clone() else {
            return false;
        };
        let current_stamp = file_stamp(&path);
        let current_local_stamp = self.local_config_path.as_deref().and_then(file_stamp);
        if current_stamp == self.last_config_stamp
            && current_local_stamp == self.last_local_config_stamp
        {
            return false;
        }
        let local_changed = current_local_stamp != self.last_local_config_stamp;
        self.last_config_stamp = current_stamp;
        self.last_local_config_stamp = current_local_stamp;
        let global_res = AppConfig::load_or_default(&path);
        let res = if self.local_config_path.is_some() || self.isolate_sync_peers {
            AppConfig::load_with_local_overlay(
                &path,
                self.local_config_path.as_deref(),
                self.isolate_sync_peers,
            )
        } else {
            global_res.clone()
        };
        match res {
            Ok(cfg) => {
                self.theme = cfg.resolved_theme();
                self.editor_label = editor_display_name(cfg.editor.as_deref());
                self.editor_config = cfg.editor;
                self.sync_config = cfg.sync.clone();
                if let Some(db) = &mut self.dual_board {
                    db.project_sync_config = cfg.sync;
                    if let Ok(global_cfg) = global_res {
                        db.global_sync_config = global_cfg.sync;
                    }
                }
                self.set_message(if local_changed {
                    "Reloaded .roke.toml"
                } else {
                    "Reloaded config.toml"
                });
                true
            }
            Err(err) => {
                self.set_message(format!("Config reload error: {err}"));
                true
            }
        }
    }

    pub fn request_sync(&mut self) {
        if !self.has_any_sync_peers() {
            self.set_message("No sync peers configured in config.toml");
            return;
        }
        if self.syncing {
            self.set_message("Sync already in progress…");
            return;
        }
        self.sync_requested = true;
        self.set_message("Syncing…");
    }

    pub fn build_sync_jobs(&self) -> Vec<SyncJob> {
        if let Some(db) = &self.dual_board {
            let mut jobs = Vec::new();
            let include_project = matches!(db.scope, BoardScope::Project | BoardScope::All);
            let include_global = matches!(db.scope, BoardScope::Global | BoardScope::All);
            if include_project && !db.project_sync_config.peers.is_empty() {
                jobs.push(SyncJob {
                    slot: SyncSlot::Primary,
                    store: db.project_store.clone(),
                    config: db.project_sync_config.clone(),
                    state_dir: Some(sync_state_dir_for(Some(&db.project_path))),
                });
            }
            if include_global && !db.global_sync_config.peers.is_empty() {
                jobs.push(SyncJob {
                    slot: SyncSlot::Global,
                    store: db.global_store.clone(),
                    config: db.global_sync_config.clone(),
                    state_dir: Some(sync_state_dir_for(Some(&db.global_path))),
                });
            }
            return jobs;
        }
        if self.sync_config.peers.is_empty() {
            return Vec::new();
        }
        vec![SyncJob {
            slot: SyncSlot::Primary,
            store: self.store.clone(),
            config: self.sync_config.clone(),
            state_dir: Some(sync_state_dir_for(self.storage_path.as_deref())),
        }]
    }

    pub fn apply_sync_batch(&mut self, batch: SyncBatchOutcome) {
        if let Some(mut db) = self.dual_board.take() {
            let selected_uid = self
                .selected_task_id()
                .and_then(|id| self.store.get(id))
                .map(|t| t.uid.clone());

            let mut total_synced = 0usize;
            let mut errors = Vec::new();

            for (slot, outcome) in batch.results {
                total_synced += outcome.synced_peers;
                errors.extend(outcome.errors);
                match slot {
                    SyncSlot::Primary => {
                        let dev = db.project_sync_config.effective_device_name();
                        db.project_store =
                            merge_stores(&db.project_store, &outcome.merged_store, &dev, &dev);
                        let _ = persist_store_file(
                            &mut db.project_store,
                            &db.project_path,
                            &mut db.project_stamp,
                            &dev,
                        );
                    }
                    SyncSlot::Global => {
                        let dev = db.global_sync_config.effective_device_name();
                        db.global_store =
                            merge_stores(&db.global_store, &outcome.merged_store, &dev, &dev);
                        let _ = persist_store_file(
                            &mut db.global_store,
                            &db.global_path,
                            &mut db.global_stamp,
                            &dev,
                        );
                    }
                }
            }

            self.store = db.compose_view();
            self.dual_board = Some(db);
            self.clamp_selections();
            if let Some(uid) = selected_uid {
                if let Some(id) = self.store.tasks.iter().find(|t| t.uid == uid).map(|t| t.id) {
                    self.select_task_by_id(id);
                }
            }
            if errors.is_empty() {
                let noun = if total_synced == 1 { "peer" } else { "peers" };
                self.set_message(format!("Synced {total_synced} {noun}"));
            } else {
                self.set_message(format!("Sync error: {}", errors[0]));
            }
            return;
        }

        if let Some((_, outcome)) = batch.results.into_iter().next() {
            self.apply_sync_outcome(outcome);
        }
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
        self.status_message_at = Some(Instant::now());
    }

    pub fn tick_status_message(&mut self) -> bool {
        if self.syncing {
            return false;
        }
        if let Some(at) = self.status_message_at {
            if at.elapsed() >= STATUS_TOAST_DURATION {
                self.status_message = None;
                self.status_message_at = None;
                return true;
            }
        }
        false
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
            Mode::QuickAdd { .. } => self.handle_quick_add_key(key),
            Mode::TaskModal(_) => self.handle_modal_key(key),
            Mode::TagFilter(_) => self.handle_tag_filter_key(key),
            Mode::NotesPreview { .. } => self.handle_notes_preview_key(key),
            Mode::Archive { .. } => self.handle_archive_key(key),
            Mode::Help(_) => self.handle_help_key(key),
        }
    }

    fn toggle_notes_checklist(&mut self, task_id: u64, check_idx: usize) {
        if self.store.get(task_id).is_none() {
            return;
        }
        self.push_undo();
        let toggled = self
            .store
            .get_mut(task_id)
            .and_then(|t| t.toggle_checklist_item(check_idx));
        if let Some(checked) = toggled {
            self.persist();
            self.set_message(if checked {
                "Checked item"
            } else {
                "Unchecked item"
            });
        } else {
            self.undo_stack.pop();
        }
    }

    fn handle_notes_preview_key(&mut self, key: KeyEvent) {
        let Mode::NotesPreview {
            task_id,
            mut scroll,
            mut selected_check,
        } = self.mode
        else {
            return;
        };
        let total_checks = self
            .store
            .get(task_id)
            .and_then(|t| t.checklist_progress())
            .map(|(_, total)| total)
            .unwrap_or(0);
        if total_checks > 0 && selected_check >= total_checks {
            selected_check = total_checks - 1;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                scroll = scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                scroll = scroll.saturating_sub(1);
            }
            KeyCode::Char('d') | KeyCode::PageDown => {
                scroll = scroll.saturating_add(8);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                scroll = scroll.saturating_sub(8);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                scroll = u16::MAX;
            }
            KeyCode::Tab | KeyCode::Char(']') if total_checks > 0 => {
                selected_check = (selected_check + 1) % total_checks;
            }
            KeyCode::BackTab | KeyCode::Char('[') if total_checks > 0 => {
                selected_check = if selected_check == 0 {
                    total_checks - 1
                } else {
                    selected_check - 1
                };
            }
            KeyCode::Char(' ') | KeyCode::Enter if total_checks > 0 => {
                self.toggle_notes_checklist(task_id, selected_check);
            }
            KeyCode::Char('n') => {
                self.pending_editor = Some(PendingEditor::Task(task_id));
            }
            KeyCode::Char('y') => {
                if let Some(task) = self.store.get(task_id) {
                    if task.description.trim().is_empty() {
                        self.pending_yank = Some(task.title.clone());
                        self.set_message("Yanked title");
                    } else {
                        self.pending_yank = Some(task.description.clone());
                        self.set_message("Yanked notes");
                    }
                }
            }
            KeyCode::Char('o') => {
                self.open_url_for_task(task_id);
            }
            KeyCode::Char('e') => {
                self.mode = Mode::Normal;
                self.select_task_by_id(task_id);
                self.open_edit_modal();
                return;
            }
            _ => {}
        }
        self.mode = Mode::NotesPreview {
            task_id,
            scroll,
            selected_check,
        };
    }

    pub fn open_archive_modal(&mut self) {
        let Some(path) = &self.storage_path else {
            self.set_message("Archive requires a backing storage file");
            return;
        };
        let arch_path = archive_path_for(path);
        let Ok(store) = TaskStore::load_or_empty(&arch_path) else {
            self.set_message("Could not read archive.jsonl");
            return;
        };
        if store.tasks.is_empty() {
            self.set_message("Archive is empty");
            return;
        }
        self.mode = Mode::Archive {
            tasks: store.tasks,
            selected: 0,
        };
    }

    fn restore_archived_at(&mut self, tasks: &mut Vec<Task>, idx: usize) {
        if idx >= tasks.len() {
            return;
        }
        let mut restored = tasks.remove(idx);
        let title = restored.title.clone();
        if let Some(path) = &self.storage_path {
            let arch_path = archive_path_for(path);
            let mut arch_store = TaskStore::new_empty();
            arch_store.tasks = tasks.clone();
            if let Ok(_lock) = FileLock::acquire(&arch_path) {
                let _ = arch_store.save(&arch_path);
            }
        }
        self.push_undo();
        let tomb_ts = self.store.tombstones.remove(&restored.uid).unwrap_or(0);
        restored.updated_at = now_millis()
            .max(restored.updated_at.saturating_add(1))
            .max(tomb_ts.saturating_add(1));
        if restored.id == 0 || self.store.tasks.iter().any(|t| t.id == restored.id) {
            restored.id = self.store.next_id;
            self.store.next_id += 1;
        } else if restored.id >= self.store.next_id {
            self.store.next_id = restored.id + 1;
        }
        let restored_id = restored.id;
        self.store.tasks.push(restored);
        self.persist();
        self.clamp_selections();
        self.select_task_by_id(restored_id);
        self.set_message(format!("Restored \"{title}\""));
    }

    fn handle_archive_key(&mut self, key: KeyEvent) {
        let Mode::Archive {
            mut tasks,
            mut selected,
        } = std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return;
        };
        if tasks.is_empty() {
            return;
        }
        if selected >= tasks.len() {
            selected = tasks.len() - 1;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('V') => {
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if selected + 1 < tasks.len() {
                    selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                selected = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                selected = tasks.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('r') => {
                self.restore_archived_at(&mut tasks, selected);
                if tasks.is_empty() {
                    return;
                }
                if selected >= tasks.len() {
                    selected = tasks.len() - 1;
                }
            }
            _ => {}
        }
        self.mode = Mode::Archive { tasks, selected };
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
        let Mode::Help(mut scroll) = self.mode else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                scroll = scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                scroll = scroll.saturating_sub(1);
            }
            KeyCode::Char('d') | KeyCode::PageDown => {
                scroll = scroll.saturating_add(8);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                scroll = scroll.saturating_sub(8);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                scroll = u16::MAX;
            }
            _ => {}
        }
        self.mode = Mode::Help(scroll);
    }

    fn handle_quick_add_key(&mut self, key: KeyEvent) {
        let Mode::QuickAdd {
            mut input,
            mut error,
        } = std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                return;
            }
            KeyCode::Enter => match parse_quick_add(&input.value) {
                Ok(parsed) => {
                    let status = self.default_new_task_status();
                    let mut tags = self.tag_filters.clone();
                    for t in parsed.tags {
                        if !tags.contains(&t) {
                            tags.push(t);
                        }
                    }
                    self.push_undo();
                    let device = self.sync_config.effective_device_name();
                    let id = self.store.add_with_origin(
                        parsed.title.clone(),
                        String::new(),
                        status,
                        parsed.priority.unwrap_or(Priority::Medium),
                        tags,
                        &device,
                    );
                    if let Some(task) = self.store.get_mut(id) {
                        task.due = parsed.due;
                        task.due_time = parsed.due_time;
                        task.repeat = parsed.repeat;
                    }
                    self.persist();
                    self.clamp_selections();
                    self.select_task_by_id(id);
                    self.set_message(format!("Added \"{}\"", parsed.title));
                    return;
                }
                Err(msg) => {
                    error = Some(msg);
                }
            },
            _ => {
                if input.handle_key(key, true) {
                    error = None;
                }
            }
        }
        self.mode = Mode::QuickAdd { input, error };
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
            _ => {
                if self.search_input.handle_key(key, true) {
                    self.clamp_selections();
                }
            }
        }
    }

    pub fn open_notes_in_editor(&mut self) {
        if let Some(id) = self.selected_task_id() {
            self.pending_editor = Some(PendingEditor::Task(id));
        }
    }

    fn resolve_parent_uid(
        &self,
        raw: Option<&str>,
        self_uid: Option<&str>,
    ) -> Result<Option<String>, String> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let cleaned = raw.trim_start_matches('#');
        let resolved = if let Some(pid) = self.store.find_id(cleaned) {
            self.store
                .get(pid)
                .map(|t| t.uid.clone())
                .unwrap_or_else(|| cleaned.to_string())
        } else {
            cleaned.to_string()
        };
        if self_uid.is_some_and(|u| u == resolved) {
            return Err("A task cannot be its own parent".into());
        }
        Ok(Some(resolved))
    }

    pub fn editor_initial_document(&self, target: PendingEditor) -> Option<TaskDocument> {
        match target {
            PendingEditor::Task(id) => self.store.get(id).map(TaskDocument::from_task),
            PendingEditor::Modal => {
                if let Mode::TaskModal(form) = &self.mode {
                    let (due, due_time) = if form.due.value.trim().is_empty() {
                        (None, None)
                    } else if let Ok((d, t)) = parse_due_spec(form.due.value.trim()) {
                        (Some(d), t)
                    } else {
                        (None, None)
                    };
                    let repeat = if form.repeat.value.trim().is_empty() {
                        None
                    } else {
                        parse_repeat(form.repeat.value.trim()).ok()
                    };
                    Some(TaskDocument {
                        title: form.title.value.clone(),
                        description: form.description.value.clone(),
                        status: form.status,
                        priority: form.priority,
                        tags: parse_tags(&form.tags.value),
                        due,
                        due_time,
                        start: form.start.clone(),
                        start_time: form.start_time.clone(),
                        repeat,
                        reminders: form.reminders.clone(),
                        parent_uid: form.parent_uid.clone(),
                        location: form.location.clone(),
                        url: form.url.clone(),
                        percent_complete: form.percent_complete,
                    })
                } else {
                    None
                }
            }
        }
    }

    pub fn apply_editor_result(
        &mut self,
        target: PendingEditor,
        result: Result<Option<TaskDocument>, String>,
    ) {
        match result {
            Err(err) => {
                self.set_message(format!("Editor error: {err}"));
            }
            Ok(None) => {
                self.set_message("Task unchanged");
            }
            Ok(Some(mut doc)) => match target {
                PendingEditor::Task(id) => {
                    let Some(self_uid) = self.store.get(id).map(|t| t.uid.clone()) else {
                        self.set_message("Task no longer exists");
                        return;
                    };
                    match self.resolve_parent_uid(doc.parent_uid.as_deref(), Some(&self_uid)) {
                        Ok(resolved) => doc.parent_uid = resolved,
                        Err(err) => {
                            self.set_message(err);
                            return;
                        }
                    }
                    self.push_undo();
                    if let Some(task) = self.store.get_mut(id) {
                        let was_done = task.status == Status::Done;
                        let target_status = doc.status;
                        doc.apply_to_task(task);
                        if target_status == Status::Done && !was_done && task.repeat.is_some() {
                            task.complete_if_recurring();
                        }
                        task.touch();
                    }
                    self.persist();
                    self.clamp_selections();
                    self.select_task_by_id(id);
                    self.set_message("Task updated");
                }
                PendingEditor::Modal => {
                    if let Mode::TaskModal(form) = &mut self.mode {
                        form.title = TextInput::new(doc.title);
                        form.description = TextInput::new(doc.description);
                        form.tags = TextInput::new(format_tags(&doc.tags));
                        let due_disp = match (&doc.due, &doc.due_time) {
                            (Some(d), Some(t)) => format!("{d} {t}"),
                            (Some(d), None) => d.clone(),
                            (None, _) => String::new(),
                        };
                        form.due = TextInput::new(due_disp);
                        form.repeat = TextInput::new(doc.repeat.unwrap_or_default());
                        form.status = doc.status;
                        form.priority = doc.priority;
                        form.start = doc.start;
                        form.start_time = doc.start_time;
                        form.reminders = doc.reminders;
                        form.parent_uid = doc.parent_uid;
                        form.location = doc.location;
                        form.url = doc.url;
                        form.percent_complete = doc.percent_complete;
                        form.error = None;
                        self.set_message("Task updated in form");
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
                FormField::Title => {
                    if form.title.handle_key(key, false) {
                        form.error = None;
                    }
                }
                FormField::Description => {
                    if form.description.value.contains('\n') {
                        match key.code {
                            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char(_)
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
                        form.description.handle_key(key, false);
                    }
                }
                FormField::Tags => {
                    form.tags.handle_key(key, false);
                }
                FormField::Due => {
                    if form.due.handle_key(key, false) {
                        form.error = None;
                    }
                }
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
                FormField::Repeat => {
                    if form.repeat.handle_key(key, false) {
                        form.error = None;
                    }
                }
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
        let due_raw = form.due.value.trim();
        let (due, due_time) = if due_raw.is_empty() {
            (None, None)
        } else {
            let (d, t) = parse_due_spec(due_raw)?;
            (Some(d), t)
        };
        let repeat_raw = form.repeat.value.trim();
        let repeat = if repeat_raw.is_empty() {
            None
        } else {
            Some(parse_repeat(repeat_raw)?)
        };
        let self_uid = form
            .editing_id
            .and_then(|id| self.store.get(id))
            .map(|t| t.uid.clone());
        let parent_uid =
            self.resolve_parent_uid(form.parent_uid.as_deref(), self_uid.as_deref())?;
        self.push_undo();

        let target_id = if let Some(id) = form.editing_id {
            if let Some(task) = self.store.get_mut(id) {
                task.title = title;
                task.description = description;
                task.set_status(form.status);
                task.priority = form.priority;
                task.tags = tags;
                task.due = due;
                task.due_time = due_time;
                task.start = form.start.clone();
                task.start_time = form.start_time.clone();
                task.repeat = repeat;
                task.reminders = form.reminders.clone();
                task.parent_uid = parent_uid;
                task.location = form.location.clone();
                task.url = form.url.clone();
                task.percent_complete = form.percent_complete;
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
            if let Some(task) = self.store.get_mut(id) {
                task.due = due;
                task.due_time = due_time;
                task.start = form.start.clone();
                task.start_time = form.start_time.clone();
                task.repeat = repeat;
                task.reminders = form.reminders.clone();
                task.parent_uid = parent_uid;
                task.location = form.location.clone();
                task.url = form.url.clone();
                task.percent_complete = form.percent_complete;
            }
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
                KeyCode::Char('x') => {
                    if let Some(id) = self.selected_task_id() {
                        self.open_url_for_task(id);
                    }
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
                self.mode = Mode::Help(0);
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
            KeyCode::Char('Q') => {
                self.mode = Mode::QuickAdd {
                    input: TextInput::new(""),
                    error: None,
                };
            }
            KeyCode::Char('e') | KeyCode::Char('i') | KeyCode::Enter => {
                self.open_edit_modal();
            }
            KeyCode::Char('n') => {
                self.open_notes_in_editor();
            }
            KeyCode::Char('v') => {
                if let Some(id) = self.selected_task_id() {
                    self.mode = Mode::NotesPreview {
                        task_id: id,
                        scroll: 0,
                        selected_check: 0,
                    };
                }
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
            KeyCode::Char('b') if self.dual_board.is_some() => {
                self.cycle_board_scope();
            }
            KeyCode::Char('p') => {
                self.cycle_selected_priority();
            }
            KeyCode::Char('O') => {
                self.cycle_sort_order();
            }
            KeyCode::Char('A') => {
                self.archive_done_tasks();
            }
            KeyCode::Char('V') => {
                self.open_archive_modal();
            }
            KeyCode::Char('y') => {
                self.yank_selected(false);
            }
            KeyCode::Char('Y') => {
                self.yank_selected(true);
            }
            KeyCode::Char('m') => {
                self.toggle_mark_selected();
            }
            KeyCode::Char('M') => {
                self.toggle_mark_all_visible();
            }
            KeyCode::Char('t') => {
                self.open_tag_filter_modal();
            }
            KeyCode::Char('T') => {
                self.open_edit_tags_modal();
            }
            KeyCode::Char('D') => {
                self.open_edit_due_modal();
            }
            KeyCode::Char('@') => {
                self.toggle_due_focus();
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.delete_selected_task();
            }
            KeyCode::Char('u') => {
                self.undo();
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.redo();
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
            KeyCode::Esc if !self.marked_ids.is_empty() => {
                self.marked_ids.clear();
                self.set_message("Marks cleared");
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
            KeyCode::Esc if self.due_focus => {
                self.due_focus = false;
                self.clamp_selections();
                self.set_message("Showing all dates");
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

    fn default_new_task_status(&self) -> Status {
        match self.view {
            View::Kanban => self.current_kanban_status(),
            View::TodoList => match self.todo_filter {
                TodoFilter::All | TodoFilter::Todo => Status::Todo,
                TodoFilter::InProgress => Status::InProgress,
                TodoFilter::Done => Status::Done,
            },
        }
    }

    pub fn open_add_modal(&mut self) {
        let default_status = self.default_new_task_status();
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

    pub fn open_edit_due_modal(&mut self) {
        if let Some(id) = self.selected_task_id() {
            if let Some(task) = self.store.get(id).cloned() {
                let mut form = TaskForm::edit_task(&task);
                form.active_field = FormField::Due;
                self.mode = Mode::TaskModal(form);
            }
        }
    }

    pub fn toggle_due_focus(&mut self) {
        self.due_focus = !self.due_focus;
        self.todo_selected = 0;
        self.todo_scroll = 0;
        self.kanban_selected = [0, 0, 0];
        self.kanban_scroll = [0, 0, 0];
        self.clamp_selections();
        if self.due_focus {
            self.set_message("Focus: due today & overdue");
        } else {
            self.set_message("Showing all dates");
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
        let mut rolled_due: Option<String> = None;
        if let Some(task) = self.store.get_mut(task_id) {
            if next_status == Status::Done && task.repeat.is_some() {
                rolled_due = task.complete_if_recurring();
            } else {
                task.set_status(next_status);
                task.touch();
            }
        }
        self.persist();
        self.clamp_selections();
        if self.view == View::TodoList && self.todo_filter == TodoFilter::All {
            self.select_task_by_id(task_id);
        }
        if let Some(next_due) = rolled_due {
            self.set_message(format!("Completed — next due {next_due}"));
        } else {
            self.set_message(format!("Marked as {}", next_status.label()));
        }
    }

    fn toggle_or_advance_selected(&mut self) {
        if !self.marked_ids.is_empty() {
            match self.view {
                View::TodoList => self.batch_toggle_marked_done(),
                View::Kanban => self.cycle_selected_status(),
            }
            return;
        }
        let Some(id) = self.selected_task_id() else {
            return;
        };
        match self.view {
            View::TodoList => self.toggle_task_done(id),
            View::Kanban => self.cycle_selected_status(),
        }
    }

    fn batch_toggle_marked_done(&mut self) {
        let ids: Vec<u64> = self
            .marked_ids
            .iter()
            .copied()
            .filter(|&id| self.store.get(id).is_some())
            .collect();
        if ids.is_empty() {
            return;
        }
        let all_done = ids
            .iter()
            .all(|&id| self.store.get(id).is_some_and(|t| t.status == Status::Done));
        let target = if all_done { Status::Todo } else { Status::Done };
        self.push_undo();
        for &id in &ids {
            if let Some(task) = self.store.get_mut(id) {
                if target == Status::Done && task.repeat.is_some() {
                    task.complete_if_recurring();
                } else {
                    task.set_status(target);
                    task.touch();
                }
            }
        }
        self.persist();
        self.clamp_selections();
        let n = ids.len();
        self.set_message(format!("Marked {n} tasks as {}", target.label()));
    }

    fn cycle_selected_status(&mut self) {
        if !self.marked_ids.is_empty() {
            let ids: Vec<u64> = self
                .marked_ids
                .iter()
                .copied()
                .filter(|&id| self.store.get(id).is_some())
                .collect();
            if ids.is_empty() {
                return;
            }
            self.push_undo();
            for &id in &ids {
                if let Some(task) = self.store.get_mut(id) {
                    let next = task.status.cycle();
                    if next == Status::Done && task.repeat.is_some() {
                        task.complete_if_recurring();
                    } else {
                        task.set_status(next);
                        task.touch();
                    }
                }
            }
            self.persist();
            self.clamp_selections();
            self.set_message(format!("Cycled status on {} tasks", ids.len()));
            return;
        }
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
        if !self.marked_ids.is_empty() {
            let ids: Vec<u64> = self
                .marked_ids
                .iter()
                .copied()
                .filter(|&id| self.store.get(id).is_some())
                .collect();
            let mut moved = 0usize;
            if ids.iter().any(|&id| {
                self.store.get(id).is_some_and(|t| {
                    if forward {
                        t.status.next().is_some()
                    } else {
                        t.status.prev().is_some()
                    }
                })
            }) {
                self.push_undo();
                for &id in &ids {
                    if let Some(task) = self.store.get_mut(id) {
                        let target = if forward {
                            task.status.next()
                        } else {
                            task.status.prev()
                        };
                        if let Some(next_status) = target {
                            if next_status == Status::Done && task.repeat.is_some() {
                                task.complete_if_recurring();
                            } else {
                                task.set_status(next_status);
                                task.touch();
                            }
                            moved += 1;
                        }
                    }
                }
                self.persist();
                self.clamp_selections();
                self.set_message(format!("Moved {moved} tasks"));
            }
            return;
        }
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
        let mut rolled_due: Option<String> = None;
        if let Some(task) = self.store.get_mut(task_id) {
            if target == Status::Done && task.repeat.is_some() {
                rolled_due = task.complete_if_recurring();
            } else {
                task.set_status(target);
                task.touch();
            }
        }
        self.persist();
        self.clamp_selections();
        if follow {
            self.select_task_by_id(task_id);
        }
        if let Some(next_due) = rolled_due {
            self.set_message(format!("Completed — next due {next_due}"));
        } else {
            self.set_message(format!("Moved to {}", target.label()));
        }
    }

    pub fn cycle_sort_order(&mut self) {
        self.sort_order = self.sort_order.cycle();
        self.clamp_selections();
        self.set_message(format!("Sort: {}", self.sort_order.label()));
    }

    fn reorder_selected_task(&mut self, down: bool) {
        if self.sort_order != SortOrder::Manual {
            self.set_message("Switch to manual sort (O) to reorder");
            return;
        }
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
        self.store.swap_order(pos_a, pos_b);
        self.persist();

        match self.view {
            View::TodoList => self.todo_selected = neighbor_idx,
            View::Kanban => self.kanban_selected[self.kanban_col] = neighbor_idx,
        }
    }

    fn cycle_selected_priority(&mut self) {
        if !self.marked_ids.is_empty() {
            let ids: Vec<u64> = self
                .marked_ids
                .iter()
                .copied()
                .filter(|&id| self.store.get(id).is_some())
                .collect();
            if ids.is_empty() {
                return;
            }
            self.push_undo();
            for &id in &ids {
                if let Some(task) = self.store.get_mut(id) {
                    task.priority = task.priority.cycle();
                    task.touch();
                }
            }
            self.persist();
            self.set_message(format!("Cycled priority on {} tasks", ids.len()));
            return;
        }
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
        if !self.marked_ids.is_empty() {
            let ids: Vec<u64> = self
                .marked_ids
                .iter()
                .copied()
                .filter(|&id| self.store.get(id).is_some())
                .collect();
            if ids.is_empty() {
                return;
            }
            self.push_undo();
            let mut deleted = 0usize;
            for id in ids {
                if self.store.remove(id).is_some() {
                    deleted += 1;
                }
            }
            self.marked_ids.clear();
            self.persist();
            self.clamp_selections();
            self.set_message(format!("Deleted {deleted} tasks (press u to undo)"));
            return;
        }
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

    pub fn open_url_for_task(&mut self, task_id: u64) {
        let Some(task) = self.store.get(task_id) else {
            return;
        };
        if let Some(url) = task
            .url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| extract_first_url(&task.title))
            .or_else(|| extract_first_url(&task.description))
        {
            self.set_message(format!("Opening {url}"));
            self.pending_open_url = Some(url);
        } else {
            self.set_message("No URL found in task");
        }
    }

    fn format_task_yank(&self, task: &Task, full_markdown: bool) -> String {
        if !full_markdown {
            return task.title.clone();
        }
        let check = if task.status == Status::Done {
            "x"
        } else {
            " "
        };
        let mut out = format!("- [{check}] {}", task.title);
        let eff_tags = self.effective_tags(task);
        if !eff_tags.is_empty() {
            for tag in &eff_tags {
                out.push_str(&format!(" #{tag}"));
            }
        }
        if let Some(due) = &task.due {
            if let Some(t) = &task.due_time {
                out.push_str(&format!(" @{due}@{t}"));
            } else {
                out.push_str(&format!(" @{due}"));
            }
        }
        if !task.description.trim().is_empty() {
            out.push('\n');
            for line in task.description.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
            out.truncate(out.trim_end().len());
        }
        out
    }

    pub fn yank_selected(&mut self, full_markdown: bool) {
        if !self.marked_ids.is_empty() {
            let parts: Vec<String> = self
                .marked_ids
                .iter()
                .filter_map(|&id| self.store.get(id))
                .map(|t| self.format_task_yank(t, full_markdown))
                .collect();
            if !parts.is_empty() {
                let n = parts.len();
                self.pending_yank = Some(parts.join("\n"));
                self.set_message(format!("Yanked {n} tasks"));
            }
            return;
        }
        let Some(id) = self.selected_task_id() else {
            return;
        };
        let Some(task) = self.store.get(id).cloned() else {
            return;
        };
        self.pending_yank = Some(self.format_task_yank(&task, full_markdown));
        self.set_message(if full_markdown {
            "Yanked task"
        } else {
            "Yanked title"
        });
    }

    pub fn archive_done_tasks(&mut self) {
        let done_ids: Vec<u64> = self
            .store
            .tasks
            .iter()
            .filter(|t| t.status == Status::Done)
            .map(|t| t.id)
            .collect();
        if done_ids.is_empty() {
            self.set_message("No done tasks to archive");
            return;
        }
        let done_tasks: Vec<Task> = done_ids
            .iter()
            .filter_map(|&id| self.store.get(id).cloned())
            .collect();

        if let Some(db) = &self.dual_board {
            match db.scope {
                BoardScope::Project => {
                    let archive_path = archive_path_for(&db.project_path);
                    if let Err(err) = append_tasks_to_archive(&archive_path, &done_tasks) {
                        self.set_message(format!("Archive failed: {err}"));
                        return;
                    }
                }
                BoardScope::Global => {
                    let archive_path = archive_path_for(&db.global_path);
                    if let Err(err) = append_tasks_to_archive(&archive_path, &done_tasks) {
                        self.set_message(format!("Archive failed: {err}"));
                        return;
                    }
                }
                BoardScope::All => {
                    let mut proj_done = Vec::new();
                    let mut glob_done = Vec::new();
                    for task in &done_tasks {
                        match db.uid_owner.get(&task.uid).copied() {
                            Some(BoardSource::Global) => glob_done.push(task.clone()),
                            _ => proj_done.push(task.clone()),
                        }
                    }
                    if let Err(err) =
                        append_tasks_to_archive(&archive_path_for(&db.project_path), &proj_done)
                    {
                        self.set_message(format!("Archive failed: {err}"));
                        return;
                    }
                    if let Err(err) =
                        append_tasks_to_archive(&archive_path_for(&db.global_path), &glob_done)
                    {
                        self.set_message(format!("Archive failed: {err}"));
                        return;
                    }
                }
            }
        } else if let Some(path) = &self.storage_path {
            let archive_path = archive_path_for(path);
            if let Err(err) = append_tasks_to_archive(&archive_path, &done_tasks) {
                self.set_message(format!("Archive failed: {err}"));
                return;
            }
        }

        self.push_undo();
        for id in &done_ids {
            self.store.remove(*id);
        }
        self.persist();
        self.clamp_selections();
        let n = done_ids.len();
        let noun = if n == 1 { "task" } else { "tasks" };
        self.set_message(format!("Archived {n} done {noun} (press u to undo)"));
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
            self.redo_stack.push(self.store.clone());
            if self.redo_stack.len() > MAX_UNDO {
                self.redo_stack.remove(0);
            }
            self.store = Self::reconcile_restored_store(&self.store, prev);
            self.persist();
            self.clamp_selections();
            self.set_message("Undid last change");
        } else {
            self.set_message("Nothing to undo");
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.store.clone());
            if self.undo_stack.len() > MAX_UNDO {
                self.undo_stack.remove(0);
            }
            self.store = Self::reconcile_restored_store(&self.store, next);
            self.persist();
            self.clamp_selections();
            self.set_message("Redid change");
        } else {
            self.set_message("Nothing to redo");
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let x = mouse.column;
        let y = mouse.row;

        match self.mode {
            Mode::Help(scroll) => {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(area) = self.regions.help_modal {
                            if !rect_contains(area, x, y) {
                                self.mode = Mode::Normal;
                            }
                        } else {
                            self.mode = Mode::Normal;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        self.mode = Mode::Help(scroll.saturating_add(2));
                    }
                    MouseEventKind::ScrollUp => {
                        self.mode = Mode::Help(scroll.saturating_sub(2));
                    }
                    _ => {}
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
            Mode::NotesPreview {
                task_id,
                scroll,
                selected_check,
            } => {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(area) = self.regions.notes_modal {
                            if !rect_contains(area, x, y) {
                                self.mode = Mode::Normal;
                            } else {
                                let checks = self.regions.notes_check_rows.clone();
                                for (check_idx, rect) in checks {
                                    if rect_contains(rect, x, y) {
                                        self.toggle_notes_checklist(task_id, check_idx);
                                        self.mode = Mode::NotesPreview {
                                            task_id,
                                            scroll,
                                            selected_check: check_idx,
                                        };
                                        return;
                                    }
                                }
                            }
                        } else {
                            self.mode = Mode::Normal;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        self.mode = Mode::NotesPreview {
                            task_id,
                            scroll: scroll.saturating_add(2),
                            selected_check,
                        };
                    }
                    MouseEventKind::ScrollUp => {
                        self.mode = Mode::NotesPreview {
                            task_id,
                            scroll: scroll.saturating_sub(2),
                            selected_check,
                        };
                    }
                    _ => {}
                }
                return;
            }
            Mode::Archive { .. } => {
                let Mode::Archive {
                    mut tasks,
                    mut selected,
                } = std::mem::replace(&mut self.mode, Mode::Normal)
                else {
                    return;
                };
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(area) = self.regions.archive_modal {
                            if !rect_contains(area, x, y) {
                                return;
                            }
                            let rows = self.regions.archive_rows.clone();
                            for (idx, rect) in rows {
                                if rect_contains(rect, x, y) {
                                    selected = idx;
                                    self.restore_archived_at(&mut tasks, idx);
                                    if tasks.is_empty() {
                                        return;
                                    }
                                    if selected >= tasks.len() {
                                        selected = tasks.len() - 1;
                                    }
                                    break;
                                }
                            }
                        } else {
                            return;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if !tasks.is_empty() && selected + 1 < tasks.len() {
                            selected += 1;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        selected = selected.saturating_sub(1);
                    }
                    _ => {}
                }
                self.mode = Mode::Archive { tasks, selected };
                return;
            }
            Mode::QuickAdd { .. } => {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.mode = Mode::Normal;
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
        } else if rect_contains(modal_regions.due, x, y) {
            form.active_field = FormField::Due;
        } else if rect_contains(modal_regions.status, x, y) {
            form.active_field = FormField::Status;
            form.status = form.status.cycle();
        } else if rect_contains(modal_regions.priority, x, y) {
            form.active_field = FormField::Priority;
            form.priority = form.priority.cycle();
        } else if rect_contains(modal_regions.repeat, x, y) {
            form.active_field = FormField::Repeat;
        }

        self.mode = Mode::TaskModal(form);
    }

    fn is_double_click(&mut self, task_id: u64) -> bool {
        let now = Instant::now();
        let is_double = self.last_click.as_ref().is_some_and(|c| {
            c.task_id == task_id && now.duration_since(c.at) < Duration::from_millis(400)
        });
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
        if rect_contains(self.regions.board_btn, x, y) {
            self.cycle_board_scope();
            return;
        }
        if rect_contains(self.regions.sync_btn, x, y) {
            self.request_sync();
            return;
        }
        if rect_contains(self.regions.sort_btn, x, y) {
            self.cycle_sort_order();
            return;
        }
        if rect_contains(self.regions.due_btn, x, y) {
            self.toggle_due_focus();
            return;
        }
        if rect_contains(self.regions.add_btn, x, y) {
            self.open_add_modal();
            return;
        }
        if rect_contains(self.regions.help_btn, x, y) {
            self.mode = Mode::Help(0);
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
                            hover_before_id: None,
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

    fn drop_before_task_in_column(
        &self,
        target_status: Status,
        drag_task_id: u64,
        y: u16,
    ) -> Option<u64> {
        self.regions
            .kanban_cards
            .iter()
            .filter(|(id, s, _)| *s == target_status && *id != drag_task_id)
            .find(|(_, _, rect)| y < rect.y.saturating_add(rect.height / 2))
            .map(|(id, _, _)| *id)
    }

    fn compute_store_insert_index(
        &self,
        from_pos: usize,
        target_status: Status,
        before_id: Option<u64>,
        drag_task_id: u64,
    ) -> usize {
        if let Some(bid) = before_id {
            if let Some(bpos) = self.store.tasks.iter().position(|t| t.id == bid) {
                return if from_pos < bpos {
                    bpos.saturating_sub(1)
                } else {
                    bpos
                };
            }
        }
        // Insert after the last task in `target_status` (or at end of store)
        if let Some(last_pos) = self
            .store
            .tasks
            .iter()
            .rposition(|t| t.status == target_status && t.id != drag_task_id)
        {
            if from_pos < last_pos {
                last_pos
            } else {
                (last_pos + 1).min(self.store.tasks.len().saturating_sub(1))
            }
        } else {
            from_pos
        }
    }

    fn handle_left_drag(&mut self, x: u16, y: u16) {
        let hover = self.column_at(x, y);
        let drag_task_id = self.drag.as_ref().map(|d| d.task_id);
        let before_id = match (hover, drag_task_id) {
            (Some(status), Some(tid)) => self.drop_before_task_in_column(status, tid, y),
            _ => None,
        };
        if let Some(drag) = self.drag.as_mut() {
            drag.current_x = x;
            drag.current_y = y;
            let dist = drag.start_x.abs_diff(x) + drag.start_y.abs_diff(y);
            if dist >= 2 || hover.is_some_and(|s| s != drag.origin_status) {
                drag.active = true;
            }
            drag.hover_status = hover;
            drag.hover_before_id = before_id;
        }
    }

    fn handle_left_up(&mut self, x: u16, y: u16) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.active {
            if let Some(target) = self.column_at(x, y) {
                let before_id = self.drop_before_task_in_column(target, drag.task_id, y);
                if target != drag.origin_status {
                    self.push_undo();
                    let mut rolled_due: Option<String> = None;
                    if let Some(task) = self.store.get_mut(drag.task_id) {
                        if target == Status::Done && task.repeat.is_some() {
                            rolled_due = task.complete_if_recurring();
                        } else {
                            task.set_status(target);
                            task.touch();
                        }
                    }
                    if rolled_due.is_none() && self.sort_order == SortOrder::Manual {
                        if let Some(from_pos) =
                            self.store.tasks.iter().position(|t| t.id == drag.task_id)
                        {
                            let to_pos = self.compute_store_insert_index(
                                from_pos,
                                target,
                                before_id,
                                drag.task_id,
                            );
                            self.store.move_to_position(from_pos, to_pos);
                        }
                    }
                    self.persist();
                    self.clamp_selections();
                    self.select_task_by_id(drag.task_id);
                    if let Some(next_due) = rolled_due {
                        self.set_message(format!("Completed — next due {next_due}"));
                    } else {
                        self.set_message(format!("Moved to {}", target.label()));
                    }
                } else if self.sort_order != SortOrder::Manual {
                    self.set_message("Switch to manual sort (O) to reorder");
                } else if let Some(from_pos) =
                    self.store.tasks.iter().position(|t| t.id == drag.task_id)
                {
                    let to_pos =
                        self.compute_store_insert_index(from_pos, target, before_id, drag.task_id);
                    if to_pos != from_pos {
                        self.push_undo();
                        self.store.move_to_position(from_pos, to_pos);
                        self.persist();
                        self.clamp_selections();
                        self.select_task_by_id(drag.task_id);
                        self.set_message("Reordered task");
                    }
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
        store.add(
            "Many tags".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            tags,
        );
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

        // Redo delete with Ctrl-R
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered_todo_ids(), vec![2, 3, 4]);
    }

    #[test]
    fn readline_shortcuts_and_status_toast_expiry() {
        let mut input = TextInput::new("alpha beta gamma");
        input.handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            true,
        );
        assert_eq!(input.value, "alpha beta ");
        input.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            true,
        );
        assert_eq!(input.cursor, 0);
        input.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT), true);
        assert_eq!(input.cursor, 5);
        input.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            true,
        );
        assert_eq!(input.value, "alpha");
        input.handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            true,
        );
        assert_eq!(input.value, "");

        let mut app = sample_app();
        app.set_message("Transient");
        app.status_message_at =
            Some(Instant::now() - STATUS_TOAST_DURATION - Duration::from_secs(1));
        assert!(app.tick_status_message());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn config_defaults_and_sorting_work() {
        let cfg: AppConfig = toml::from_str(
            r#"
default_view = "kanban"
default_filter = "todo"
default_sort = "priority"
"#,
        )
        .unwrap();
        let mut store = TaskStore::new_empty();
        let id1 = store.add(
            "Low soon".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            Vec::new(),
        );
        let id2 = store.add(
            "High later".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
        );
        let id3 = store.add(
            "Med earliest".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        store.get_mut(id1).unwrap().due = Some("2026-05-02".into());
        store.get_mut(id2).unwrap().due = Some("2026-05-10".into());
        store.get_mut(id3).unwrap().due = Some("2026-05-01".into());

        let mut app = App::with_config(store, None, cfg);
        assert_eq!(app.view, View::Kanban);
        assert_eq!(app.todo_filter, TodoFilter::Todo);
        assert_eq!(app.sort_order, SortOrder::Priority);
        assert_eq!(app.column_task_ids(Status::Todo), vec![id2, id3, id1]);

        // Press 'O' to switch Priority -> Due
        app.handle_key(key(KeyCode::Char('O')));
        assert_eq!(app.sort_order, SortOrder::Due);
        assert_eq!(app.column_task_ids(Status::Todo), vec![id3, id1, id2]);
    }

    #[test]
    fn quick_add_shorthand_parses_tags_priority_due_and_repeat() {
        let mut app = sample_app();
        app.handle_key(key(KeyCode::Char('Q')));
        for ch in "Ship v0.3 #ops #release !high @2026-12-15 ~weekly".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));

        let created = app
            .store
            .tasks
            .iter()
            .find(|t| t.title == "Ship v0.3")
            .expect("quick-added task should exist");
        assert_eq!(created.priority, Priority::High);
        assert_eq!(created.tags, vec!["ops", "release"]);
        assert_eq!(created.due.as_deref(), Some("2026-12-15"));
        assert_eq!(created.repeat.as_deref(), Some("weekly"));
    }

    #[test]
    fn yank_copies_title_full_markdown_and_preview_notes() {
        let mut app = sample_app();
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.pending_yank.take().as_deref(), Some("Alpha"));

        app.handle_key(key(KeyCode::Char('Y')));
        let full = app.pending_yank.take().expect("should yank full task");
        assert!(full.contains("- [ ] Alpha #work"));
        assert!(full.contains("First task"));

        app.handle_key(key(KeyCode::Char('v')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.pending_yank.take().as_deref(), Some("First task"));
    }

    #[test]
    fn gx_extracts_and_opens_first_url_from_task_or_markdown() {
        assert_eq!(
            extract_first_url("See [PR](https://github.com/a/b/pull/42).").as_deref(),
            Some("https://github.com/a/b/pull/42")
        );
        assert_eq!(extract_first_url("no links here"), None);

        let mut store = TaskStore::new_empty();
        store.add(
            "Review PR".into(),
            "Check https://example.com/pr/123, please!".into(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        let mut app = App::new(store, None);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.pending_open_url.take().as_deref(),
            Some("https://example.com/pr/123")
        );
    }

    #[test]
    fn quick_due_edit_and_due_focus_filter() {
        let mut store = TaskStore::new_empty();
        let id_overdue = store.add(
            "Overdue".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
        );
        let id_today = store.add(
            "Today".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        let id_future = store.add(
            "Future".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            Vec::new(),
        );
        let id_done = store.add(
            "Done overdue".into(),
            String::new(),
            Status::Done,
            Priority::Low,
            Vec::new(),
        );
        store.get_mut(id_overdue).unwrap().due = Some("2020-01-01".into());
        store.get_mut(id_today).unwrap().due = Some(today_ymd());
        store.get_mut(id_future).unwrap().due = Some("2099-12-31".into());
        store.get_mut(id_done).unwrap().due = Some("2020-01-01".into());

        let mut app = App::new(store, None);
        // '@' focuses only non-done overdue + due-today tasks
        app.handle_key(key(KeyCode::Char('@')));
        assert!(app.due_focus);
        assert_eq!(app.filtered_todo_ids(), vec![id_overdue, id_today]);

        // 'D' opens task modal focused directly on Due field
        app.handle_key(key(KeyCode::Char('D')));
        if let Mode::TaskModal(ref form) = app.mode {
            assert_eq!(form.active_field, FormField::Due);
        } else {
            panic!("expected TaskModal on Due field");
        }
        app.handle_key(key(KeyCode::Esc));

        // Esc clears due_focus
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.due_focus);
        assert_eq!(app.filtered_todo_ids().len(), 4);
    }

    #[test]
    fn multi_select_marks_apply_batch_move_delete_and_single_undo() {
        let mut app = sample_app();
        // Mark task 1 and task 2 with 'm', 'm'
        app.handle_key(key(KeyCode::Char('m')));
        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.marked_ids, vec![1, 2]);

        // Batch toggle done with Space
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.store.get(1).unwrap().status, Status::Done);
        assert_eq!(app.store.get(2).unwrap().status, Status::Done);

        // Single undo restores both tasks
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.store.get(1).unwrap().status, Status::Todo);
        assert_eq!(app.store.get(2).unwrap().status, Status::Todo);

        // Batch delete removes both tasks and clears marks
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.store.tasks.len(), 2);
        assert!(app.marked_ids.is_empty());

        // Single undo restores both deleted tasks
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.store.tasks.len(), 4);
    }

    #[test]
    fn live_config_reload_updates_theme_and_editor() {
        let dir = std::env::temp_dir().join(format!("roke_cfg_reload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[theme]\npreset = \"nord\"\n").unwrap();

        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        let mut app =
            App::with_config_and_path(TaskStore::new_empty(), None, Some(cfg_path.clone()), cfg);
        assert_eq!(app.theme, Theme::preset("nord").unwrap());
        assert!(!app.reload_config_if_changed());

        std::fs::write(
            &cfg_path,
            "editor = \"helix\"\n[theme]\npreset = \"gruvbox\"\n",
        )
        .unwrap();
        assert!(app.reload_config_if_changed());
        assert_eq!(app.theme, Theme::preset("gruvbox").unwrap());
        assert_eq!(app.editor_label, "helix");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tui_archive_modal_browses_and_restores_tasks() {
        let dir = std::env::temp_dir().join(format!("roke_arch_tui_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("todos.jsonl");

        let mut store = TaskStore::new_empty();
        store.add(
            "Open".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        store.add(
            "Finished".into(),
            String::new(),
            Status::Done,
            Priority::Low,
            vec!["ship".into()],
        );
        store.save(&path).unwrap();

        let mut app = App::with_config(store, Some(path.clone()), AppConfig::default());
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.store.tasks.len(), 1);

        // Open archive browser with 'V'
        app.handle_key(key(KeyCode::Char('V')));
        assert!(matches!(app.mode, Mode::Archive { ref tasks, selected: 0 } if tasks.len() == 1));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Press Enter to restore "Finished"
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.store.tasks.len(), 2);
        assert!(app.store.tasks.iter().any(|t| t.title == "Finished"));

        let _ = std::fs::remove_dir_all(&dir);
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

        // Status filter pill counts must reflect active search query ("All 1", "To Do 1", "Done 0")
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let row_y = app.regions.filter_pills[0].1.y;
        let line: String = (0..80).map(|x| buf[(x, row_y)].symbol()).collect();
        assert!(
            line.contains("All 1"),
            "expected 'All 1' in filter bar, got: {line}"
        );
        assert!(
            line.contains("To Do 1"),
            "expected 'To Do 1' in filter bar, got: {line}"
        );
        assert!(
            line.contains("Done 0"),
            "expected 'Done 0' in filter bar, got: {line}"
        );
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
    fn kanban_vertical_drag_reorders_cards_within_column() {
        let mut app = sample_app();
        app.view = View::Kanban;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Initially Todo column has [1 (Alpha), 2 (Beta)]
        assert_eq!(app.column_task_ids(Status::Todo), vec![1, 2]);
        let (_, _, rect1) = app
            .regions
            .kanban_cards
            .iter()
            .copied()
            .find(|(id, _, _)| *id == 1)
            .unwrap();
        let (_, _, rect2) = app
            .regions
            .kanban_cards
            .iter()
            .copied()
            .find(|(id, _, _)| *id == 2)
            .unwrap();

        // Drag card 1 down past the bottom of card 2 in the same column
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect1.x + 1,
            row: rect1.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: rect2.x + 1,
            row: rect2.y + rect2.height,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: rect2.x + 1,
            row: rect2.y + rect2.height,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.column_task_ids(Status::Todo), vec![2, 1]);
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
        app.mode = Mode::Help(0);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.handle_key(key(KeyCode::Char('G')));
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        if let Mode::Help(scroll) = app.mode {
            assert!(scroll > 0, "small terminal help modal should scroll");
        } else {
            panic!("expected Help mode");
        }
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

        // Press 'n' in Normal mode to request opening selected task (frontmatter + notes) in $EDITOR
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.pending_editor, Some(PendingEditor::Task(1)));
        let target = app.pending_editor.take().unwrap();
        let mut doc = app.editor_initial_document(target).unwrap();
        assert_eq!(doc.description, "First task");
        assert_eq!(doc.title, "Alpha");

        doc.description = "# Alpha Notes\n- [ ] Step 1\n- [x] Step 2".into();
        doc.location = Some("Desk".into());
        doc.reminders = vec!["-15m".into()];
        app.apply_editor_result(target, Ok(Some(doc)));
        assert_eq!(
            app.store.get(1).unwrap().description,
            "# Alpha Notes\n- [ ] Step 1\n- [x] Step 2"
        );
        assert_eq!(app.store.get(1).unwrap().location.as_deref(), Some("Desk"));
        assert_eq!(app.store.get(1).unwrap().reminders, vec!["-15m"]);
        assert_eq!(app.store.get(1).unwrap().checklist_progress(), Some((1, 2)));

        // Open read-only Markdown notes preview with 'v' and toggle checklist interactively
        app.handle_key(key(KeyCode::Char('v')));
        assert!(matches!(
            app.mode,
            Mode::NotesPreview {
                task_id: 1,
                scroll: 0,
                selected_check: 0
            }
        ));
        let preview_backend = TestBackend::new(80, 24);
        let mut preview_terminal = Terminal::new(preview_backend).unwrap();
        preview_terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        // Space toggles item 0 (`- [ ] Step 1` -> `- [x] Step 1`)
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.store.get(1).unwrap().checklist_progress(), Some((2, 2)));
        assert!(app
            .store
            .get(1)
            .unwrap()
            .description
            .contains("- [x] Step 1"));
        // Tab moves to item 1 (`- [x] Step 2`) and Space unchecks it
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.store.get(1).unwrap().checklist_progress(), Some((1, 2)));
        assert!(app
            .store
            .get(1)
            .unwrap()
            .description
            .contains("- [ ] Step 2"));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));

        // Press 'e' then Ctrl-E inside modal to open task (frontmatter + notes) in $EDITOR
        app.handle_key(key(KeyCode::Char('e')));
        assert!(matches!(app.mode, Mode::TaskModal(_)));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.pending_editor, Some(PendingEditor::Modal));
        let modal_target = app.pending_editor.take().unwrap();
        let mut modal_doc = app.editor_initial_document(modal_target).unwrap();
        modal_doc.description = "# Updated\nMore lines".into();
        modal_doc.url = Some("https://example.com".into());
        app.apply_editor_result(modal_target, Ok(Some(modal_doc)));

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
        assert_eq!(
            app.store.get(1).unwrap().description,
            "# Updated\nMore lines"
        );
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        app.handle_key(key(KeyCode::Char('2')));
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn dual_board_merged_view_isolates_storage_tags_and_sync_jobs() {
        use crate::config::{PeerConfig, PeerKind};

        let dir = std::env::temp_dir().join(format!("roke_dual_board_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let proj_path = dir.join(".roke.jsonl");
        let glob_path = dir.join("global-todos.jsonl");

        let mut proj_store = TaskStore::new_empty();
        proj_store.add_with_origin(
            "Ship API".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["backend".into()],
            "laptop",
        );
        proj_store.save(&proj_path).unwrap();

        let mut glob_store = TaskStore::new_empty();
        let gid = glob_store.add_with_origin(
            "Buy groceries".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            vec!["home".into()],
            "laptop",
        );
        let glob_uid = glob_store.get(gid).unwrap().uid.clone();
        glob_store.save(&glob_path).unwrap();

        let proj_sync = SyncConfig {
            peers: vec![PeerConfig::minimal("team", PeerKind::Caldav)],
            ..SyncConfig::default()
        };
        let glob_sync = SyncConfig {
            peers: vec![PeerConfig::minimal("phone", PeerKind::Caldav)],
            ..SyncConfig::default()
        };

        let mut app = App::with_config(
            proj_store.clone(),
            Some(proj_path.clone()),
            AppConfig::default(),
        );
        app.attach_dual_board(DualBoard::new(
            "myapp".into(),
            BoardScope::Project,
            proj_path.clone(),
            glob_path.clone(),
            proj_store,
            glob_store,
            proj_sync,
            glob_sync,
        ));

        // Starts in Project scope (1 task)
        assert_eq!(app.board_scope_label().as_deref(), Some("myapp"));
        assert_eq!(app.store.tasks.len(), 1);
        assert_eq!(app.build_sync_jobs().len(), 1);

        // Press 'b' -> All scope (merged view shows both boards with dynamic board tags)
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.board_scope_label().as_deref(), Some("myapp+global"));
        assert_eq!(app.store.tasks.len(), 2);
        assert_eq!(app.build_sync_jobs().len(), 2);

        let glob_task = app
            .store
            .tasks
            .iter()
            .find(|t| t.uid == glob_uid)
            .unwrap()
            .clone();
        assert!(app
            .effective_tags(&glob_task)
            .contains(&"global".to_string()));
        assert!(!glob_task.tags.contains(&"global".to_string()));

        // Toggle the global task done while in merged All view
        app.toggle_task_done(glob_task.id);

        // Verify strict file isolation: global file updated, project file untouched!
        let proj_on_disk = TaskStore::load_or_empty(&proj_path).unwrap();
        let glob_on_disk = TaskStore::load_or_empty(&glob_path).unwrap();
        assert_eq!(proj_on_disk.tasks.len(), 1);
        assert_eq!(proj_on_disk.tasks[0].title, "Ship API");
        assert_eq!(glob_on_disk.tasks.len(), 1);
        assert_eq!(glob_on_disk.tasks[0].status, Status::Done);

        // Render merged header with board scope pill cleanly
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(app.regions.board_btn.width > 0);

        // Press 'b' -> Global scope -> 'b' -> back to Project scope
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.board_scope_label().as_deref(), Some("global"));
        assert_eq!(app.store.tasks.len(), 1);
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.board_scope_label().as_deref(), Some("myapp"));
        assert_eq!(app.store.tasks.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
