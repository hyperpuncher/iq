use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::borrow::Cow;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "iq", version = "0.1.0", about = "Interactive jq REPL")]
struct Args {
    /// JSON file to query (reads stdin if omitted)
    file: Option<PathBuf>,

    /// Indentation size for JSON output
    #[arg(short = 'i', long, default_value_t = 4)]
    indent: usize,
}

fn main() -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout()
            .execute(LeaveAlternateScreen)
            .and_then(|s| s.execute(DisableMouseCapture));
        original_hook(info);
    }));

    let args = Args::parse();

    // Show full help when run without args and no pipe
    if args.file.is_none() && std::io::stdin().is_terminal() {
        let _ = Args::command().print_help();
        println!();
        return Ok(());
    }

    let raw = if let Some(ref path) = args.file {
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?
    } else {
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .context("failed to read stdin")?;
        buf
    };

    if raw.is_empty() {
        anyhow::bail!("no input");
    }

    // Strip BOM and trailing whitespace for clean parsing
    let raw_str = String::from_utf8_lossy(&raw);
    let trimmed = raw_str.trim();
    // Re-parse as bytes for simdjson (preserves raw number formatting)
    let clean = trimmed.as_bytes().to_vec();

    let padded = qj::simdjson::pad_buffer(&clean);
    let value = qj::simdjson::dom_parse_to_value(&padded, clean.len()).with_context(|| {
        format!(
            "failed to parse JSON: bytes={}, first={:?}",
            clean.len(),
            &clean[..clean.len().min(40)]
        )
    })?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let raw_preview = String::from_utf8_lossy(&clean[..clean.len().min(60)]).to_string();
    let filename = args
        .file
        .as_ref()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "(stdin)".into());
    let mut app = App::new(value, raw_preview, filename, args.indent);
    terminal.draw(|f| ui(f, &mut app))?;

    while app.running {
        let mut redraw = false;

        // Poll wakes on input OR when the timeout fires. The timeout lets us
        // pick up eval-thread results via poll_eval() even when the user is
        // idle; 20ms is well under a frame and halves idle wakeups vs 8ms.
        if event::poll(Duration::from_millis(20))? {
            // Handle all pending events — input changes are instant
            loop {
                let ev = event::read()?;
                handle_event(&mut app, ev);
                if !app.running || !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
            redraw = true;
        }

        if app.poll_eval() {
            redraw = true;
        }

        if redraw {
            terminal.draw(|f| ui(f, &mut app))?;
        }
    }

    disable_raw_mode()?;
    io::stdout()
        .execute(DisableMouseCapture)?
        .execute(LeaveAlternateScreen)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct EvalResult {
    text: String,
    count: usize,
    error: Option<String>,
    duration: Duration,
}

struct App {
    raw_preview: String,
    json_value: Arc<qj::value::Value>,
    filename: String,
    indent_size: usize,
    result_count: usize,
    eval_duration: Option<Duration>,
    input: String,
    cursor: usize,
    result_text: String,
    /// Byte offset of the start of each display line. line_count = len.
    line_offsets: Vec<u32>,
    error: Option<String>,
    scroll: usize,
    max_scroll: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    popup: Option<Popup>,
    show_debug: bool,
    running: bool,
    /// Evaluator thread
    eval_tx: mpsc::Sender<String>,
    eval_rx: mpsc::Receiver<EvalResult>,
}

struct Popup {
    items: Vec<String>,
    selected: usize,
    scroll_off: usize,
}

fn spawn_eval_thread(
    value: Arc<qj::value::Value>,
    indent_size: usize,
    input_rx: mpsc::Receiver<String>,
    result_tx: mpsc::Sender<EvalResult>,
) {
    thread::spawn(move || {
        while let Ok(mut input) = input_rx.recv() {
            // Drain any newer inputs — only process the latest
            while let Ok(newer) = input_rx.try_recv() {
                input = newer;
            }

            let start = Instant::now();
            let trimmed = input.trim().to_string();
            let val = &*value;

            let (text, count, error) = if trimmed.is_empty() {
                let count = match val {
                    qj::value::Value::Array(a) => a.len(),
                    _ => 1,
                };
                (render_value(val, indent_size), count, None)
            } else {
                match qj::filter::parse(&trimmed) {
                    Ok(filter) => {
                        let mut outputs = Vec::new();
                        qj::filter::eval::eval_filter(&filter, val, &mut |v| {
                            outputs.push(v);
                        });
                        let err = qj::filter::eval::take_last_error()
                            .map(|e| format!("runtime error: {e:?}"));
                        let count = outputs.len();
                        let text = if outputs.is_empty() {
                            String::new()
                        } else {
                            outputs
                                .iter()
                                .map(|v| render_value(v, indent_size))
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        (text, count, err)
                    }
                    Err(e) => (String::new(), 0, Some(format!("{e}"))),
                }
            };

            let _ = result_tx.send(EvalResult {
                text,
                count,
                error,
                duration: start.elapsed(),
            });
        }
    });
}

impl App {
    fn new(
        value: qj::value::Value,
        raw_preview: String,
        filename: String,
        indent_size: usize,
    ) -> Self {
        let value = Arc::new(value);
        let (eval_tx, input_rx) = mpsc::channel::<String>();
        let (res_tx, eval_rx) = mpsc::channel::<EvalResult>();
        spawn_eval_thread(value.clone(), indent_size, input_rx, res_tx);

        let mut app = Self {
            raw_preview,
            json_value: value,
            filename,
            indent_size,
            result_count: 0,
            eval_duration: None,
            input: String::new(),
            cursor: 0,
            result_text: String::new(),
            line_offsets: Vec::new(),
            error: None,
            scroll: 0,
            max_scroll: 0,
            history: load_history(),
            history_idx: None,
            popup: None,
            show_debug: false,
            running: true,
            eval_tx,
            eval_rx,
        };
        app.eval_sync();
        app.open_completion();
        app
    }

    /// Synchronous eval — for initial load (empty input, just renders the value).
    fn eval_sync(&mut self) {
        let val = &*self.json_value;
        self.result_count = match val {
            qj::value::Value::Array(a) => a.len(),
            _ => 1,
        };
        self.result_text = render_value(val, self.indent_size);
        self.line_offsets = compute_offsets(&self.result_text);
        self.error = None;
        self.eval_duration = Some(Duration::ZERO);
        self.scroll = 0;
    }

    /// Send current input to the eval thread for async processing.
    fn eval_async(&mut self) {
        let _ = self.eval_tx.send(self.input.clone());
    }

    /// After an edit that changed `self.input`: re-eval + re-open completions.
    fn after_edit(&mut self) {
        self.eval_async();
        self.open_completion();
    }

    /// After a cursor-only move: completions depend on position, but the
    /// filter text is unchanged so there's nothing to re-evaluate.
    fn after_cursor(&mut self) {
        self.open_completion();
    }

    /// Check if eval results arrived. Returns true if new results were applied.
    fn poll_eval(&mut self) -> bool {
        let mut updated = false;
        while let Ok(result) = self.eval_rx.try_recv() {
            self.result_text = result.text;
            self.line_offsets = compute_offsets(&self.result_text);
            self.result_count = result.count;
            self.error = result.error;
            self.eval_duration = Some(result.duration);
            self.scroll = 0;
            updated = true;
        }
        updated
    }
    // -- Input editing --

    fn insert_char(&mut self, c: char) {
        self.popup = None;
        self.history_idx = None;
        self.input.insert(self.cursor, c);
        self.cursor += 1;
        self.after_edit();
    }

    fn delete_before(&mut self) {
        self.popup = None;
        self.history_idx = None;
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
            self.after_edit();
        }
    }

    fn delete_after(&mut self) {
        self.popup = None;
        self.history_idx = None;
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
            self.after_edit();
        }
    }

    // -- History --

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            Some(i) if i > 0 => i - 1,
            None => self.history.len() - 1,
            _ => return,
        };
        self.history_idx = Some(idx);
        self.input = self.history[idx].clone();
        self.cursor = self.input.len();
        self.popup = None;
        self.after_edit();
    }

    fn history_down(&mut self) {
        match self.history_idx {
            Some(i) if i < self.history.len() - 1 => {
                self.history_idx = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor = self.input.len();
                self.popup = None;
                self.after_edit();
            }
            Some(_) => {
                self.history_idx = None;
                self.input.clear();
                self.cursor = 0;
                self.popup = None;
                self.after_edit();
            }
            None => {}
        }
    }

    fn push_history(&mut self) {
        let trimmed = self.input.trim().to_string();
        let is_new = !trimmed.is_empty() && self.history.last() != Some(&trimmed);
        if is_new {
            self.history.push(trimmed.clone());
            append_history(&trimmed);
        }
        self.history_idx = None;
    }

    // -- Tab completion --

    fn open_completion(&mut self) {
        let cursor = self.cursor.min(self.input.len());
        if cursor > 0 && self.input.as_bytes()[cursor - 1] == b')' {
            return;
        }
        // Find innermost unclosed `(` before cursor — cursor is inside a function call
        let mut stack: Vec<usize> = Vec::new();
        for (i, c) in self.input[..cursor].char_indices() {
            match c {
                '(' => stack.push(i),
                ')' => {
                    stack.pop();
                }
                _ => {}
            }
        }
        // If inside parens, use only the content after the last unclosed `(`
        let local = match stack.last() {
            Some(&paren) => &self.input[paren + 1..cursor],
            None => &self.input[..cursor],
        };
        // Inside function calls, the effective root is the array element (not the array itself)
        let inside_parens = stack.last().is_some();
        let root: &qj::value::Value = if inside_parens {
            match self.json_value.as_ref() {
                qj::value::Value::Array(arr) => {
                    arr.first().unwrap_or_else(|| self.json_value.as_ref())
                }
                _ => self.json_value.as_ref(),
            }
        } else {
            self.json_value.as_ref()
        };
        let items = get_completions(root, local);
        // Inside parens at expression start, prefix all keys with `.`
        let items: Vec<String> = if inside_parens && local.is_empty() {
            items
                .into_iter()
                .map(|k| {
                    if !k.starts_with('.') {
                        format!(".{k}")
                    } else {
                        k
                    }
                })
                .collect()
        } else {
            items
        };
        if items.is_empty() {
            return;
        }
        self.popup = Some(Popup {
            items,
            selected: 0,
            scroll_off: 0,
        });
    }

    /// Cycle the completion popup if open, otherwise open it.
    fn cycle_or_open(&mut self) {
        if self.popup.is_some() {
            self.cycle_completion(true);
        } else {
            self.open_completion();
        }
    }

    fn cycle_completion(&mut self, forward: bool) {
        match self.popup {
            Some(ref mut p) => {
                let n = p.items.len();
                if forward {
                    p.selected = (p.selected + 1) % n;
                } else {
                    p.selected = if p.selected == 0 {
                        n - 1
                    } else {
                        p.selected - 1
                    };
                }
                // Auto-scroll: keep selected item visible
                let max_visible = 5usize;
                if p.selected < p.scroll_off {
                    p.scroll_off = p.selected;
                } else if p.selected >= p.scroll_off + max_visible {
                    p.scroll_off = p.selected - max_visible + 1;
                }
            }
            None => {
                self.open_completion();
            }
        }
    }

    fn accept_completion(&mut self) {
        if let Some(p) = self.popup.take()
            && let Some(key) = p.items.get(p.selected)
        {
            let cursor = self.cursor.min(self.input.len());
            if key.starts_with('.') || key.starts_with('[') {
                // Dot/array-prefixed completions — append at cursor
                self.input.insert_str(cursor, key);
                self.cursor = cursor + key.len();
            } else {
                // Bare field name — replace word at cursor
                let ws = word_start_at(&self.input, cursor);
                let before = &self.input[..ws];
                let after = &self.input[cursor..];
                let mut new = String::with_capacity(before.len() + key.len() + after.len());
                new.push_str(before);
                new.push_str(key);
                new.push_str(after);
                self.input = new;
                self.cursor = ws + key.len();
            }
            self.history_idx = None;
            self.after_edit();
        }
    }

    // -- Scrolling --

    fn scroll_page_up(&mut self, page: usize) {
        self.scroll = self.scroll.saturating_sub(page);
    }

    fn scroll_page_down(&mut self, page: usize) {
        self.scroll = (self.scroll + page).min(self.max_scroll);
    }
}

