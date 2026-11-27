<div align="center">

# roke

**A quiet todo list and Kanban board for the terminal.**

Built in Rust with [Ratatui](https://ratatui.rs/) — Vim keybindings, mouse drag-and-drop, multi-tag filtering, JSONL storage, TOML theming, and multi-backend sync (SSH, CalDAV / Tasks.org, Microsoft To Do, Google Tasks, WebDAV, and Syncthing/local files).

</div>

<br />

## What is roke?

Most task trackers wrap a simple list in layers of chrome, accounts, and cloud lock-in. **roke** stays out of the way: clean rounded cards on your terminal's native background, a single hairline header, one-line footer, and keyboard controls shaped around how Vim moves through text.

Switch between a filterable **Todo List** and a three-column **Kanban Board** with a single keystroke or click. Every task lives in a plain `todos.jsonl` file you can grep, script, or commit to Git — and when you work across multiple machines, roke syncs directly over SSH with zero extra daemons or servers.

## Features

- **Two views, one store** — Switch seamlessly between a **Todo List** (`All`, `To Do`, `In Progress`, `Done`) and a three-column **Kanban Board** (`Tab`, `gt`, `1`/`2`, or click the header tabs).
- **Vim-first, mouse-friendly** — Navigate with `h`/`j`/`k`/`l`, `gg`/`G`, quick-add with `Q`, move cards across columns with `H`/`L` or `<`/`>`, reorder with `J`/`K`, sort with `O`, yank with `y`/`Y`, undo/redo with `u`/`Ctrl-R`, archive/restore with `A`/`V`, and search live with `/`. Click tabs, filters, checkboxes (`○`/`◐`/`✓`), double-click cards to edit, scroll with the wheel, and drag-and-drop cards to reorder or move across Kanban columns.
- **Zero-clutter multi-tag filtering, natural due dates/times & recurrence** — Tag tasks (`work, personal`), set due dates and optional local times (`YYYY-MM-DD`, `today`, `tomorrow@14:30`, `+3d`, `+2w`, `fri 9:30am`, `eow`, `eom`) with local-time overdue highlighting and recurrence (`daily`, `weekdays`, `weekly`, `monthly`, `every 3d`), filter by multiple tags via clicks or the scrollable `t` picker, and toggle between `and` and `or`. Tags, due dates, and repeat rules tuck quietly into card bottom borders.
- **Plain JSONL storage, project-local boards & live reload** — Stored in `$XDG_DATA_HOME/roke/todos.jsonl` (`~/.local/share/roke/todos.jsonl`), or auto-discovered from a project-local `.roke.jsonl` (`roke init`), one compact JSON object per line with atomic locked saves. The TUI watches both `todos.jsonl` and `config.toml` and reloads live.
- **Interactive Markdown notes & checklists** — Task notes support full Markdown, show `[done/total]` checklist progress on cards, preview cleanly in-app with `v` (where you can toggle `- [ ]` / `- [x]` items directly with `Tab`/`Space` or click), and open in `nvim` / `$VISUAL` / `$EDITOR` via `n` in the TUI, `Ctrl-E` in the edit modal, or `roke notes <id>`.
- **Scriptable CLI & built-in MCP server** — Manage tasks from scripts or AI agents via `roke add`/`list`/`edit`/`notes`/`move`/`done`/`rm`/`tags`/`stats`/`archive`/`unarchive`/`gc`/`sync`/`init`/`completions` (supporting multi-ID batch `move`/`done`/`rm`, `--json`, and theme-aware colours), or connect any MCP client directly with `roke mcp` (supports `--read-only`).
- **TOML config, built-in theme presets & live reload** — Choose a built-in `[theme]` preset (`default`, `catppuccin`, `tokyo-night`, `gruvbox`, `nord`, `minimal`) or override any colour using ANSI-16 names, 256-color indices, or `#rrggbb` hex values, plus startup defaults (`default_view`, `default_filter`, `default_sort`). Edits to `config.toml` hot-reload in any open TUI.
- **Conflict-free multi-backend parallel sync** — Two-way parallel background sync across **SSH** (`BatchMode=yes`, honoring `~/.ssh/config`), **CalDAV** (Tasks.org Cloud, Radicale, Baikal, Nextcloud, Fastmail, iCloud), **Microsoft To Do** (Microsoft Graph + OAuth2 Device Code), **Google Tasks** (Tasks API v1 + OAuth2 PKCE, preserving local-only metadata), **WebDAV** (`ETag`/`If-Match` CAS), and **local/Syncthing folders** (auto-merging `*.sync-conflict-*.jsonl`). Sync state and OAuth tokens live cleanly under `$XDG_STATE_HOME/roke` (`~/.local/state/roke`).

## Installation

### Homebrew (macOS & Linux)

```bash
brew install aethar01/tap/roke
```

### From source

Requires Rust 1.75+ (Cargo).

```bash
cargo build --release
install -m 755 target/release/roke ~/.local/bin/roke
```

## Usage

```bash
roke                                     # open interactive TUI (auto-discovers .roke.jsonl or uses global board)
roke init                                # create a project-local .roke.jsonl in the current directory
roke --global                            # force the global board even inside a repo with .roke.jsonl
roke --file ./project-todos.jsonl        # use a specific JSONL task file
roke --config ./config.toml              # use a specific TOML config file
roke --print-config                      # print a full starter config.toml to stdout
roke completions zsh                     # generate shell completions (bash, zsh, fish, elvish, powershell)
```

### CLI & agent usage

All subcommands accept `--file`, `--local`, `--global`, `--config`, `--json`, and `--color <auto|always|never>` (honoring your `[theme]` colours from `config.toml` and `NO_COLOR`), and accept either a numeric task ID or stable `uid`. Any edits appear live in an open `roke` TUI.

```bash
roke list --status todo --tag work --sort priority --verbose
roke list --overdue --json
roke add "Ship release" -d "Tag release and publish" -p high -t work,release --due +3d --repeat weekly --json
roke add -Q "Weekly retro #team !med @fri ~weekly"   # quick-add shorthand
roke edit 1 --add-tag urgent --priority high --due eow
roke notes 1                             # open task Markdown notes in $EDITOR / nvim
roke notes 1 --print                     # print raw Markdown notes to stdout
cat spec.md | roke notes 1 --stdin       # set Markdown notes from stdin
roke move 1 2 in-progress                # move one or more tasks
roke done 1 2 3                          # mark one or more tasks done
roke rm 4 5                              # delete one or more tasks
roke tags
roke stats                               # compact board summary & 7-day velocity
roke stats --oneline                     # single-line segment for tmux / shell prompts (`○3 ◐1 !1`)
roke archive --dry-run                   # preview archiving Done tasks into archive.jsonl
roke archive                             # move Done tasks to archive.jsonl
roke archive --list                      # browse archived tasks
roke unarchive 1                         # restore archived task(s) back to the active board
roke gc --days 90                        # prune sync tombstones older than 90 days
roke auth microsoft                      # sign in to Microsoft To Do (device code) or Google Tasks (PKCE)
roke sync --json

# Batch add from JSON array or JSONL on stdin
echo '[{"title":"Task A","priority":"high","due":"today"},{"title":"Task B","tags":["ops"]}]' | roke add --stdin --json
```

### MCP server (`roke mcp`)

`roke` includes a zero-dependency stdio [Model Context Protocol](https://modelcontextprotocol.io/) server so AI agents (Claude Desktop, Claude Code, Cursor, T3 Code, etc.) can read and update your board directly:

```json
{
  "mcpServers": {
    "roke": {
      "command": "roke",
      "args": ["mcp"]
    }
  }
}
```

Pass `["mcp", "--file", "/path/to/todos.jsonl"]` to point an agent at a project-specific board, or `["mcp", "--read-only"]` to restrict an agent to read-only tools. Exposed tools (with MCP tool annotations): `list_tasks`, `get_task`, `add_task`, `add_tasks`, `update_task`, `delete_task`, `archive_done`, `list_archive`, `unarchive_task`, `list_tags`, `board_stats`, and `sync_tasks`, plus `roke://tasks`, `roke://tags`, and `roke://task/{selector}` resources.

### Keybindings

| Key | Action |
|---|---|
| `j` / `k` or `↓` / `↑` | Move selection down / up |
| `h` / `l` or `←` / `→` | Switch column (Kanban) or status filter (Todos) |
| `gg` / `G` | Jump to first / last task |
| `Tab` / `gt` / `1` / `2` | Switch between Todo List and Kanban Board |
| `a` / `o` | New task |
| `Q` | Quick-add bar (`title #tag !high @due ~repeat`) |
| `e` / `i` / `Enter` | Edit selected task |
| `v` | Preview task & interactive Markdown checklists (`Tab`/`Space` or click to toggle) |
| `n` | Edit task's Markdown notes in `$EDITOR` (`Ctrl-E` inside modal) |
| `Space` | Toggle done (Todos) or cycle status (Kanban) |
| `H` / `L` or `<` / `>` | Move task to previous / next column |
| `J` / `K` | Reorder task down / up |
| `s` / `p` | Cycle status / cycle priority |
| `O` | Cycle sort order (`manual` / `priority` / `due` / `updated`) |
| `t` / `T` | Multi-tag filter picker / edit tags on selected task |
| `D` / `@` | Edit due date / toggle due-today & overdue focus |
| `m` / `M` | Mark task / mark all visible or clear marks (batch `Space`/`H`/`L`/`p`/`d`/`y`) |
| `S` | Sync with configured peers |
| `d` / `x` | Delete selected (or marked) task(s) |
| `A` / `V` | Archive all Done tasks / Browse & restore archive |
| `y` / `Y` | Yank title (or notes in preview) / Yank full task as Markdown |
| `gx` | Open first URL in browser (`o` inside preview) |
| `u` / `Ctrl-R` | Undo / Redo last change (up to 50 steps) |
| `f` | Cycle status filter (Todos) |
| `/` / `Esc` | Smart search (`p:high due:overdue #tag -done`) / clear marks & filters |
| `?` | Toggle in-app shortcuts overlay |
| `q` / `Ctrl-C` | Quit |

### Mouse controls

- **Header**: Click `Todos`, `Kanban`, `↕ sort` (when sorted), `↻ sync` (when peers are configured), `+ new`, or `?`.
- **Filters**: Click status filters, click `#tag` pills to toggle multiple tags, click `[and]`/`[or]` (when 2+ tags are active) to switch match mode, or click `+N` on narrow terminals to open the scrollable tag picker.
- **Cards**: Click to select, click the status glyph (`○`/`◐`/`✓`) in Todo List to toggle done, double-click to edit, scroll wheel to navigate, and **drag cards to reorder vertically or move across columns** in Kanban view.
- **Modals**: Click fields to focus or cycle Status/Priority, click `Save ↵` / `Cancel`, or click outside the modal to close.

## Configuration

roke needs no configuration. To customize defaults, colours, or SSH sync, create `~/.config/roke/config.toml` (only include what you want to override):

```toml
editor = "nvim"            # optional; overrides $VISUAL / $EDITOR for Markdown notes
default_view = "todos"     # "todos" or "kanban"
default_filter = "all"     # "all", "todo", "in-progress", or "done"
default_sort = "manual"    # "manual", "priority", "due", or "updated"

[theme]
preset = "catppuccin"      # optional: "default", "catppuccin", "tokyo-night", "gruvbox", "nord", "minimal"
accent = "blue"            # override any key with ANSI-16 name, 0-255 index, or "#rrggbb"

[sync]
device_name = "macbook"    # optional; defaults to local hostname
inject_device_tag = false  # true dynamically adds #<peer> tags to remote tasks locally
auto_sync_on_start = true  # sync in the background on launch
auto_sync_on_save = false  # sync in the background after every edit

[[sync.peers]]
name = "server"
kind = "ssh"               # default; can be omitted
host = "server"            # user@host or an alias from ~/.ssh/config
# remote_path = "~/.local/share/roke/todos.jsonl"
# port = 22
# identity_file = "~/.ssh/id_ed25519"
# inject_device_tag = true # optional per-peer override

[[sync.peers]]
name = "phone"
kind = "caldav"            # works with Tasks.org Cloud, Radicale, Baikal, Nextcloud, Fastmail, etc.
url = "https://caldav.tasks.org/"  # server root, home-set, or direct list URL (auto-discovered)
list = "Default list"              # optional; defaults to first VTODO list
username = "user"
password_env = "ROKE_CALDAV_PASSWORD" # or password_cmd = "pass show caldav"

[[sync.peers]]
name = "cloud"
kind = "webdav"            # single todos.jsonl file over HTTP WebDAV with ETag CAS
url = "https://dav.example.com/roke/todos.jsonl"
username = "user"
password_env = "ROKE_WEBDAV_PASSWORD"

[[sync.peers]]
name = "syncthing"
kind = "file"              # local or Syncthing/Dropbox/iCloud synced file
path = "~/Sync/roke/todos.jsonl"

[[sync.peers]]
name = "microsoft"
kind = "ms-todo"           # Microsoft To Do via Graph API (run `roke auth microsoft` once)
client_id = "<azure-app-client-id>"
# tenant = "common"        # optional (default: "common" for personal + work accounts)
# list = "Tasks"           # optional; defaults to your default To Do list

[[sync.peers]]
name = "google"
kind = "google-tasks"      # Google Tasks API v1 (run `roke auth google` once)
client_id = "<google-desktop-oauth-client-id>"
client_secret_env = "ROKE_GOOGLE_CLIENT_SECRET" # or client_secret_cmd = "..."
# list = "My Tasks"        # optional; defaults to @default
```

Run `roke --print-config` to see every available theme key and default value.

### Sync backend notes

- **State & tokens (`$XDG_STATE_HOME/roke`)**: Machine-local sync cursors, cached discovery URLs, and OAuth refresh tokens (`0600`) live in `$XDG_STATE_HOME/roke` (`~/.local/state/roke`), keeping Git repos and Syncthing folders clean. Project-local `.roke.jsonl` boards automatically get isolated state subdirs under `boards/<hash>/`.
- **SSH (`kind = "ssh"`, default)**: Uses non-interactive `ssh -o BatchMode=yes` with `cksum` compare-and-swap, so key-based authentication (`ssh-agent` or `~/.ssh/config`) should already work for `ssh <host>`.
- **CalDAV (`kind = "caldav"`)**: Syncs against any RFC 4791 CalDAV server (**Tasks.org Cloud**, Radicale, Baikal, Nextcloud, Fastmail, iCloud) for native mobile apps like **Tasks.org** on Android or Reminders on iOS. Supports RFC 6764 auto-discovery (`/.well-known/caldav` → `current-user-principal` → `calendar-home-set` → `list` or first `VTODO` collection) and caches the resolved list URL. `roke` patches existing VTODOs in place so phone-only fields (`VALARM` reminders, geofences, subtasks, complex `RRULE`s) are preserved verbatim, and tracks last-seen UIDs so deletions on your phone propagate cleanly. Archiving a Done task in `roke` deletes it from CalDAV; unarchiving pushes it back.
- **Microsoft To Do (`kind = "ms-todo"`)**: Syncs title, notes, all 3 statuses (`notStarted`/`inProgress`/`completed`), priority, tags (`categories`), due dates, reminder times, and recurrence via Microsoft Graph. Authenticate once with `roke auth <peer>` (Device Code flow — works over SSH too).
- **Google Tasks (`kind = "google-tasks"`)**: Syncs title, notes, completion state, and due dates via Google Tasks API v1. Authenticate once with `roke auth <peer>` (localhost PKCE flow). Because Google's API lacks priority, tags, `in-progress`, due times, and recurrence, `roke` preserves those local-only fields in `$XDG_STATE_HOME/roke` across remote edits.
- **WebDAV (`kind = "webdav"`)**: Syncs a single remote `todos.jsonl` file with `ETag` / `If-Match` compare-and-swap.
- **File / Syncthing (`kind = "file"`)**: Merges with a local synced `todos.jsonl` file using `FileLock` + `cksum` CAS. If Syncthing creates `*.sync-conflict-*.jsonl` files next to your board (either your main board or a `file` peer), `roke` automatically merges and cleans them up.
- **Credentials**: CalDAV/WebDAV peers accept `password_env`, `password_cmd` (e.g. `pass`, `op`, `security`), or `password`. OAuth peers use `roke auth <peer>` (or an external `token_cmd`).
- By default (`inject_device_tag = false`), tasks from all peers merge seamlessly without extra tags. Enabling `inject_device_tag = true` (globally or per peer) dynamically injects `#<peer>` into the local UI and filters without modifying remote `tags`.

## Storage format

Tasks live in `~/.local/share/roke/todos.jsonl` (one compact JSON object per line):

```json
{"id":1,"uid":"macbook-1764143800000-123-1","title":"Ship release","status":"Todo","priority":"High","tags":["work"],"updated_at":1764143800000,"origin":"macbook"}
{"uid":"macbook-1764143800000-123-2","deleted_at":1764143900000}
```

Minimal lines such as `{"title":"Buy milk"}` can also be appended directly from the shell and will be picked up automatically on load.

## The name

**roke** is named after Roke, the Isle of the Wise in Ursula K. Le Guin's *Earthsea*, as it appears on her [map of the archipelago](https://www.ursulakleguin.com/maps-and-drawings) (copyright The Ursula K. Le Guin Foundation, made available under [CC BY-NC-ND 4.0](https://creativecommons.org/licenses/by-nc-nd/4.0/)).
