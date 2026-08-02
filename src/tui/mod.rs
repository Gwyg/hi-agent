mod event;
mod ui;

use crate::agent::{Engine, EngineEvent};
use crate::llm::tools::sandbox;
use crate::llm::{LlmClient, Toolbox};
use crossterm::{
    event::{EventStream, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

/// 消息角色
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 消息状态
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum MsgStatus {
    Sending,
    Streaming,
    Thinking,
    Completed,
    Error,
}

/// 工具调用状态
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

/// 工具调用信息(内联在 AI 消息下方的卡片)
#[derive(Clone)]
#[allow(dead_code)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub args: String,
    pub status: ToolStatus,
}

/// 对话消息
#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub status: MsgStatus,
    pub tool_calls: Vec<ToolCallInfo>,
}

impl Message {
    pub fn user(content: String) -> Self {
        Self {
            role: Role::User,
            content,
            status: MsgStatus::Completed,
            tool_calls: Vec::new(),
        }
    }

    /// 占位 assistant 消息(Thinking 状态),后续事件追加 content/tool_calls
    pub fn assistant_thinking() -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            status: MsgStatus::Thinking,
            tool_calls: Vec::new(),
        }
    }
}

/// TUI 运行模式
pub enum Mode {
    /// 等待用户输入(空会话也在此模式,ui 按 messages.is_empty() 显示提示)
    Input,
    /// 等待 agent 回复
    Thinking,
    /// 退出
    Quit,
}

/// 光标:二维行列坐标(column 为该行内的字符序号)
#[derive(Clone, Copy)]
pub struct Cursor {
    pub line: usize,
    pub column: usize,
}

impl Cursor {
    pub fn origin() -> Self {
        Self { line: 0, column: 0 }
    }
}

/// App 状态
pub struct App {
    /// 多行输入缓冲区:每行一个 String(硬换行按 Shift+Enter)
    pub buffer: Vec<String>,
    /// 光标:行列二维坐标
    pub cursor: Cursor,
    /// 最近一次渲染时的输入框内容宽度(软换行用,供光标跨视觉行移动)
    pub input_width: usize,
    pub messages: Vec<Message>,
    pub mode: Mode,
    pub model_name: String,
    /// 启动时缓存的工作目录(状态栏显示,避免每帧 IO)
    pub cwd: String,
    /// 思考开始时间(用于 spinner 计时)
    pub thinking_since: Option<Instant>,
    /// spinner 动画帧索引(0-5)
    pub frame: u8,
    /// 消息列表滚动偏移(视觉行)
    pub scroll_offset: usize,
    /// 是否自动滚动到底部(新消息/流式输出时跟随)
    pub auto_scroll: bool,
    /// 光标 ▎ 是否显示(500ms 闪烁)
    pub cursor_visible: bool,
    /// 闪烁计数(每 tick 累加,每 4 tick 翻转 ≈ 480ms)
    pub blink_tick: u8,
}

impl App {
    pub fn new(model_name: String, cwd: String) -> Self {
        Self {
            buffer: vec![String::new()],
            cursor: Cursor::origin(),
            input_width: 0,
            messages: Vec::new(),
            mode: Mode::Input,
            model_name,
            cwd,
            thinking_since: None,
            frame: 0,
            scroll_offset: 0,
            auto_scroll: true,
            cursor_visible: true,
            blink_tick: 0,
        }
    }

    /// 输入缓冲区是否为空(只有一行且为空)
    pub fn is_empty(&self) -> bool {
        self.buffer.len() == 1 && self.buffer[0].is_empty()
    }

    /// 提交当前输入:加 user 消息 + 占位 assistant(Thinking) 消息,切 Thinking 模式
    pub fn submit_input(&mut self) {
        let content = self.buffer.join("\n").trim().to_string();
        if content.is_empty() {
            return;
        }
        self.messages.push(Message::user(content));
        // 占位 assistant 消息:后续 TokenDelta 追加 content,ToolStart 加卡片
        self.messages.push(Message::assistant_thinking());
        self.buffer = vec![String::new()];
        self.cursor = Cursor::origin();
        self.mode = Mode::Thinking;
        self.thinking_since = Some(Instant::now());
        self.auto_scroll = true;
    }

