mod components;
mod event;
mod pages;
mod text;
mod theme;
mod ui;

use crate::agent::{AskReply, Engine, EngineEvent};
use crate::llm::tools::sandbox;
use crate::llm::{LlmClient, Toolbox};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, EventStream, KeyEvent,
    },
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

/// 消息内容块:按到达顺序交错存放文本与工具调用
/// 保留时序,避免"文本全在前、工具卡片全在后"导致卡片被后续文本顶下去
#[derive(Clone)]
pub enum Block {
    Text(String),
    Tool(ToolCallInfo),
}

/// 对话消息:blocks 按到达顺序记录文本段与工具卡片
#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
    pub status: MsgStatus,
    /// 本轮是否在等模型回复(等首 token):true 显示 Thinking
    /// 事件驱动:新建/工具结束时置 true,首 token/ToolStart/Done/Error 置 false
    pub awaiting_model: bool,
}

impl Message {
    pub fn user(content: String) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text(content)],
            status: MsgStatus::Completed,
            awaiting_model: false,
        }
    }

    /// 占位 assistant 消息(Thinking 状态),后续事件按序追加 Text/Tool 块
    pub fn assistant_thinking() -> Self {
        Self {
            role: Role::Assistant,
            blocks: Vec::new(),
            status: MsgStatus::Thinking,
            awaiting_model: true,
        }
    }

    /// 追加文本增量:末块是 Text 则拼接,否则新开一个 Text 块(保留工具后的文本时序)
    pub fn push_token(&mut self, token: &str) {
        match self.blocks.last_mut() {
            Some(Block::Text(s)) => s.push_str(token),
            _ => self.blocks.push(Block::Text(token.to_string())),
        }
    }

    /// 是否含任一工具块(状态栏判断"工具调用中"用)
    pub fn has_tool(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Tool(_)))
    }

    /// 是否有工具处于 Running(决定显示工具动画而非 Thinking)
    pub fn has_running_tool(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| matches!(b, Block::Tool(tc) if tc.status == ToolStatus::Running))
    }

    /// 是否无任何文本内容(Thinking 占位判断用)
    pub fn text_is_empty(&self) -> bool {
        !self
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Text(t) if !t.is_empty()))
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

/// 待确认的工具调用(Action::Ask):存下后端的 reply 通道,等用户按键回传
pub struct PendingAsk {
    #[allow(dead_code)]
    pub id: String,
    pub prompt: String,
    pub persistable: bool,
    /// 当前高亮选项索引:0=允许,(persistable 时)1=总是允许,末位=拒绝
    pub selected: usize,
    pub reply: tokio::sync::oneshot::Sender<AskReply>,
}

impl PendingAsk {
    /// 选项数:persistable 时 3(允许/总是允许/拒绝),否则 2(允许/拒绝)
    pub fn option_count(&self) -> usize {
        if self.persistable { 3 } else { 2 }
    }

