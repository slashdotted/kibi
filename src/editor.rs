#![allow(clippy::wildcard_imports)]

use std::env;
use std::io::{
    self, BufRead, BufReader, ErrorKind::InvalidInput, ErrorKind::NotFound, Read, Seek, Write,
};
use std::iter::{self, repeat, successors};
use std::sync::{Arc, RwLock};
use std::{fmt::Display, fs::File, path::Path, process::Command, thread, time::Instant};

use melda::{melda::Melda, memoryadapter::MemoryAdapter};
use serde_json::{json, Map, Value};
use url::Url;

use crate::row::{HlState, Row, UuidChar};
use crate::{ansi_escape::*, syntax::Conf as SyntaxConf, sys, terminal, Config, Error};

const fn ctrl_key(key: u8) -> u8 { key & 0x1f }
const EXIT: u8 = ctrl_key(b'Q');
const DELETE_BIS: u8 = ctrl_key(b'H');
const REFRESH_SCREEN: u8 = ctrl_key(b'L');
const REFRESH_REPLICA: u8 = ctrl_key(b'P');
const SAVE: u8 = ctrl_key(b'S');
const SAVE_AS: u8 = ctrl_key(b'N');
const FIND: u8 = ctrl_key(b'F');
const GOTO: u8 = ctrl_key(b'G');
const DUPLICATE: u8 = ctrl_key(b'D');
const EXECUTE: u8 = ctrl_key(b'E');
const REMOVE_LINE: u8 = ctrl_key(b'R');
const BACKSPACE: u8 = 127;

const HELP_MESSAGE: &str =
    "Ctrl-S = save | Ctrl-Q = quit | Ctrl-F = find | Ctrl-G = go to | Ctrl-D = duplicate | Ctrl-E = execute";

/// `set_status!` sets a formatted status message for the editor.
/// Example usage: `set_status!(editor, "{} written to {}", file_size, file_name)`
macro_rules! set_status {
    ($editor:expr, $($arg:expr),*) => ($editor.status_msg = Some(StatusMessage::new(format!($($arg),*))))
}

/// Enum of input keys
enum Key {
    Arrow(AKey),
    CtrlArrow(AKey),
    Page(PageKey),
    Home,
    End,
    Delete,
    Escape,
    Char(u8),
}

/// Enum of arrow keys
enum AKey {
    Left,
    Right,
    Up,
    Down,
}

/// Enum of page keys
enum PageKey {
    Up,
    Down,
}

/// Describes the cursor position and the screen offset
#[derive(Default, Clone)]
struct CursorState {
    /// x position (indexing the characters, not the columns)
    x: usize,
    /// y position (row number, 0-indexed)
    y: usize,
    /// Row offset
    roff: usize,
    /// Column offset
    coff: usize,
}

impl CursorState {
    fn move_to_next_line(&mut self) {
        self.y += 1;
        self.x = 0;
    }

    /// Scroll the terminal window vertically and horizontally (i.e. adjusting the row offset and
    /// the column offset) so that the cursor can be shown.
    fn scroll(&mut self, rx: usize, screen_rows: usize, screen_cols: usize) {
        self.roff = self.roff.clamp(self.y.saturating_sub(screen_rows.saturating_sub(1)), self.y);
        self.coff = self.coff.clamp(rx.saturating_sub(screen_cols.saturating_sub(1)), rx);
    }
}

enum AdapterReadyFor {
    LOADING,
    SAVING,
}

/// The `Editor` struct, contains the state and configuration of the text editor.
#[derive(Default)]
pub struct Editor {
    /// If not `None`, the current prompt mode (Save, Find, GoTo). If `None`, we are in regular
    /// edition mode.
    prompt_mode: Option<PromptMode>,
    /// The current state of the cursor.
    cursor: CursorState,
    /// The padding size used on the left for line numbering.
    ln_pad: usize,
    /// The width of the current window. Will be updated when the window is resized.
    window_width: usize,
    /// The number of rows that can be used for the editor, excluding the status bar and the message
    /// bar
    screen_rows: usize,
    /// The number of columns that can be used for the editor, excluding the part used for line numbers
    screen_cols: usize,
    /// The collection of rows, including the content and the syntax highlighting information.
    rows: Vec<Row>,
    /// Whether the document has been modified since it was open.
    dirty: bool,
    /// The configuration for the editor.
    config: Config,
    /// The number of warnings remaining before we can quit without saving. Defaults to
    /// `config.quit_times`, then decreases to 0.
    quit_times: usize,
    /// The file name. If None, the user will be prompted for a file name the first time they try to
    /// save.
    // TODO: It may be better to store a PathBuf instead
    file_name: Option<String>,
    /// The current status message being shown.
    status_msg: Option<StatusMessage>,
    /// The syntax configuration corresponding to the current file's extension.
    syntax: SyntaxConf,
    /// The number of bytes contained in `rows`. This excludes new lines.
    n_bytes: u64,
    /// The original terminal mode. It will be restored when the `Editor` instance is dropped.
    orig_term_mode: Option<sys::TermMode>,
    /// Local Document replica
    local_replica: Option<Melda>,
    /// Currently loaded url
    remote_url: Option<Url>,
    /// Remote Document replica
    remote_replica: Option<Melda>,
    /// Adapter status
    ready_for: Option<AdapterReadyFor>,
    // Username
    username: Option<String>,
    // Password
    password: Option<String>,
}

/// Describes a status message, shown at the bottom at the screen.
struct StatusMessage {
    /// The message to display.
    msg: String,
    /// The `Instant` the status message was first displayed.
    time: Instant,
}

impl StatusMessage {
    /// Create a new status message and set time to the current date/time.
    fn new(msg: String) -> Self { Self { msg, time: Instant::now() } }
}

/// Pretty-format a size in bytes.
fn format_size(n: u64) -> String {
    if n < 1024 {
        return format!("{}B", n);
    }
    // i is the largest value such that 1024 ^ i < n
    // To find i we compute the smallest b such that n <= 1024 ^ b and subtract 1 from it
    let i = (64 - n.leading_zeros() + 9) / 10 - 1;
    // Compute the size with two decimal places (rounded down) as the last two digits of q
    // This avoid float formatting reducing the binary size
    let q = 100 * n / (1024 << ((i - 1) * 10));
    format!("{}.{:02}{}B", q / 100, q % 100, b" kMGTPEZ"[i as usize] as char)
}