    /// 消费 agent 事件,更新最后一条 assistant 消息
    pub fn handle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::TokenDelta(token) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.content.push_str(&token);
                        last.status = MsgStatus::Streaming;
                    }
                }
            }
            EngineEvent::ToolStart { id, name, args } => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.tool_calls.push(ToolCallInfo {
                            id,
                            name,
                            args,
                            status: ToolStatus::Running,
                        });
                    }
                }
            }
            EngineEvent::ToolResult { id, .. } => {
                if let Some(last) = self.messages.last_mut() {
                    if let Some(tc) = last.tool_calls.iter_mut().find(|t| t.id == id) {
                        tc.status = ToolStatus::Done;
                    }
                }
            }
            EngineEvent::Done(_) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant && last.status != MsgStatus::Error {
                        last.status = MsgStatus::Completed;
                    }
                }
                self.mode = Mode::Input;
                self.thinking_since = None;
            }
            EngineEvent::Error(msg) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        if last.content.is_empty() {
                            last.content = msg;
                        }
                        last.status = MsgStatus::Error;
                    }
                }
                self.mode = Mode::Input;
                self.thinking_since = None;
            }
            // Ask 暂不处理交互确认:reply drop → agent_loop fallback 暂拒
            EngineEvent::Ask { .. } | EngineEvent::ToolOutputDelta { .. } => {}
        }
    }

    pub fn should_quit(&self) -> bool {
        matches!(self.mode, Mode::Quit)
    }

    // ===== 输入编辑方法(光标移动/插入/删除,平台无关) =====

    /// 行内字符序号 → 字节偏移
    fn byte_idx(line: &str, column: usize) -> usize {
        line.char_indices()
            .nth(column)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    }

    pub fn insert_char(&mut self, c: char) {
        let byte = Self::byte_idx(&self.buffer[self.cursor.line], self.cursor.column);
        self.buffer[self.cursor.line].insert(byte, c);
        self.cursor.column += 1;
    }

    pub fn insert_newline(&mut self) {
        let (line, column) = (self.cursor.line, self.cursor.column);
        let byte = Self::byte_idx(&self.buffer[line], column);
        let tail = self.buffer[line][byte..].to_string();
        self.buffer[line].truncate(byte);
        self.buffer.insert(line + 1, tail);
        self.cursor = Cursor::origin();
        self.cursor.line = line + 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor.column > 0 {
            let line = &mut self.buffer[self.cursor.line];
            let byte = Self::byte_idx(line, self.cursor.column);
            let prev_len = line[..byte]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            line.replace_range(byte - prev_len..byte, "");
            self.cursor.column -= 1;
        } else if self.cursor.line > 0 {
            let prev_len = self.buffer[self.cursor.line - 1].chars().count();
            let tail = self.buffer.remove(self.cursor.line);
            self.buffer[self.cursor.line - 1].push_str(&tail);
            self.cursor.line -= 1;
            self.cursor.column = prev_len;
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor.column > 0 {
            self.cursor.column -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.column = self.buffer[self.cursor.line].chars().count();
        }
    }

    pub fn cursor_right(&mut self) {
        let line_len = self.buffer[self.cursor.line].chars().count();
        if self.cursor.column < line_len {
            self.cursor.column += 1;
        } else if self.cursor.line + 1 < self.buffer.len() {
            self.cursor.line += 1;
            self.cursor.column = 0;
        }
    }

    pub fn cursor_up(&mut self) {
        let width = self.input_width;
        let (cur_vi, cur_vcol) = ui::cursor_visual_pos(&self.buffer, self.cursor, width);
        if cur_vi == 0 {
            return;
        }
        let visual = ui::visual_lines(&self.buffer, width);
        let cur_x = ui::visual_x_of(&visual[cur_vi], cur_vcol);
        let (line, column) = ui::visual_to_buffer(&self.buffer, width, cur_vi - 1, cur_x);
        self.cursor = Cursor { line, column };
    }

    pub fn cursor_down(&mut self) {
        let width = self.input_width;
        let (cur_vi, cur_vcol) = ui::cursor_visual_pos(&self.buffer, self.cursor, width);
        let visual = ui::visual_lines(&self.buffer, width);
        if cur_vi + 1 >= visual.len() {
            return;
        }
        let cur_x = ui::visual_x_of(&visual[cur_vi], cur_vcol);
        let (line, column) = ui::visual_to_buffer(&self.buffer, width, cur_vi + 1, cur_x);
        self.cursor = Cursor { line, column };
    }
}