// ---------------------------------------------------------------------------
// JSON → styled lines
// ---------------------------------------------------------------------------

/// Color scheme expressed as SGR escapes, one per JSON token category.
/// Distinct codes per category so the SGR parser can map each back to a Style.
const COLORS: qj::output::ColorScheme = qj::output::ColorScheme {
    null: "\x1b[90m",            // DarkGray
    bool_val: "\x1b[33m",        // Yellow
    number: "\x1b[33m",          // Yellow
    string: "\x1b[32m",          // Green
    array_bracket: "\x1b[1;37m", // White + bold
    object_brace: "\x1b[1;37m",  // White + bold
    object_key: "\x1b[1;36m",    // Cyan + bold
    reset: "\x1b[0m",
};

/// Render a JSON value to styled lines via qj's own serializer, then parse
/// the embedded ANSI SGR codes back into ratatui `Span`s.
/// Serialize a value to ANSI-colored pretty JSON text (no trailing newline).
fn render_value(value: &qj::value::Value, size: usize) -> String {
    use qj::output::{OutputConfig, OutputMode, write_value};
    let config = OutputConfig {
        mode: OutputMode::Pretty,
        indent: " ".repeat(size),
        join_output: true, // suppress trailing newline
        color: COLORS,
        ..OutputConfig::default()
    };
    let mut buf = Vec::new();
    write_value(&mut buf, value, &config).ok();
    String::from_utf8(buf).unwrap_or_default()
}