/// `slice_find` returns the index of `needle` in slice `s` if `needle` is a subslice of `s`,
/// otherwise returns `None`.
fn slice_find<T: PartialEq>(s: &[T], needle: &[T]) -> Option<usize> {
    (0..(s.len() + 1).saturating_sub(needle.len())).find(|&i| s[i..].starts_with(needle))
}

impl Editor {
    /// Initialize the text editor.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an error occurs when enabling termios raw mode, creating the signal hook
    /// or when obtaining the terminal window size.
    #[allow(clippy::field_reassign_with_default)] // False positive : https://github.com/rust-lang/rust-clippy/issues/6312
    pub fn new(config: Config) -> Result<Self, Error> {
        sys::register_winsize_change_signal_handler()?;
        let mut editor = Self::default();
        editor.initialize_local_replica();

        editor.quit_times = config.quit_times;
        editor.config = config;

        // Enable raw mode and store the original (non-raw) terminal mode.
        editor.orig_term_mode = Some(sys::enable_raw_mode()?);
        editor.update_window_size()?;

        set_status!(editor, "{}", HELP_MESSAGE);

        Ok(editor)
    }

    fn initialize_local_replica(&mut self) {
        // Initialize local replica
        let adapter = Box::new(MemoryAdapter::new());
        self.local_replica = Some(Melda::new(Arc::new(RwLock::new(adapter))).unwrap());
    }

    fn initialize_remote_replica(&mut self, url: &Url) {
        if self.remote_url.is_none() || self.remote_url.as_ref().unwrap().ne(&url) {
            let mut auth_url = url.clone();
            if let Some(u) = &self.username {
                auth_url.set_username(u).unwrap();
            }
            if let Some(p) = &self.password {
                auth_url.set_password(Some(p)).unwrap();
            }
            let adapter = melda::adapter::get_adapter(auth_url.as_str())
                .expect("cannot_initialize_remote_adapter");
            self.remote_replica = Some(Melda::new(Arc::new(RwLock::new(adapter))).unwrap());
            self.remote_url = Some(url.clone());
        } else if self.remote_replica.is_some() {
            self.remote_replica.as_mut().unwrap().reload().expect("cannot_reload");
        }
    }

    fn serialize(&self) -> Map<String, Value> {
        let mut rows = vec![];
        for (_, row) in self.rows.iter().enumerate() {
            let mut rowdata = Map::<String, Value>::new();
            rowdata.insert("_id".to_string(), Value::from(row.uuid.clone()));
            let mut rowchars = vec![];
            for c in &row.chars {
                let mut chardata = Map::<String, Value>::new();
                chardata.insert("#".to_string(), Value::from(c.0.to_string()));
                chardata.insert("_id".to_string(), Value::from(c.1.clone()));
                rowchars.push(Value::from(chardata));
            }
            rowdata.insert("\u{0394}c\u{266D}".to_string(), Value::from(rowchars));
            rows.push(Value::from(rowdata));
        }
        json!({ "\u{0394}rows\u{266D}": Value::from(rows) }).as_object().unwrap().clone()
    }

    fn deserialize(&mut self) -> u64 {
        let mut total = 0;
        match self.local_replica.as_ref().unwrap().read(None) {
            Ok(data) => {
                self.rows.clear();
                let rows = data.get("\u{0394}rows\u{266D}").unwrap().as_array().unwrap();
                for r in rows {
                    let row = r.as_object().unwrap();
                    let rid = row.get("_id").unwrap().as_str().unwrap();
                    let rowchars = row.get("\u{0394}c\u{266D}").unwrap().as_array().unwrap();
                    let mut chars = vec![];
                    for c in rowchars {
                        let chardata = c.as_object().unwrap();
                        let cvalue =
                            chardata.get("#").unwrap().as_str().unwrap().parse::<u8>().unwrap();
                        let cid = chardata.get("_id").unwrap().as_str().unwrap();
                        chars.push(UuidChar(cvalue, cid.to_string()));
                        total += 1;
                    }
                    self.rows.push(Row::new(chars, Some(rid.to_string())));
                }
                self.update_all_rows();
                self.update_screen_cols();
                self.n_bytes = self.rows.iter().map(|row| row.chars.len() as u64).sum();
            }
            Err(e) => set_status!(self, "Error: {}", e),
        }
        total
    }

    /// Return the current row if the cursor points to an existing row, `None` otherwise.
    fn current_row(&self) -> Option<&Row> { self.rows.get(self.cursor.y) }

    /// Return the position of the cursor, in terms of rendered characters (as opposed to
    /// `self.cursor.x`, which is the position of the cursor in terms of bytes).
    fn rx(&self) -> usize {
        self.current_row().map_or(0, |r| {
            let fpos = std::cmp::min(r.cx2rx.len() - 1, self.cursor.x);
            r.cx2rx[fpos]
        })
    }

    /// Move the cursor following an arrow key (← → ↑ ↓).
    fn move_cursor(&mut self, key: &AKey) {
        match (key, self.current_row()) {
            (AKey::Left, Some(row)) if self.cursor.x > 0 =>
                self.cursor.x -= row.get_char_size(
                    row.cx2rx[std::cmp::max(row.cx2rx.len() - 1, self.cursor.x - 1)] - 1,
                ),
            (AKey::Left, _) if self.cursor.y > 0 => {
                // ← at the beginning of the line: move to the end of the previous line. The x
                // position will be adjusted after this `match` to accommodate the current row
                // length, so we can just set here to the maximum possible value here.
                self.cursor.y -= 1;
                self.cursor.x = usize::MAX;
            }
            (AKey::Right, Some(row)) if self.cursor.x < row.chars.len() =>
                self.cursor.x += row.get_char_size(row.cx2rx[self.cursor.x]),
            (AKey::Right, Some(_)) => self.cursor.move_to_next_line(),
            // TODO: For Up and Down, move self.cursor.x to be consistent with tabs and UTF-8
            //  characters, i.e. according to rx
            (AKey::Up, _) if self.cursor.y > 0 => self.cursor.y -= 1,
            (AKey::Down, Some(_)) => self.cursor.y += 1,
            _ => (),
        }
        self.update_cursor_x_position();
    }