    /// 当前高亮项对应的回答:0→One;persistable 时 1→Always;末位→Deny
    pub fn reply_for_selected(&self) -> AskReply {
        match self.selected {
            0 => AskReply::One,
            1 if self.persistable => AskReply::Always,
            _ => AskReply::Deny,
        }
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
    /// 待用户确认的工具调用(Action::Ask);Some 时输入框上方显示确认条并拦截按键
    pub pending_ask: Option<PendingAsk>,
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
            pending_ask: None,
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
                        last.push_token(&token);
                        last.status = MsgStatus::Streaming;
                        last.awaiting_model = false; // 首 token 到,模型已在回复
                    }
                }
            }
            EngineEvent::ToolStart { id, name, args } => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.awaiting_model = false; // 转工具动画,由卡片自己转
                        last.blocks.push(Block::Tool(ToolCallInfo {
                            id,
                            name,
                            args,
                            status: ToolStatus::Running,
                        }));
                    }
                }
            }
            EngineEvent::ToolResult { id, .. } => {
                if let Some(last) = self.messages.last_mut() {
                    if let Some(tc) = last.blocks.iter_mut().find_map(|b| match b {
                        Block::Tool(tc) if tc.id == id => Some(tc),
                        _ => None,
                    }) {
                        tc.status = ToolStatus::Done;
                    }
                    // 工具结束,又开始等下一轮模型(渲染层再用 has_running_tool 兜底多工具场景)
                    last.awaiting_model = true;
                }
            }
            EngineEvent::Done(_) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant && last.status != MsgStatus::Error {
                        last.status = MsgStatus::Completed;
                    }
                    last.awaiting_model = false;
                }
                self.mode = Mode::Input;
                self.thinking_since = None;
            }
            EngineEvent::Error(msg) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        if last.text_is_empty() {
                            last.blocks.push(Block::Text(msg));
                        }
                        last.status = MsgStatus::Error;
                    }
                    last.awaiting_model = false;
                }
                self.mode = Mode::Input;
                self.thinking_since = None;
            }
            EngineEvent::Ask {
                id,
                prompt,
                persistable,
                reply,
            } => {
                self.pending_ask = Some(PendingAsk {
                    id,
                    prompt,
                    persistable,
                    selected: 0, // 默认高亮"允许"
                    reply,
                });
            }
            EngineEvent::ToolOutputDelta { .. } => {}
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
        let (cur_vi, cur_vcol) = text::cursor_visual_pos(&self.buffer, self.cursor, width);
        if cur_vi == 0 {
            return;
        }
        let visual = text::visual_lines(&self.buffer, width);
        let cur_x = text::visual_x_of(&visual[cur_vi], cur_vcol);
        let (line, column) = text::visual_to_buffer(&self.buffer, width, cur_vi - 1, cur_x);
        self.cursor = Cursor { line, column };
    }

    pub fn cursor_down(&mut self) {
        let width = self.input_width;
        let (cur_vi, cur_vcol) = text::cursor_visual_pos(&self.buffer, self.cursor, width);
        let visual = text::visual_lines(&self.buffer, width);
        if cur_vi + 1 >= visual.len() {
            return;
        }
        let cur_x = text::visual_x_of(&visual[cur_vi], cur_vcol);
        let (line, column) = text::visual_to_buffer(&self.buffer, width, cur_vi + 1, cur_x);
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
    let model_name = crate::config::get()
        .map(|c| c.llm.model())
        .unwrap_or_else(|_| "deepseek-chat".to_string());
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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
            event::Event::Mouse(m) => handle_mouse(&mut app, m),
            event::Event::Engine(ev) => app.handle_engine_event(ev),
            event::Event::FocusGained => {
                // Windows 失焦回焦可能重置 console mode 导致鼠标捕获丢失,
                // 重新抢占鼠标,恢复 TUI 滚动/点击(不改 raw mode,避免闪烁)
                let _ = execute!(io::stdout(), EnableMouseCapture);
            }
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
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;

    Ok(())
}

/// 滚轮滚动消息列表:上滚上翻(禁用跟随),下滚下翻(接近底部时渲染层恢复跟随)
fn handle_mouse(app: &mut App, m: crossterm::event::MouseEvent) {
    use crossterm::event::MouseEventKind;
    match m.kind {
        MouseEventKind::ScrollUp => {
            app.auto_scroll = false;
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        _ => {}
    }
}

async fn handle_key(app: &mut App, key: KeyEvent, input_tx: &mpsc::Sender<String>) {
    use crossterm::event::{KeyCode, KeyModifiers};

    // Ctrl+C / Ctrl+D 始终可退出(即便有待确认项)
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        app.mode = Mode::Quit;
        return;
    }

    // 有待确认工具时,拦截按键做选择确认:←/→/Tab 移动高亮,Enter 确认,Esc 拒绝
    if let Some(mut pending) = app.pending_ask.take() {
        match key.code {
            KeyCode::Left => {
                pending.selected = pending.selected.saturating_sub(1);
                app.pending_ask = Some(pending);
            }
            KeyCode::Right | KeyCode::Tab => {
                pending.selected = (pending.selected + 1).min(pending.option_count() - 1);
                app.pending_ask = Some(pending);
            }
            KeyCode::Enter => {
                let ans = pending.reply_for_selected();
                let _ = pending.reply.send(ans);
            }
            KeyCode::Esc => {
                let _ = pending.reply.send(AskReply::Deny);
            }
            // 其他键忽略:放回,继续等待
            _ => app.pending_ask = Some(pending),
        }
        return;
    }

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