/// Byte offset of the start of each line. Empty for empty text.
fn compute_offsets(text: &str) -> Vec<u32> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v = vec![0u32];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            v.push((i + 1) as u32);
        }
    }
    v
}

/// Parse the visible window of the serialized result into styled lines.
/// Windowed rendering: we never build a `Vec<Line>` for the whole output,
/// only the ~height lines on screen.
fn visible_lines(text: &str, offsets: &[u32], scroll: usize, n: usize) -> Vec<Line<'static>> {
    if scroll >= offsets.len() {
        return Vec::new();
    }
    let end = (scroll + n).min(offsets.len());
    let last = offsets.len();
    let mut out = Vec::with_capacity(end - scroll);
    for i in scroll..end {
        let start = offsets[i] as usize;
        let line_end = if i + 1 < last {
            offsets[i + 1] as usize - 1 // drop the '\n'
        } else {
            text.len()
        };
        let line = &text[start..line_end];
        out.push(parse_ansi_line(line));
    }
    out
}

/// Parse one line containing `\x1b[...m` SGR codes into a ratatui `Line`.
fn parse_ansi_line(s: &str) -> Line<'static> {
    let bytes = s.as_bytes();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    let mut start = 0;
    let mut style = Style::default();
    while i < bytes.len() {
        // SGR escape: ESC '[' params 'm'. 0x1b never starts a UTF-8 continuation byte.
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if i > start {
                spans.push(Span::styled(s[start..i].to_string(), style));
            }
            let rest = &s[i + 2..];
            match rest.find('m') {
                Some(e) => {
                    style = sgr_to_style(&rest[..e]);
                    let next = i + 2 + e + 1;
                    i = next;
                    start = next;
                }
                None => break,
            }
        } else {
            i += 1;
        }
    }
    if start < s.len() {
        spans.push(Span::styled(s[start..].to_string(), style));
    }
    if spans.is_empty() {
        Line::raw(String::new())
    } else {
        Line::from(spans)
    }
}