    /// Update the cursor x position. If the cursor y position has changed, the current position
    /// might be illegal (x is further right than the last character of the row). If that is the
    /// case, clamp `self.cursor.x`.
    fn update_cursor_x_position(&mut self) {
        self.cursor.x = self.cursor.x.min(self.current_row().map_or(0, |row| row.chars.len()));
    }

    /// Run a loop to obtain the key that was pressed. At each iteration of the loop (until a key is
    /// pressed), we listen to the `ws_changed` channel to check if a window size change signal has
    /// been received. When bytes are received, we match to a corresponding `Key`. In particular,
    /// we handle ANSI escape codes to return `Key::Delete`, `Key::Home` etc.
    fn loop_until_keypress(&mut self) -> Result<Key, Error> {
        loop {
            // Handle window size if a signal has be received
            if sys::has_window_size_changed() {
                self.update_window_size()?;
                self.refresh_screen()?;
            }
            let mut bytes = sys::stdin()?.bytes();
            // Match on the next byte received or, if the first byte is <ESC> ('\x1b'), on the next
            // few bytes.
            match bytes.next().transpose()? {
                Some(b'\x1b') => {
                    return Ok(match bytes.next().transpose()? {
                        Some(b @ (b'[' | b'O')) => match (b, bytes.next().transpose()?) {
                            (b'[', Some(b'A')) => Key::Arrow(AKey::Up),
                            (b'[', Some(b'B')) => Key::Arrow(AKey::Down),
                            (b'[', Some(b'C')) => Key::Arrow(AKey::Right),
                            (b'[', Some(b'D')) => Key::Arrow(AKey::Left),
                            (b'[' | b'O', Some(b'H')) => Key::Home,
                            (b'[' | b'O', Some(b'F')) => Key::End,
                            (b'[', mut c @ Some(b'0'..=b'8')) => {
                                let mut d = bytes.next().transpose()?;
                                if let (Some(b'1'), Some(b';')) = (c, d) {
                                    // 1 is the default modifier value. Therefore, <ESC>[1;5C is
                                    // equivalent to <ESC>[5C, etc.
                                    c = bytes.next().transpose()?;
                                    d = bytes.next().transpose()?;
                                }
                                match (c, d) {
                                    (Some(c), Some(b'~')) if c == b'1' || c == b'7' => Key::Home,
                                    (Some(c), Some(b'~')) if c == b'4' || c == b'8' => Key::End,
                                    (Some(b'3'), Some(b'~')) => Key::Delete,
                                    (Some(b'5'), Some(b'~')) => Key::Page(PageKey::Up),
                                    (Some(b'6'), Some(b'~')) => Key::Page(PageKey::Down),
                                    (Some(b'5'), Some(b'A')) => Key::CtrlArrow(AKey::Up),
                                    (Some(b'5'), Some(b'B')) => Key::CtrlArrow(AKey::Down),
                                    (Some(b'5'), Some(b'C')) => Key::CtrlArrow(AKey::Right),
                                    (Some(b'5'), Some(b'D')) => Key::CtrlArrow(AKey::Left),
                                    _ => Key::Escape,
                                }
                            }
                            (b'O', Some(b'a')) => Key::CtrlArrow(AKey::Up),
                            (b'O', Some(b'b')) => Key::CtrlArrow(AKey::Down),
                            (b'O', Some(b'c')) => Key::CtrlArrow(AKey::Right),
                            (b'O', Some(b'd')) => Key::CtrlArrow(AKey::Left),
                            _ => Key::Escape,
                        },
                        _ => Key::Escape,
                    });
                }
                Some(a) => return Ok(Key::Char(a)),
                None => continue,
            }
        }
    }

    /// Update the `screen_rows`, `window_width`, `screen_cols` and `ln_padding` attributes.
    fn update_window_size(&mut self) -> Result<(), Error> {
        let wsize = sys::get_window_size().or_else(|_| terminal::get_window_size_using_cursor())?;
        self.screen_rows = wsize.0.saturating_sub(2); // Make room for the status bar and status message
        self.window_width = wsize.1;
        self.update_screen_cols();
        Ok(())
    }

    /// Update the `screen_cols` and `ln_padding` attributes based on the maximum number of digits
    /// for line numbers (since the left padding depends on this number of digits).
    fn update_screen_cols(&mut self) {
        // The maximum number of digits to use for the line number is the number of digits of the
        // last line number. This is equal to the number of times we can divide this number by ten,
        // computed below using `successors`.
        let n_digits =
            successors(Some(self.rows.len()), |u| Some(u / 10).filter(|u| *u > 0)).count();
        let show_line_num = self.config.show_line_num && n_digits + 2 < self.window_width / 4;
        self.ln_pad = if show_line_num { n_digits + 2 } else { 0 };
        self.screen_cols = self.window_width.saturating_sub(self.ln_pad);
    }

    /// Given a file path, try to find a syntax highlighting configuration that matches the path
    /// extension in one of the config directories (`/etc/kibi/syntax.d`, etc.). If such a
    /// configuration is found, set the `syntax` attribute of the editor.
    fn select_syntax_highlight(&mut self, path: &Path) -> Result<(), Error> {
        let extension = path.extension().and_then(std::ffi::OsStr::to_str);
        if let Some(s) = extension.and_then(|e| SyntaxConf::get(e).transpose()) {
            self.syntax = s?;
        }
        Ok(())
    }

