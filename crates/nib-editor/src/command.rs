//! Every user-invokable action. The keymap maps chords to these; the shortcut editor page
//! lists them with `title`/`category` so it can be searched and rebound visually.

use serde::{Deserialize, Serialize};

macro_rules! commands {
    ($( $variant:ident => $name:literal, $cat:literal, $title:literal; )*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Command { $( $variant, )* }

        impl Command {
            pub const ALL: &'static [Command] = &[ $( Command::$variant, )* ];
            /// Stable identifier used in config.toml.
            pub fn name(self) -> &'static str { match self { $( Command::$variant => $name, )* } }
            pub fn category(self) -> &'static str { match self { $( Command::$variant => $cat, )* } }
            pub fn title(self) -> &'static str { match self { $( Command::$variant => $title, )* } }
            pub fn from_name(s: &str) -> Option<Command> {
                match s { $( $name => Some(Command::$variant), )* _ => None }
            }
        }
    };
}

commands! {
    Save            => "save",              "File",      "Save";
    Quit            => "quit",              "File",      "Quit";
    Undo            => "undo",              "Edit",      "Undo";
    Redo            => "redo",              "Edit",      "Redo";
    Copy            => "copy",              "Edit",      "Copy (line if no selection)";
    Cut             => "cut",               "Edit",      "Cut (line if no selection)";
    Paste           => "paste",             "Edit",      "Paste";
    SelectAll       => "select_all",        "Selection", "Select all";
    Indent          => "indent",            "Edit",      "Indent / insert tab";
    Dedent          => "dedent",            "Edit",      "Dedent";
    Newline         => "newline",           "Edit",      "New line (keeps indentation)";
    DeleteBackward  => "delete_backward",   "Edit",      "Delete character before cursor";
    DeleteForward   => "delete_forward",    "Edit",      "Delete character after cursor";
    DeleteWordBackward => "delete_word_backward", "Edit", "Delete word before cursor";
    DeleteLine      => "delete_line",       "Edit",      "Delete line";
    DuplicateLine   => "duplicate_line",    "Edit",      "Duplicate line";
    ToggleComment   => "toggle_comment",    "Edit",      "Toggle line comment";
    MoveLeft        => "move_left",         "Cursor",    "Move left";
    MoveRight       => "move_right",        "Cursor",    "Move right";
    MoveUp          => "move_up",           "Cursor",    "Move up";
    MoveDown        => "move_down",         "Cursor",    "Move down";
    MoveWordLeft    => "move_word_left",    "Cursor",    "Move to previous word";
    MoveWordRight   => "move_word_right",   "Cursor",    "Move to next word";
    MoveLineStart   => "move_line_start",   "Cursor",    "Move to line start (smart)";
    MoveLineEnd     => "move_line_end",     "Cursor",    "Move to line end";
    MoveDocStart    => "move_doc_start",    "Cursor",    "Move to start of file";
    MoveDocEnd      => "move_doc_end",      "Cursor",    "Move to end of file";
    PageUp          => "page_up",           "Cursor",    "Page up";
    PageDown        => "page_down",         "Cursor",    "Page down";
    SelectLeft      => "select_left",       "Selection", "Extend selection left";
    SelectRight     => "select_right",      "Selection", "Extend selection right";
    SelectUp        => "select_up",         "Selection", "Extend selection up";
    SelectDown      => "select_down",       "Selection", "Extend selection down";
    SelectWordLeft  => "select_word_left",  "Selection", "Extend selection to previous word";
    SelectWordRight => "select_word_right", "Selection", "Extend selection to next word";
    SelectLineStart => "select_line_start", "Selection", "Extend selection to line start";
    SelectLineEnd   => "select_line_end",   "Selection", "Extend selection to line end";
    SelectPageUp    => "select_page_up",    "Selection", "Extend selection page up";
    SelectPageDown  => "select_page_down",  "Selection", "Extend selection page down";
    SelectDocStart  => "select_doc_start",  "Selection", "Extend selection to start of file";
    SelectDocEnd    => "select_doc_end",    "Selection", "Extend selection to end of file";
    CollapseSelection => "collapse_selection", "Selection", "Clear selection";
    ChangeAllOccurrences => "change_all_occurrences", "Selection", "Change all occurrences (a cursor on every instance in this file)";
    GoToDefinition  => "go_to_definition",  "Code",      "Go to definition";
    FindReferences  => "find_references",   "Code",      "Find all references";
    RenameSymbol    => "rename_symbol",     "Code",      "Rename symbol (across the project)";
    CommandPalette  => "command_palette",   "Navigate",  "Commands: find any command by name and run it";
    Find            => "find",              "Navigate",  "Find: in the file or chat with focus (Tab: the whole project, file names too)";
    NavigateBack    => "navigate_back",     "Navigate",  "Go back (after a jump)";
    NavigateForward => "navigate_forward",  "Navigate",  "Go forward";
    ToggleExplorer  => "toggle_explorer",   "Navigate",  "File tree: show it and go there (from inside it: hide)";
    OpenInNewTab    => "open_in_new_tab",   "Navigate",  "Open the selected file in a new tab";
    NextTab         => "next_tab",          "Tabs",      "Next file tab (with fewer than two files open, or outside the editor: next pane)";
    PrevTab         => "prev_tab",          "Tabs",      "Previous file tab (with fewer than two files open, or outside the editor: previous pane)";
    CloseTab        => "close_tab",         "Tabs",      "Close what has focus (editor tab, terminal, or the file tree)";
    ReopenClosedTab => "reopen_closed_tab", "Tabs",      "Reopen the last closed tab";
    GoToTab1        => "go_to_tab_1",       "Tabs",      "Go to tab 1";
    GoToTab2        => "go_to_tab_2",       "Tabs",      "Go to tab 2";
    GoToTab3        => "go_to_tab_3",       "Tabs",      "Go to tab 3";
    GoToTab4        => "go_to_tab_4",       "Tabs",      "Go to tab 4";
    GoToTab5        => "go_to_tab_5",       "Tabs",      "Go to tab 5";
    GoToTab6        => "go_to_tab_6",       "Tabs",      "Go to tab 6";
    GoToTab7        => "go_to_tab_7",       "Tabs",      "Go to tab 7";
    GoToTab8        => "go_to_tab_8",       "Tabs",      "Go to tab 8";
    GoToTab9        => "go_to_tab_9",       "Tabs",      "Go to tab 9";
    FontBigger      => "font_bigger",       "View",      "Bigger font";
    FontSmaller     => "font_smaller",      "View",      "Smaller font";
    FontReset       => "font_reset",        "View",      "Font back to the default size (a preview or page: back to 100%)";
    TogglePreview   => "toggle_preview",    "View",      "Preview (Markdown, HTML) / table or text (CSV)";
    ToggleTerminal  => "toggle_terminal",   "Panels",    "Terminals: open one, or show / hide them (hold it with H: hide / show terminals, B: toolbar)";
    NewTerminal     => "new_terminal",      "Panels",    "Another terminal";
    ToggleApps      => "toggle_apps",       "Panels",    "Apps panel: this project's apps (open, stop, register)";
    AppUp           => "app_up",            "Panels",    "Apps: start every approved app, each server in a terminal (apps.json in NIBDIR)";
    AppDown         => "app_down",          "Panels",    "Apps: stop them all";
    AppRestart      => "app_restart",       "Panels",    "Apps: restart the server whose terminal has the keyboard";
    NextPane        => "next_pane",         "Panels",    "Next pane (file tree, editor, terminals, Git panel, toolbar)";
    PrevPane        => "prev_pane",         "Panels",    "Previous pane";
    FocusToolbar    => "focus_toolbar",     "Panels",    "Toolbar: jump to it and back (arrows pick a button, Enter presses it)";
    ToggleGit       => "toggle_git",        "Panels",    "Git panel: history, pull, push, commit, stash";
    ToggleDiff      => "toggle_diff",       "Panels",    "Review tab: what changed, like a pull request";
    ToggleAi        => "toggle_ai",         "Panels",    "Agents tab: show / hide";
    NextAgent       => "next_agent",        "Agents",    "Next agent";
    PrevAgent       => "prev_agent",        "Agents",    "Previous agent";
    NewAgent        => "new_agent",         "Agents",    "New agent";
    StopAgent       => "stop_agent",        "Agents",    "Stop what the shown agent is doing (it stays, with its conversation)";
    KillAgent       => "kill_agent",        "Agents",    "Kill the agent's conversation (a local model is unloaded)";
    FoldAgents      => "fold_agents",       "Agents",    "Fold the agent list: one agent at a time";
    AgentHistory    => "agent_history",     "Agents",    "History: past conversations (search, reopen, replay, delete)";
    Panic           => "panic",             "Agents",    "Panic: pause every agent, choose which to stop";
    ToggleSsh       => "toggle_ssh",        "Panels",    "SSH panel: saved connections, connect, add a host";
    OpenFolder      => "open_folder",       "File",      "Open a folder as the project (browse this machine)";
    OpenSettings    => "open_settings",     "Settings",  "Settings: Appearance";
    KeyboardShortcuts => "keyboard_shortcuts", "Settings", "Settings: Keyboard shortcuts";
    OpenLanguages   => "open_languages",    "Settings",  "Settings: Languages (add syntax highlighting)";
    ToggleBeginnerMode => "toggle_beginner_mode", "Settings", "Beginner mode (shortcut cheat sheet)";
    ToggleFullscreen => "toggle_fullscreen",  "Window",    "Fullscreen";
}