/// Map a single SGR parameter string to a ratatui `Style`.
fn sgr_to_style(code: &str) -> Style {
    match code {
        "90" => Style::new().fg(Color::DarkGray),
        "33" => Style::new().fg(Color::Yellow),
        "32" => Style::new().fg(Color::Green),
        "1;37" => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        "1;36" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        _ => Style::default(),
    }
}

/// Resolve a path against a Value using qj's own filter engine.
/// Resolve a dotted field path like `a.b.c` against a Value by direct tree walk.
///
/// For an array root, descends into the first element at each step (so
/// completion context mirrors what `map(...)` / `select(...)` see).
///
/// ponytail: hand-rolled walk instead of `qj::filter::eval_filter` so completion
/// context lookup is O(depth × siblings) with zero cloning — recomputing via the
/// filter engine cloned every array element and blocked the event loop.
fn resolve_path<'a>(value: &'a qj::value::Value, path: &str) -> Cow<'a, qj::value::Value> {
    use qj::value::Value;
    let mut cur = value;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        // A segment may carry a field name followed by `[]` iterations:
        // `a`, `[]`, `a[]`, `a[][]` ... Split off the field, then descend
        // once per `[]` (into the first element, matching completion context).
        let (field, rest) = match seg.find("[]") {
            Some(i) => (&seg[..i], &seg[i..]),
            None => (seg, ""),
        };
        if !field.is_empty() {
            match cur {
                Value::Object(obj) => match obj.iter().find(|(k, _)| k == field) {
                    Some((_, v)) => cur = v,
                    None => return Cow::Owned(Value::Null),
                },
                _ => return Cow::Owned(Value::Null),
            }
        }
        let mut r = rest;
        while let Some(rr) = r.strip_prefix("[]") {
            match cur {
                Value::Array(a) => match a.first() {
                    Some(v) => cur = v,
                    None => return Cow::Owned(Value::Null),
                },
                _ => return Cow::Owned(Value::Null),
            }
            r = rr;
        }
    }
    Cow::Borrowed(cur)
}