    /// Update a row, given its index. If `ignore_following_rows` is `false` and the highlight state
    /// has changed during the update (for instance, it is now in "multi-line comment" state, keep
    /// updating the next rows
    fn update_row(&mut self, y: usize, ignore_following_rows: bool) {
        let mut hl_state = if y > 0 { self.rows[y - 1].hl_state } else { HlState::Normal };
        for row in self.rows.iter_mut().skip(y) {
            let previous_hl_state = row.hl_state;
            hl_state = row.update(&self.syntax, hl_state, self.config.tab_stop);
            if ignore_following_rows || hl_state == previous_hl_state {
                return;
            }
            // If the state has changed (for instance, a multi-line comment started in this row),
            // continue updating the following rows
        }
    }

    /// Update all the rows.
    fn update_all_rows(&mut self) {
        let mut hl_state = HlState::Normal;
        for row in &mut self.rows {
            hl_state = row.update(&self.syntax, hl_state, self.config.tab_stop);
        }
    }

    /// Insert a byte at the current cursor position. If there is no row at the current cursor
    /// position, add a new row and insert the byte.
    fn insert_byte(&mut self, c: u8) {
        if let Some(row) = self.rows.get_mut(self.cursor.y) {
            row.chars.insert(self.cursor.x, UuidChar::new(c, None))
        } else {
            self.rows.push(Row::new(vec![UuidChar::new(c, None)], None));
            // The number of rows has changed. The left padding may need to be updated.
            self.update_screen_cols();
        }
        self.update_row(self.cursor.y, false);
        self.cursor.x += 1;
        self.n_bytes += 1;
        self.dirty = true;
    }

    /// Insert a new line at the current cursor position and move the cursor to the start of the new
    /// line. If the cursor is in the middle of a row, split off that row.
    fn insert_new_line(&mut self) {
        let (position, new_row_chars) = if self.cursor.x == 0 {
            (self.cursor.y, Vec::new())
        } else {
            // self.rows[self.cursor.y] must exist, since cursor.x = 0 for any cursor.y ≥ row.len()
            let new_chars = self.rows[self.cursor.y].chars.split_off(self.cursor.x);
            self.update_row(self.cursor.y, false);
            (self.cursor.y + 1, new_chars)
        };
        self.rows.insert(position, Row::new(new_row_chars, None));
        self.update_row(position, false);
        self.update_screen_cols();
        self.cursor.move_to_next_line();
        self.dirty = true;
    }

    /// Delete a character at the current cursor position. If the cursor is located at the beginning
    /// of a row that is not the first or last row, merge the current row and the previous row. If
    /// the cursor is located after the last row, move up to the last character of the previous row.
    fn delete_char(&mut self) {
        if self.cursor.x > 0 {
            let row = &mut self.rows[self.cursor.y];
            if self.cursor.x >= row.cx2rx.len() || row.cx2rx[self.cursor.x] <= 0 {
                self.cursor.x = 0;
                return;
            }
            let n_bytes_to_remove = row.get_char_size(row.cx2rx[self.cursor.x] - 1);
            row.chars.splice(self.cursor.x - n_bytes_to_remove..self.cursor.x, iter::empty());
            self.update_row(self.cursor.y, false);
            self.cursor.x -= n_bytes_to_remove;
            self.dirty = if self.is_empty() { self.file_name.is_some() } else { true };
            self.n_bytes -= n_bytes_to_remove as u64;
        } else if self.cursor.y < self.rows.len() && self.cursor.y > 0 {
            let row = self.rows.remove(self.cursor.y);
            let previous_row = &mut self.rows[self.cursor.y - 1];
            self.cursor.x = previous_row.chars.len();
            for c in row.chars {
                previous_row.chars.push(c);
            }
            self.update_row(self.cursor.y - 1, true);
            self.update_row(self.cursor.y, false);
            // The number of rows has changed. The left padding may need to be updated.
            self.update_screen_cols();
            self.dirty = true;
            self.cursor.y -= 1;
        } else if self.cursor.y == self.rows.len() {
            // If the cursor is located after the last row, pressing backspace is equivalent to
            // pressing the left arrow key.
            self.move_cursor(&AKey::Left);
        }
    }

    fn delete_current_row(&mut self) {
        if self.cursor.y < self.rows.len() {
            self.rows[self.cursor.y].chars.clear();
            self.update_row(self.cursor.y, false);
            self.cursor.move_to_next_line();
            self.delete_char();
        }
    }

    fn duplicate_current_row(&mut self) {
        if let Some(row) = self.current_row() {
            let new_row = Row::new_from_new_chars(row.chars.iter().map(|x| x.0).collect(), None);
            self.n_bytes += new_row.chars.len() as u64;
            self.rows.insert(self.cursor.y + 1, new_row);
            self.update_row(self.cursor.y + 1, false);
            self.cursor.y += 1;
            self.dirty = true;
            // The line number has changed
            self.update_screen_cols();
        }
    }

