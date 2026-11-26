<div align="center">

# roke

**A quiet todo list and Kanban board for the terminal.**

Built in Rust with [Ratatui](https://ratatui.rs/) — Vim keybindings, mouse drag-and-drop, multi-tag filtering, JSONL storage, TOML theming, and peer-to-peer SSH sync.

</div>

<br />

## What is roke?

Most task trackers wrap a simple list in layers of chrome, accounts, and cloud lock-in. **roke** stays out of the way: clean rounded cards on your terminal's native background, a single hairline header, one-line footer, and keyboard controls shaped around how Vim moves through text.

Switch between a filterable **Todo List** and a three-column **Kanban Board** with a single keystroke or click. Every task lives in a plain `todos.jsonl` file you can grep, script, or commit to Git — and when you work across multiple machines, roke syncs directly over SSH with zero extra daemons or servers.

## Features

- **Two views, one store** — Switch seamlessly between a **Todo List** (`All`, `To Do`, `In Progress`, `Done`) and a three-column **Kanban Board** (`Tab`, `gt`, `1`/`2`, or click the header tabs).
- **Vim-first, mouse-friendly** — Navigate with `h`/`j`/`k`/`l`, `gg`/`G`, move cards across columns with `H`/`L` or `<`/`>`, reorder with `J`/`K`, undo with `u`, and search live with `/`. Click tabs, filters, checkboxes (`○`/`◐`/`✓`), double-click cards to edit, scroll with the wheel, and drag-and-drop cards across Kanban columns.
- **Zero-clutter multi-tag filtering** — Tag tasks (`work, personal`), filter by multiple tags at once via clicks or the `t` picker, and toggle between `and` (match all) and `or` (match any). Tags tuck quietly into card bottom borders and disappear completely when unused.
- **Plain JSONL storage & live reload** — Stored in `$XDG_DATA_HOME/roke/todos.jsonl` (`~/.local/share/roke/todos.jsonl`), one compact JSON object per line with atomic saves. The TUI watches for external changes and reloads live, merging safely on save.
- **Markdown notes in `$EDITOR`** — Task notes support full Markdown and open directly in `nvim` / `$VISUAL` / `$EDITOR` (or a configured `editor` in `config.toml`) via `n` in the TUI, `Ctrl-E` in the edit modal, or `roke notes <id>`.
- **Scriptable CLI & built-in MCP server** — Manage tasks from scripts or AI agents via `roke add`/`list`/`edit`/`notes`/`move`/`done`/`rm`/`tags`/`sync` (with `--json` and theme-aware colours), or connect any MCP client directly with `roke mcp`.
- **TOML config & theming** — Override any colour in `$XDG_CONFIG_HOME/roke/config.toml` using ANSI-16 names (`blue`, `dark_gray`, `reset`), 256-color indices, or `#rrggbb` hex values.
- **Conflict-free SSH sync** — Two-way background sync over system `ssh` (`BatchMode=yes`, honoring `~/.ssh/config`) using last-write-wins timestamps and tombstones. Works across `bash`, `zsh`, `sh`, and `fish` login shells, with an optional `inject_device_tag` setting to dynamically tag tasks with the machine they came from.

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
roke                                     # open interactive TUI (~/.local/share/roke/todos.jsonl)
roke --file ./project-todos.jsonl        # use a specific JSONL task file
roke --config ./config.toml              # use a specific TOML config file
roke --print-config                      # print a full starter config.toml to stdout
```

### CLI & agent usage

All subcommands accept `--file`, `--config`, `--json`, and `--color <auto|always|never>` (honoring your `[theme]` colours from `config.toml` and `NO_COLOR`), and accept either a numeric task ID or stable `uid`. Any edits appear live in an open `roke` TUI.

```bash
roke list --status todo --tag work --verbose
roke list --json
roke add "Ship release" -d "Tag v0.1.2 and publish" -p high -t work,release --json
roke edit 1 --add-tag urgent --priority high
roke notes 1                             # open task Markdown notes in $EDITOR / nvim
roke notes 1 --print                     # print raw Markdown notes to stdout
cat spec.md | roke notes 1 --stdin       # set Markdown notes from stdin
roke move 1 in-progress
roke done 1
roke rm 1
roke tags
roke sync --json

# Batch add from JSON array or JSONL on stdin
echo '[{"title":"Task A","priority":"high"},{"title":"Task B","tags":["ops"]}]' | roke add --stdin --json
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

Pass `["mcp", "--file", "/path/to/todos.jsonl"]` to point an agent at a project-specific board. Exposed tools: `list_tasks`, `get_task`, `add_task`, `update_task`, `delete_task`, `list_tags`, and `sync_tasks`, plus `roke://tasks` and `roke://tags` resources.

### Keybindings

| Key | Action |
|---|---|
| `j` / `k` or `↓` / `↑` | Move selection down / up |
| `h` / `l` or `←` / `→` | Switch column (Kanban) or status filter (Todos) |
| `gg` / `G` | Jump to first / last task |
| `Tab` / `gt` / `1` / `2` | Switch between Todo List and Kanban Board |
| `a` / `o` | New task |
| `e` / `i` / `Enter` | Edit selected task |
| `n` | Edit task's Markdown notes in `$EDITOR` (`Ctrl-E` inside modal) |
| `Space` | Toggle done (Todos) or cycle status (Kanban) |
| `H` / `L` or `<` / `>` | Move task to previous / next column |
| `J` / `K` | Reorder task down / up |
| `s` / `p` | Cycle status / cycle priority |
| `t` / `T` | Multi-tag filter picker / edit tags on selected task |
| `S` | Sync now via SSH |
| `d` / `x` | Delete selected task |
| `u` | Undo last change (up to 50 steps) |
| `f` | Cycle status filter (Todos) |
| `/` / `Esc` | Live search / clear search or tag filters |
| `?` | Toggle in-app shortcuts overlay |
| `q` / `Ctrl-C` | Quit |

### Mouse controls

- **Header**: Click `Todos`, `Kanban`, `↻ sync` (when peers are configured), `+ new`, or `?`.
- **Filters**: Click status filters, click `#tag` pills to toggle multiple tags, click `[and]`/`[or]` (when 2+ tags are active) to switch match mode, or click `+N` on narrow terminals to open the tag picker.
- **Cards**: Click to select, click the status glyph (`○`/`◐`/`✓`) in Todo List to toggle done, double-click to edit, scroll wheel to navigate, and **drag cards across columns** in Kanban view.
- **Modals**: Click fields to focus or cycle Status/Priority, click `Save ↵` / `Cancel`, or click outside the modal to close.

## Configuration

roke needs no configuration. To customize colours or enable SSH sync, create `~/.config/roke/config.toml` (only include what you want to override):

```toml
editor = "nvim"            # optional; overrides $VISUAL / $EDITOR for Markdown notes

[theme]
accent = "blue"            # ANSI-16 name ("blue", "dark_gray", "reset"), 0-255 index, or "#rrggbb"

[sync]
device_name = "macbook"    # optional; defaults to local hostname
inject_device_tag = false  # true dynamically adds #<peer> tags to remote tasks locally
auto_sync_on_start = true  # sync in the background on launch
auto_sync_on_save = false  # sync in the background after every edit

[[sync.peers]]
name = "server"
host = "server"            # user@host or an alias from ~/.ssh/config
# remote_path = "~/.local/share/roke/todos.jsonl"
# port = 22
# identity_file = "~/.ssh/id_ed25519"
# inject_device_tag = true # optional per-peer override
```

Run `roke --print-config` to see every available theme key and default value.

### SSH sync notes

- Sync uses non-interactive `ssh -o BatchMode=yes`, so key-based authentication (`ssh-agent` or an unencrypted key in `~/.ssh/config`) should already work for `ssh <host>`.
- By default (`inject_device_tag = false`), tasks from all machines merge seamlessly without extra tags. Enabling `inject_device_tag = true` (globally or per peer) dynamically injects `#<peer>` into the local UI and filters without modifying the remote file's stored `tags`.

## Storage format

Tasks live in `~/.local/share/roke/todos.jsonl` (one compact JSON object per line):

```json
{"id":1,"uid":"macbook-1764143800000-123-1","title":"Ship release","status":"Todo","priority":"High","tags":["work"],"updated_at":1764143800000,"origin":"macbook"}
{"uid":"macbook-1764143800000-123-2","deleted_at":1764143900000}
```

Minimal lines such as `{"title":"Buy milk"}` can also be appended directly from the shell and will be picked up automatically on load.

## The name

**roke** is named after Roke, the Isle of the Wise in Ursula K. Le Guin's *Earthsea*, as it appears on her [map of the archipelago](https://www.ursulakleguin.com/maps-and-drawings) (copyright The Ursula K. Le Guin Foundation, made available under [CC BY-NC-ND 4.0](https://creativecommons.org/licenses/by-nc-nd/4.0/)).