// -- Persistent filter history at $XDG_CACHE_HOME/iq/history (or ~/.cache) --

fn history_path() -> Option<PathBuf> {
    let cache = dirs::cache_dir()?;
    Some(cache.join("iq").join("history"))
}

fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn append_history(line: &str) {
    let Some(path) = history_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Common jq builtins for auto-completion.
fn jq_builtins() -> &'static [&'static str] {
    &[
        "empty",
        "length",
        "keys",
        "has",
        "map",
        "select",
        "del",
        "add",
        "sort",
        "reverse",
        "unique",
        "contains",
        "startswith",
        "endswith",
        "tonumber",
        "tostring",
        "type",
        "null",
        "true",
        "false",
    ]
}

/// Get sorted field names from an object or first-object-in-array.
fn object_keys(value: &qj::value::Value) -> Vec<String> {
    let mut keys: Vec<String> = object_key_refs(value).map(String::from).collect();
    keys.sort();
    keys
}

/// Borrowed field name iterator — no allocation per field.
fn object_key_refs(value: &qj::value::Value) -> Box<dyn Iterator<Item = &str> + '_> {
    use qj::value::Value;
    let iter: Box<dyn Iterator<Item = &str> + '_> = match value {
        Value::Object(obj) => Box::new(obj.iter().map(|(k, _)| k.as_str())),
        Value::Array(arr) => match arr.first() {
            Some(Value::Object(obj)) => Box::new(obj.iter().map(|(k, _)| k.as_str())),
            _ => Box::new(std::iter::empty()),
        },
        _ => Box::new(std::iter::empty()),
    };
    iter
}