    /// Try to load a file. If found, load the rows and update the render and syntax highlighting.
    /// If not found, do not return an error.
    fn load(&mut self, path: &Path) -> Result<(), Error> {
        match Url::parse(path.to_str().unwrap()) {
            Ok(u) => {
                self.ready_for = Some(AdapterReadyFor::LOADING);
                if self.ensure_adapter_is_ready(&u) {
                    self.initialize_local_replica();
                    self.initialize_remote_replica(&u);
                    let dc = self.remote_replica.as_ref().unwrap();
                    self.local_replica.as_mut().unwrap().meld(dc).expect("meld_from_remote_failed");
                    self.local_replica.as_mut().unwrap().reload().expect("failed_to_reload");
                    let total = self.deserialize();
                    set_status!(self, "{} characters read", total);
                    Ok(())
                } else {
                    Ok(())
                }
            }
            Err(_) => {
                let ft = std::fs::metadata(path)?.file_type();
                if !(ft.is_file() || ft.is_symlink()) {
                    return Err(io::Error::new(InvalidInput, "Invalid input file type").into());
                }
                match File::open(path) {
                    Ok(file) => {
                        for line in BufReader::new(file).split(b'\n') {
                            self.rows.push(Row::new_from_new_chars(line?, None));
                        }
                        // If the file ends with an empty line or is empty, we need to append an empty row
                        // to `self.rows`. Unfortunately, BufReader::split doesn't yield an empty Vec in
                        // this case, so we need to check the last byte directly.
                        let mut file = File::open(path)?;
                        file.seek(io::SeekFrom::End(0))?;
                        if file.bytes().next().transpose()?.map_or(true, |b| b == b'\n') {
                            self.rows.push(Row::new(Vec::new(), None));
                        }
                        self.update_all_rows();
                        // The number of rows has changed. The left padding may need to be updated.
                        self.update_screen_cols();
                        self.n_bytes = self.rows.iter().map(|row| row.chars.len() as u64).sum();
                    }
                    Err(e) if e.kind() == NotFound => self.rows.push(Row::new(Vec::new(), None)),
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            }
        }
    }

    /// Save the text to a file, given its name.
    fn save(&mut self, file_name: &str) -> Result<String, io::Error> {
        match Url::parse(file_name) {
            Ok(u) => {
                self.ready_for = Some(AdapterReadyFor::SAVING);
                if self.ensure_adapter_is_ready(&u) {
                    // Update local replica from new content
                    if self.dirty {
                        let serialized_state = self.serialize();
                        self.local_replica
                            .as_mut()
                            .unwrap()
                            .update(serialized_state)
                            .expect("update_failed");
                    }
                    let commit_result =
                        self.local_replica.as_mut().unwrap().commit(None).expect("commit_failed");
                    self.initialize_remote_replica(&u);
                    // Update local replica to integrate remote changes
                    let melded = self
                        .local_replica
                        .as_mut()
                        .unwrap()
                        .meld(self.remote_replica.as_ref().unwrap())
                        .expect("meld_from_remote_failed");
                    if !melded.is_empty() {
                        // Something was obtained from the remote replica, reload
                        self.local_replica.as_mut().unwrap().reload().expect("reload_failed");
                        self.deserialize();
                    }
                    match commit_result {
                        Some(bid) => {
                            // Update remote replica
                            self.remote_replica
                                .as_mut()
                                .unwrap()
                                .meld(&self.local_replica.as_ref().unwrap())
                                .expect("meld_to_remote_failed");
                            Ok(bid.first().map_or("Nothing", |v| v).to_string())
                        }
                        None => Ok("Nothing".to_string()),
                    }
                } else {
                    Ok("".to_string())
                }
            }
            Err(_) => {
                let mut file = File::create(file_name)?;
                let mut written = 0;
                for (i, row) in self.rows.iter().enumerate() {
                    file.write_all(&row.chars.iter().map(|x| x.0).collect::<Vec<u8>>())?;
                    written += row.chars.len();
                    if i != (self.rows.len() - 1) {
                        file.write_all(&[b'\n'])?;
                        written += 1
                    }
                }
                file.sync_all()?;
                Ok(format_size(written as u64))
            }
        }
    }

    /// Save the text to a file and handle all errors. Errors and success messages will be printed
    /// to the status bar. Return whether the file was successfully saved.
    fn save_and_handle_io_errors(&mut self, file_name: &str) -> bool {
        let saved = self.save(file_name);
        // Print error or success message to the status bar
        match saved.as_ref() {
            Ok(w) => set_status!(self, "{} written to {}", w, file_name),
            Err(err) => set_status!(self, "Can't save! I/O error: {}", err),
        }
        // If save was successful, set dirty to false.
        self.dirty &= saved.is_err();
        saved.is_ok()
    }

    /// Save to a file after obtaining the file path from the prompt. If successful, the `file_name`
    /// attribute of the editor will be set and syntax highlighting will be updated.
    fn save_as(&mut self, file_name: String) -> Result<(), Error> {
        // TODO: What if file_name already exists?
        if self.save_and_handle_io_errors(&file_name) {
            // If save was successful
            self.select_syntax_highlight(Path::new(&file_name))?;
            self.file_name = Some(file_name);
            self.update_all_rows();
        }
        Ok(())
    }

    /// Draw the left part of the screen: line numbers and vertical bar.
    fn draw_left_padding<T: Display>(&self, buffer: &mut String, val: T) {
        if self.ln_pad >= 2 {
            // \x1b[38;5;240m: Dark grey color; \u{2502}: pipe "│"
            buffer.push_str(&format!("\x1b[38;5;240m{:>1$} \u{2502}", val, self.ln_pad - 2));
            buffer.push_str(RESET_FMT);
        }
    }

    /// Return whether the file being edited is empty or not. If there is more than one row, even if
    /// all the rows are empty, `is_empty` returns `false`, since the text contains new lines.
    fn is_empty(&self) -> bool { self.rows.len() <= 1 && self.n_bytes == 0 }

    /// Draw rows of text and empty rows on the terminal, by adding characters to the buffer.
    fn draw_rows(&self, buffer: &mut String) {
        let row_it = self.rows.iter().map(Some).chain(repeat(None)).enumerate();
        for (i, row) in row_it.skip(self.cursor.roff).take(self.screen_rows) {
            buffer.push_str(CLEAR_LINE_RIGHT_OF_CURSOR);
            if let Some(row) = row {
                // Draw a row of text
                self.draw_left_padding(buffer, i + 1);
                row.draw(self.cursor.coff, self.screen_cols, buffer);
            } else {
                // Draw an empty row
                self.draw_left_padding(buffer, '~');
                if self.is_empty() && i == self.screen_rows / 3 {
                    let welcome_message = concat!("Kibi ", env!("KIBI_VERSION"));
                    buffer.push_str(&format!("{:^1$.1$}", welcome_message, self.screen_cols));
                }
            }
            buffer.push_str("\r\n");
        }
    }

    /// Draw the status bar on the terminal, by adding characters to the buffer.
    fn draw_status_bar(&self, buffer: &mut String) {
        // Left part of the status bar
        let modified = if self.dirty { " (modified)" } else { "" };
        let mut left =
            format!("{:.30}{}", self.file_name.as_deref().unwrap_or("[No Name]"), modified);
        left.truncate(self.window_width);

        // Right part of the status bar
        let size = format_size(self.n_bytes + self.rows.len().saturating_sub(1) as u64);
        let right =
            format!("{} | {} | {}:{}", self.syntax.name, size, self.cursor.y + 1, self.rx() + 1);

        // Draw
        let rw = self.window_width.saturating_sub(left.len());
        buffer.push_str(&format!("{}{}{:>4$.4$}{}\r\n", REVERSE_VIDEO, left, right, RESET_FMT, rw));
    }

    /// Draw the message bar on the terminal, by adding characters to the buffer.
    fn draw_message_bar(&self, buffer: &mut String) {
        buffer.push_str(CLEAR_LINE_RIGHT_OF_CURSOR);
        let msg_duration = self.config.message_dur;
        if let Some(sm) = self.status_msg.as_ref().filter(|sm| sm.time.elapsed() < msg_duration) {
            buffer.push_str(&sm.msg[..sm.msg.len().min(self.window_width)]);
        }
    }

    /// Refresh the screen: update the offsets, draw the rows, the status bar, the message bar, and
    /// move the cursor to the correct position.
    fn refresh_screen(&mut self) -> Result<(), Error> {
        self.cursor.scroll(self.rx(), self.screen_rows, self.screen_cols);
        let mut buffer = format!("{}{}", HIDE_CURSOR, MOVE_CURSOR_TO_START);
        self.draw_rows(&mut buffer);
        self.draw_status_bar(&mut buffer);
        self.draw_message_bar(&mut buffer);
        let (cursor_x, cursor_y) = if self.prompt_mode.is_none() {
            // If not in prompt mode, position the cursor according to the `cursor` attributes.
            (self.rx() - self.cursor.coff + 1 + self.ln_pad, self.cursor.y - self.cursor.roff + 1)
        } else {
            // If in prompt mode, position the cursor on the prompt line at the end of the line.
            (self.status_msg.as_ref().map_or(0, |sm| sm.msg.len() + 1), self.screen_rows + 2)
        };
        // Finally, print `buffer` and move the cursor
        print!("{}\x1b[{};{}H{}", buffer, cursor_y, cursor_x, SHOW_CURSOR);
        io::stdout().flush().map_err(Error::from)
    }

    /// Process a key that has been pressed, when not in prompt mode. Returns whether the program
    /// should exit, and optionally the prompt mode to switch to.
    fn process_keypress(&mut self, key: &Key) -> (bool, Option<PromptMode>) {
        // This won't be mutated, unless key is Key::Character(EXIT)
        let mut quit_times = self.config.quit_times;
        let mut prompt_mode = None;

        match key {
            // TODO: CtrlArrow should move to next word
            Key::Arrow(arrow) | Key::CtrlArrow(arrow) => self.move_cursor(arrow),
            Key::Page(PageKey::Up) => {
                self.cursor.y = self.cursor.roff.saturating_sub(self.screen_rows);
                self.update_cursor_x_position();
            }
            Key::Page(PageKey::Down) => {
                self.cursor.y = (self.cursor.roff + 2 * self.screen_rows - 1).min(self.rows.len());
                self.update_cursor_x_position();
            }
            Key::Home => self.cursor.x = 0,
            Key::End => self.cursor.x = self.current_row().map_or(0, |row| row.chars.len()),
            Key::Char(b'\r' | b'\n') => self.insert_new_line(), // Enter
            Key::Char(BACKSPACE | DELETE_BIS) => self.delete_char(), // Backspace or Ctrl + H
            Key::Char(REMOVE_LINE) => self.delete_current_row(),
            Key::Delete => {
                self.move_cursor(&AKey::Right);
                self.delete_char();
            }
            Key::Escape | Key::Char(REFRESH_SCREEN) => (),
            Key::Char(REFRESH_REPLICA) => {
                if self.remote_replica.is_some() {
                    // Update local replica from new content
                    let serialized_state = self.serialize();
                    self.local_replica
                        .as_mut()
                        .unwrap()
                        .update(serialized_state)
                        .expect("update_failed");
                    // Save the current stage
                    let stage =
                        self.local_replica.as_ref().unwrap().stage().expect("cannot_get_stage");
                    self.local_replica.as_mut().unwrap().unstage().expect("cannot_unstage");
                    // Update local replica to integrate remote changes
                    self.local_replica
                        .as_mut()
                        .unwrap()
                        .meld(self.remote_replica.as_ref().unwrap())
                        .expect("meld_from_remote_failed");
                    self.local_replica.as_mut().unwrap().refresh().expect("refresh_failed");
                    // Replay stage
                    self.local_replica
                        .as_mut()
                        .unwrap()
                        .replay_stage(&stage)
                        .expect("failed_to_replay_stage");
                    self.deserialize();
                }
            }
            Key::Char(EXIT) => {
                quit_times = self.quit_times - 1;
                if !self.dirty || quit_times == 0 {
                    return (true, None);
                }
                let times = if quit_times > 1 { "times" } else { "time" };
                set_status!(self, "Press Ctrl+Q {} more {} to quit.", quit_times, times);
            }
            Key::Char(SAVE) => match self.file_name.take() {
                // TODO: Can we avoid using take() then reassigning the value to file_name?
                Some(file_name) => {
                    self.save_and_handle_io_errors(&file_name);
                    self.file_name = Some(file_name);
                }
                None => prompt_mode = Some(PromptMode::Save(String::new())),
            },
            Key::Char(FIND) =>
                prompt_mode = Some(PromptMode::Find(String::new(), self.cursor.clone(), None)),
            Key::Char(GOTO) => prompt_mode = Some(PromptMode::GoTo(String::new())),
            Key::Char(DUPLICATE) => self.duplicate_current_row(),
            Key::Char(EXECUTE) => prompt_mode = Some(PromptMode::Execute(String::new())),
            Key::Char(SAVE_AS) => prompt_mode = Some(PromptMode::Save(String::new())),
            Key::Char(c) => self.insert_byte(*c),
        }
        self.quit_times = quit_times;
        (false, prompt_mode)
    }

    /// Try to find a query, this is called after pressing Ctrl-F and for each key that is pressed.
    /// `last_match` is the last row that was matched, `forward` indicates whether to search forward
    /// or backward. Returns the row of a new match, or `None` if the search was unsuccessful.
    #[allow(clippy::trivially_copy_pass_by_ref)] // This Clippy recommendation is only relevant on 32 bit platforms.
    fn find(&mut self, query: &str, last_match: &Option<usize>, forward: bool) -> Option<usize> {
        let num_rows = self.rows.len();
        let mut current = last_match.unwrap_or_else(|| num_rows.saturating_sub(1));
        // TODO: Handle multiple matches per line
        for _ in 0..num_rows {
            current = (current + if forward { 1 } else { num_rows - 1 }) % num_rows;
            let row = &mut self.rows[current];
            if let Some(cx) =
                slice_find(&row.chars.iter().map(|x| x.0).collect::<Vec<u8>>(), query.as_bytes())
            {
                self.cursor.y = current as usize;
                self.cursor.x = cx;
                // Try to reset the column offset; if the match is after the offset, this
                // will be updated in self.cursor.scroll() so that the result is visible
                self.cursor.coff = 0;
                let rx = row.cx2rx[cx];
                row.match_segment = Some(rx..rx + query.len());
                return Some(current);
            }
        }
        None
    }

    fn ensure_adapter_is_ready(&mut self, u: &Url) -> bool {
        if u.scheme().starts_with("solid") && u.username().is_empty() && self.username.is_none() {
            self.prompt_mode = Some(PromptMode::Username(String::new()));
            false
        } else {
            true
        }
    }

    /// If `file_name` is not None, load the file. Then run the text editor.
    ///
    /// # Errors
    ///
    /// Will Return `Err` if any error occur.
    pub fn run(&mut self, file_name: &Option<String>) -> Result<(), Error> {
        if let Some(path) = file_name.as_ref().map(|p| sys::path(p.as_str())) {
            self.select_syntax_highlight(path.as_path())?;
            self.load(path.as_path())?;
            self.file_name = Some(path.to_string_lossy().to_string());
        } else {
            self.rows.push(Row::new(Vec::new(), None));
            self.file_name = None;
        }
        loop {
            if let Some(mode) = self.prompt_mode.as_ref() {
                set_status!(self, "{}", mode.status_msg());
            }
            self.refresh_screen()?;
            let key = self.loop_until_keypress()?;
            // TODO: Can we avoid using take()?
            self.prompt_mode = match self.prompt_mode.take() {
                // process_keypress returns (should_quit, prompt_mode)
                None => match self.process_keypress(&key) {
                    (true, _) => return Ok(()),
                    (false, prompt_mode) => prompt_mode,
                },
                Some(prompt_mode) => prompt_mode.process_keypress(self, &key)?,
            }
        }
    }
}

impl Drop for Editor {
    /// When the editor is dropped, restore the original terminal mode.
    fn drop(&mut self) {
        if let Some(orig_term_mode) = self.orig_term_mode.take() {
            sys::set_term_mode(&orig_term_mode).expect("Could not restore original terminal mode.");
        }
        if !thread::panicking() {
            print!("{}{}", CLEAR_SCREEN, MOVE_CURSOR_TO_START);
            io::stdout().flush().expect("Could not flush stdout");
        }
    }
}

/// The prompt mode.
enum PromptMode {
    /// Save(prompt buffer)
    Save(String),
    /// Find(prompt buffer, saved cursor state, last match)
    Find(String, CursorState, Option<usize>),
    /// GoTo(prompt buffer)
    GoTo(String),
    /// Execute(prompt buffer)
    Execute(String),
    /// Solid username
    Username(String),
    /// Solid password
    Password(String),
}

// TODO: Use trait with mode_status_msg and process_keypress, implement the trait for separate
//  structs for Save and Find?
impl PromptMode {
    /// Return the status message to print for the selected `PromptMode`.
    fn status_msg(&self) -> String {
        match self {
            Self::Username(buffer) => format!("Username: {}", buffer),
            Self::Password(buffer) => format!("Password: {}", "*".repeat(buffer.len())),
            Self::Save(buffer) => format!("Save as: {}", buffer),
            Self::Find(buffer, ..) => format!("Search (Use ESC/Arrows/Enter): {}", buffer),
            Self::GoTo(buffer) => format!("Enter line number[:column number]: {}", buffer),
            Self::Execute(buffer) => format!("Command to execute: {}", buffer),
        }
    }