/// TUI 入口
///
/// agent task 持有 engine,通过 mpsc channel 双向通信:
/// - input_tx → agent task:用户输入
/// - event_tx ← agent task:EngineEvent 事件流(TokenDelta/ToolStart/ToolResult/Done/Error)
/// TUI 主循环 select 键盘事件 + agent 事件流 + spinner tick
pub async fn run() -> anyhow::Result<()> {
    let toolbox = Toolbox::new();
    let model_name = std::env::var("MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let client = LlmClient::new(toolbox.definitions());
    let mut engine = Engine::new(client, toolbox);

    let cwd = sandbox::project_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string());

    let (input_tx, mut input_rx) = mpsc::channel::<String>(8);
    let (event_tx, mut event_rx) = mpsc::channel::<EngineEvent>(64);

    tokio::spawn(async move {
        while let Some(input) = input_rx.recv().await {
            // run_turn_stream emit 事件到 event_tx;出错时 agent_loop 已 emit Error
            let _ = engine.run_turn_stream(&input, event_tx.clone()).await;
        }
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(model_name, cwd);
    let mut events = EventStream::new();

    let mut tick = interval(Duration::from_millis(120));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while !app.should_quit() {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        match event::next_event(&mut events, &mut event_rx, &mut tick).await {
            event::Event::Key(key) => handle_key(&mut app, key, &input_tx).await,
            event::Event::Engine(ev) => app.handle_engine_event(ev),
            event::Event::Tick => {
                app.frame = (app.frame + 1) % 6;
                // 光标闪烁:每 4 tick(≈480ms)翻转
                app.blink_tick = app.blink_tick.wrapping_add(1);
                if app.blink_tick % 4 == 0 {
                    app.cursor_visible = !app.cursor_visible;
                }
            }
            event::Event::Quit => app.mode = Mode::Quit,
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    Ok(())
}

async fn handle_key(app: &mut App, key: KeyEvent, input_tx: &mpsc::Sender<String>) {
    use crossterm::event::{KeyCode, KeyModifiers};

    match (key.modifiers, key.code) {
        (m, KeyCode::Char('c')) | (m, KeyCode::Char('d')) if m.contains(KeyModifiers::CONTROL) => {
            app.mode = Mode::Quit;
        }
        (m, KeyCode::Enter) if matches!(app.mode, Mode::Input) => {
            if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::CONTROL) {
                app.insert_newline();
            } else {
                let content = app.buffer.join("\n");
                let content = content.trim().to_string();
                if !content.is_empty() {
                    if input_tx.send(content).await.is_ok() {
                        app.submit_input();
                    }
                }
            }
        }
        (_, KeyCode::Backspace) if matches!(app.mode, Mode::Input) => {
            app.backspace();
        }
        (_, KeyCode::Left) if matches!(app.mode, Mode::Input) => {
            app.cursor_left();
        }
        (_, KeyCode::Right) if matches!(app.mode, Mode::Input) => {
            app.cursor_right();
        }
        (_, KeyCode::Up) if matches!(app.mode, Mode::Input) => {
            app.cursor_up();
        }
        (_, KeyCode::Down) if matches!(app.mode, Mode::Input) => {
            app.cursor_down();
        }
        // Thinking 模式:↑ 上翻消息(禁用 auto_scroll),↓ 下翻(到底部附近恢复)
        (_, KeyCode::Up) if matches!(app.mode, Mode::Thinking) => {
            app.auto_scroll = false;
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
        }
        (_, KeyCode::Down) if matches!(app.mode, Mode::Thinking) => {
            app.scroll_offset = app.scroll_offset.saturating_add(1);
        }
        // PgUp/PgDown 翻消息列表(任何模式生效):PgUp 上翻+禁用跟随,PgDown 下翻(到底部附近渲染时恢复)
        (_, KeyCode::PageUp) => {
            app.auto_scroll = false;
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        (_, KeyCode::PageDown) => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        (m, KeyCode::Char(c))
            if matches!(app.mode, Mode::Input) && !m.contains(KeyModifiers::CONTROL) =>
        {
            app.insert_char(c);
        }
        _ => {}
    }
}