/// Get completions based on current input and JSON value.
/// Find the start of the current word at cursor (walk back past word chars).
fn word_start_at(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    input[..cursor]
        .rfind(['.', '(', '|', ' '])
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn get_completions(value: &qj::value::Value, input: &str) -> Vec<String> {
    use qj::value::Value;

    if input.is_empty() {
        let mut result: Vec<String> = match value {
            Value::Object(_) => object_keys(value)
                .into_iter()
                .map(|k| format!(".{k}"))
                .collect(),
            Value::Array(_) => vec![".[]".to_string()],
            _ => Vec::new(),
        };
        for b in jq_builtins() {
            if !result.iter().any(|x| x == b) {
                result.push(b.to_string());
            }
        }
        return result;
    }

    if input == "." {
        return match value {
            Value::Object(_) => object_keys(value),
            Value::Array(_) => vec!["[]".to_string()],
            _ => Vec::new(),
        };
    }

    // Ends with dot: suggest keys of the value at that path
    if let Some(prefix) = input.strip_suffix('.') {
        let ctx: &qj::value::Value = if prefix.is_empty() || prefix.contains(['(', ')', '|', ' ']) {
            value
        } else {
            &resolve_path(value, prefix)
        };
        return object_keys(ctx);
    }

    // Split context path + partial field name at last dot
    let last_dot = input.rfind('.');
    let (prefix, partial) = match last_dot {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => ("", input),
    };

    let ctx: &qj::value::Value = if prefix.is_empty() || prefix.contains(['(', ')', '|', ' ']) {
        value
    } else {
        &resolve_path(value, prefix)
    };

    if partial.is_empty() {
        let mut all_keys = object_keys(ctx);
        // At expression start (no dotted path), include jq builtins
        if prefix.is_empty() && !input.starts_with('.') {
            for b in jq_builtins() {
                if !all_keys.iter().any(|x| x == b) {
                    all_keys.push(b.to_string());
                }
            }
        }
        return all_keys;
    }

    // Filter by partial — avoid cloning all keys, filter borrowed refs first
    let matched: Vec<String> = object_key_refs(ctx)
        .filter(|k| k.starts_with(partial) && *k != partial)
        .map(String::from)
        .collect();

    if !matched.is_empty() {
        return matched;
    }

    // If prefix isn't a clean path, fallback to trimmed partial
    if prefix.contains(['(', ')', '|', ' ']) {
        let cleaned = partial.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        return if cleaned.is_empty() {
            Vec::new()
        } else {
            object_key_refs(ctx)
                .filter(|k| k.starts_with(cleaned))
                .map(String::from)
                .collect()
        };
    }

    // Exact match on a field — resolve the full path and suggest its child keys
    let path = if prefix.is_empty() { partial } else { input };
    let field_val = &resolve_path(value, path);
    if matches!(&**field_val, qj::value::Value::Array(_)) {
        vec!["[]".to_string()]
    } else {
        object_keys(field_val)
            .into_iter()
            .map(|k| format!(".{k}"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();

    // Layout: optional popup, results, input
    let popup_active = app.popup.is_some();
    let input_h = 4u16;
    let popup_h = if popup_active {
        let total = app.popup.as_ref().unwrap().items.len();
        let vis = total.min(5) as u16;
        let more = if total > 5 { 1 } else { 0 };
        vis + more + 2 // items + "N more" + borders
    } else {
        0
    };

    // Build vertical layout
    let mut layout_items = Vec::new();

    // Debug overlay (when F1 is pressed)
    if app.show_debug {
        layout_items.push(Constraint::Length(6));
    }

    // Results area
    layout_items.push(Constraint::Min(1));

    // Popup
    if popup_active {
        layout_items.push(Constraint::Length(popup_h));
    }

    // Input
    layout_items.push(Constraint::Length(input_h));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(layout_items)
        .split(area);

    let mut ci = 0usize;

    if app.show_debug {
        let text = vec![
            Line::from(Span::styled(
                format!("raw: {:?}", app.raw_preview),
                Style::default(),
            )),
            Line::from(Span::styled(
                format!(
                    "type: {}, completions: {}",
                    app.json_value.type_name(),
                    get_completions(app.json_value.as_ref(), &app.input).len()
                ),
                Style::default(),
            )),
            Line::from(Span::styled(
                format!("input: {:?}", app.input),
                Style::default(),
            )),
            Line::from(Span::styled(
                format!("popup: {}", app.popup.is_some()),
                Style::default(),
            )),
        ];
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" Debug (F1) ")),
            chunks[ci],
        );
        ci += 1;
    }

    let results_area = chunks[ci];
    ci += 1;
    let input_area = if popup_active {
        chunks[ci + 1]
    } else {
        chunks[ci]
    };

    // --- Results (windowed: only parse the visible lines) ---
    let total = app.line_offsets.len();
    let visible = results_area.height as usize;
    app.max_scroll = total.saturating_sub(visible);
    app.scroll = app.scroll.min(app.max_scroll);

    let lines = visible_lines(&app.result_text, &app.line_offsets, app.scroll, visible);
    let results = Paragraph::new(lines).block(Block::default());
    f.render_widget(results, results_area);

    // --- Completion popup ---
    if let Some(ref popup) = app.popup {
        let popup_area = chunks[ci];
        let max_visible = 5usize;
        let end = (popup.scroll_off + max_visible).min(popup.items.len());
        let visible = &popup.items[popup.scroll_off..end];
        let mut popup_lines: Vec<Line> = visible
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let idx = popup.scroll_off + i;
                let selected = idx == popup.selected;
                let style = if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let prefix = if selected { "> " } else { "  " };
                Line::from(Span::styled(format!("{prefix}{item}"), style))
            })
            .collect();
        let remaining = popup.items.len() - end;
        if remaining > 0 {
            popup_lines.push(Line::from(Span::styled(
                format!("  … {} more", remaining),
                Style::default().fg(Color::DarkGray),
            )));
        }
        let popup_widget = Paragraph::new(popup_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Keys ")
                .border_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(popup_widget, popup_area);
    }

    // --- Input line ---
    let input_prompt = make_input_prompt(app, input_area.width);
    let input_widget = Paragraph::new(input_prompt).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(input_widget, input_area);
}