    /// Process a keypress event for the selected `PromptMode`.
    fn process_keypress(self, ed: &mut Editor, key: &Key) -> Result<Option<Self>, Error> {
        ed.status_msg = None;
        match self {
            Self::Save(b) => match process_prompt_keypress(b, key) {
                PromptState::Active(b) => return Ok(Some(Self::Save(b))),
                PromptState::Cancelled => set_status!(ed, "Save aborted"),
                PromptState::Completed(file_name) => ed.save_as(file_name)?,
            },
            Self::Username(b) => match process_prompt_keypress(b, key) {
                PromptState::Active(b) => return Ok(Some(Self::Username(b))),
                PromptState::Cancelled => set_status!(ed, "No username"),
                PromptState::Completed(username) => {
                    ed.username = Some(username);
                    ed.prompt_mode = Some(PromptMode::Password(String::new()));
                    return Ok(ed.prompt_mode.take());
                }
            },
            Self::Password(b) => match process_prompt_keypress(b, key) {
                PromptState::Active(b) => return Ok(Some(Self::Password(b))),
                PromptState::Cancelled => set_status!(ed, "No password"),
                PromptState::Completed(password) => {
                    ed.password = Some(password);
                    let file_name = ed.file_name.as_ref().unwrap().clone();
                    match ed.ready_for.as_ref().unwrap() {
                        AdapterReadyFor::LOADING => {
                            ed.load(&Path::new(file_name.as_str()))?;
                        }
                        AdapterReadyFor::SAVING => {
                            ed.save(file_name.as_str()).unwrap();
                        }
                    };
                }
            },
            Self::Find(b, saved_cursor, last_match) => {
                if let Some(row_idx) = last_match {
                    ed.rows[row_idx].match_segment = None;
                }
                match process_prompt_keypress(b, key) {
                    PromptState::Active(query) => {
                        let (last_match, forward) = match key {
                            Key::Arrow(AKey::Right | AKey::Down) | Key::Char(FIND) =>
                                (last_match, true),
                            Key::Arrow(AKey::Left | AKey::Up) => (last_match, false),
                            _ => (None, true),
                        };
                        let curr_match = ed.find(&query, &last_match, forward);
                        return Ok(Some(Self::Find(query, saved_cursor, curr_match)));
                    }
                    // The prompt was cancelled. Restore the previous position.
                    PromptState::Cancelled => ed.cursor = saved_cursor,
                    // Cursor has already been moved, do nothing
                    PromptState::Completed(_) => (),
                }
            }
            Self::GoTo(b) => match process_prompt_keypress(b, key) {
                PromptState::Active(b) => return Ok(Some(Self::GoTo(b))),
                PromptState::Cancelled => (),
                PromptState::Completed(b) => {
                    let mut split = b
                        .splitn(2, ':')
                        // saturating_sub: Lines and cols are 1-indexed
                        .map(|u| u.trim().parse().map(|s: usize| s.saturating_sub(1)));
                    match (split.next().transpose(), split.next().transpose()) {
                        (Ok(Some(y)), Ok(x)) => {
                            ed.cursor.y = y.min(ed.rows.len());
                            if let Some(rx) = x {
                                ed.cursor.x = ed.current_row().map_or(0, |r| r.rx2cx[rx]);
                            } else {
                                ed.update_cursor_x_position();
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => set_status!(ed, "Parsing error: {}", e),
                        (Ok(None), _) => (),
                    }
                }
            },
            Self::Execute(b) => match process_prompt_keypress(b, key) {
                PromptState::Active(b) => return Ok(Some(Self::Execute(b))),
                PromptState::Cancelled => (),
                PromptState::Completed(b) => {
                    let mut args = b.split_whitespace();
                    match Command::new(args.next().unwrap_or_default()).args(args).output() {
                        Ok(out) if !out.status.success() => {
                            set_status!(ed, "{}", String::from_utf8_lossy(&out.stderr).trim_end())
                        }
                        Ok(out) => out.stdout.into_iter().for_each(|c| match c {
                            b'\n' => ed.insert_new_line(),
                            c => ed.insert_byte(c),
                        }),
                        Err(e) => set_status!(ed, "{}", e),
                    }
                }
            },
        }
        Ok(None)
    }
}

/// The state of the prompt after processing a keypress event.
enum PromptState {
    // Active contains the current buffer
    Active(String),
    // Completed contains the final string
    Completed(String),
    Cancelled,
}

/// Process a prompt keypress event and return the new state for the prompt.
fn process_prompt_keypress(mut buffer: String, key: &Key) -> PromptState {
    match key {
        Key::Char(b'\r') => return PromptState::Completed(buffer),
        Key::Escape | Key::Char(EXIT) => return PromptState::Cancelled,
        Key::Char(BACKSPACE | DELETE_BIS) => {
            buffer.pop();
        }
        Key::Char(c @ 0..=126) if !c.is_ascii_control() => buffer.push(*c as char),
        // No-op
        _ => (),
    }
    PromptState::Active(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_output() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1), "1B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.00kB");
        assert_eq!(format_size(1536), "1.50kB");
        // round down!
        assert_eq!(format_size(21 * 1024 - 11), "20.98kB");
        assert_eq!(format_size(21 * 1024 - 10), "20.99kB");
        assert_eq!(format_size(21 * 1024 - 3), "20.99kB");
        assert_eq!(format_size(21 * 1024), "21.00kB");
        assert_eq!(format_size(21 * 1024 + 3), "21.00kB");
        assert_eq!(format_size(21 * 1024 + 10), "21.00kB");
        assert_eq!(format_size(21 * 1024 + 11), "21.01kB");
        assert_eq!(format_size(1024 * 1024 - 1), "1023.99kB");
        assert_eq!(format_size(1024 * 1024), "1.00MB");
        assert_eq!(format_size(1024 * 1024 + 1), "1.00MB");
        assert_eq!(format_size(100 * 1024 * 1024 * 1024), "100.00GB");
        assert_eq!(format_size(313 * 1024 * 1024 * 1024 * 1024), "313.00TB");
    }

