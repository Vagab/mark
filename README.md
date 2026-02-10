# mark

```
 __  __            _
|  \/  | __ _ _ __| | __
| |\/| |/ _` | '__| |/ /
| |  | | (_| | |  |   <
|_|  |_|\__,_|_|  |_|\_\
```

A fast, minimal Markdown reader and editor for the terminal.

## Features

- Markdown preview pane with live reload
- Vim-style editing (normal/insert/visual)
- Outline of headings
- Search with highlights
- Theme picker (bat/syntect themes)
- Discover mode for finding Markdown files

## Install

### From source

```bash
cargo build --release
```

The binary will be at `target/release/mark`.

## Usage

```bash
mark README.md
```

Open discover mode (choose a file):

```bash
mark
```

Edit config:

```bash
mark config
```

Install bat themes:

```bash
mark themes install bat
```

List available themes:

```bash
mark themes list
```

## Keymap (normal mode)

- `j/k` or arrows: move
- `gg` / `G`: top / bottom
- `/` then Enter: search
- `n` / `N`: next / prev match
- `[` / `]`: prev / next heading
- `Shift+B`: toggle preview pane
- `Ctrl+B`: full preview
- `Alt+Left/Right`: resize preview
- `H`: toggle outline
- `t`: theme picker
- `?`: help
- `:w` / `:q` / `:wq`: save / quit
- `Ctrl+P` or `:open`: discover files

## Config

Config is stored in `~/.config/mark/config.toml`.

```toml
theme = "Monokai Extended"
show_outline = true
outline_width = 28
wrap = true
search_case_sensitive = false
bat_theme_dir = "~/.config/bat/themes"
tab_width = 4
forced_discover_dirs = ["~/.claude", "./.claude"]
preview_ratio = 55
```

## Notes

- Code highlighting uses syntect.
- Themes are compatible with bat.

## License

MIT. See `LICENSE`.