fn make_input_prompt(app: &App, width: u16) -> Vec<Line<'static>> {
    let error_suffix = app
        .error
        .as_ref()
        .map(|e| format!("  ({e})"))
        .unwrap_or_default();

    let before: String = app.input.chars().take(app.cursor).collect();
    let after: String = app.input.chars().skip(app.cursor).collect();
    let cur_char = after.chars().next().unwrap_or(' ');

    let input_color = if app.error.is_some() {
        Color::Red
    } else {
        Color::Green
    };

    let mut spans = vec![Span::styled("> ", Style::default().fg(input_color))];

    if !before.is_empty() {
        spans.push(Span::raw(before));
    }
    spans.push(Span::styled(
        cur_char.to_string(),
        Style::default().add_modifier(Modifier::REVERSED),
    ));
    if after.len() > 1 {
        spans.push(Span::raw(after[1..].to_string()));
    }

    let sep = "─".repeat(width as usize);
    let mut lines = vec![
        Line::from(spans),
        Line::from(Span::styled(sep, Style::default().fg(Color::DarkGray))),
    ];

    // Status line: filename left, result info right
    if error_suffix.is_empty() {
        let info = match (&app.eval_duration, app.result_count) {
            (Some(d), 0) => format!("{}ms", d.as_micros().max(1) / 1000),
            (Some(d), n) => format!("{} results • {}ms", n, d.as_micros().max(1) / 1000),
            (None, n) if n > 0 => format!("{n} results"),
            _ => String::new(),
        };
        if info.is_empty() {
            lines.push(Line::from(Span::styled(
                app.filename.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let fw = app.filename.chars().count() as u16;
            let iw = info.chars().count() as u16;
            let pad = width.saturating_sub(fw + iw + 1).max(1) as usize;
            let status = format!("{} {:pad$}{}", app.filename, "", info, pad = pad);
            lines.push(Line::from(Span::styled(
                status,
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            error_suffix,
            Style::default().fg(Color::Red),
        )));
    }

    lines
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

fn handle_event(app: &mut App, event: Event) {
    match event {
        Event::Key(key) => match key.code {
            // Debug: F1 shows/hides debug overlay (top_keys, state)
            KeyCode::F(1) => {
                app.show_debug = !app.show_debug;
            }

            // Quit
            KeyCode::Char('c') | KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                app.running = false;
            }
            KeyCode::Esc => {
                if app.popup.is_some() {
                    app.popup = None;
                } else {
                    app.running = false;
                }
            }

            // Tab completion
            KeyCode::Tab => app.cycle_or_open(),
            KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') if app.popup.is_some() => {
                app.accept_completion();
            }
            KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => {
                if app.input.is_empty() {
                    app.open_completion();
                } else {
                    app.push_history();
                }
            }

            // Input editing
            KeyCode::Backspace => app.delete_before(),
            KeyCode::Delete => app.delete_after(),
            KeyCode::Left => {
                app.cursor = app.cursor.saturating_sub(1);
                app.after_cursor();
            }
            KeyCode::Right => {
                app.cursor = (app.cursor + 1).min(app.input.len());
                app.after_cursor();
            }
            KeyCode::Home => {
                app.cursor = 0;
                app.after_cursor();
            }
            KeyCode::End => {
                app.cursor = app.input.len();
                app.after_cursor();
            }

            KeyCode::Char(ch) => {
                if ch == '\t' {
                    app.cycle_or_open();
                } else {
                    app.insert_char(ch);
                }
            }

            // Popup navigation
            KeyCode::Up if app.popup.is_some() => app.cycle_completion(false),
            KeyCode::Down if app.popup.is_some() => app.cycle_completion(true),

            // History (Up/Down when input is empty)
            KeyCode::Up if app.input.is_empty() => app.history_up(),
            KeyCode::Down if app.input.is_empty() => app.history_down(),

            // Paging
            KeyCode::PageUp => app.scroll_page_up(10),
            KeyCode::PageDown => app.scroll_page_down(10),

            _ => {}
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.scroll = app.scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                app.scroll = (app.scroll + 3).min(app.max_scroll);
            }
            _ => {}
        },
        Event::Resize(_, _) => {}
        _ => {}
    }
}