    #[test]
    fn editor_insert_byte() {
        let mut editor = Editor::default();
        let editor_cursor_x_before = editor.cursor.x;

        editor.insert_byte(b'X');
        editor.insert_byte(b'Y');
        editor.insert_byte(b'Z');

        assert_eq!(editor.cursor.x, editor_cursor_x_before + 3);
        assert_eq!(editor.rows.len(), 1);
        assert_eq!(editor.n_bytes, 3);
        assert_eq!(
            editor.rows[0].chars.iter().map(|uc| uc.0).collect::<Vec<u8>>(),
            [b'X', b'Y', b'Z']
        );
    }

    #[test]
    fn editor_insert_new_line() {
        let mut editor = Editor::default();
        let editor_cursor_y_before = editor.cursor.y;

        for _ in 0..3 {
            editor.insert_new_line();
        }

        assert_eq!(editor.cursor.y, editor_cursor_y_before + 3);
        assert_eq!(editor.rows.len(), 3);
        assert_eq!(editor.n_bytes, 0);

        for row in &editor.rows {
            assert!(row.chars.is_empty());
        }
    }
    #[test]
    fn editor_delete_char() {
        let mut editor = Editor::default();
        for b in "Hello!".as_bytes() {
            editor.insert_byte(*b);
        }
        editor.delete_char();
        assert_eq!(
            editor.rows[0].chars.iter().map(|uc| uc.0).collect::<Vec<u8>>(),
            "Hello".as_bytes()
        );
        editor.move_cursor(&AKey::Left);
        editor.move_cursor(&AKey::Left);
        editor.delete_char();
        assert_eq!(
            editor.rows[0].chars.iter().map(|uc| uc.0).collect::<Vec<u8>>(),
            "Helo".as_bytes()
        );
    }
}
