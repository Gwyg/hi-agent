use crate::agent::EngineEvent;
use crossterm::event::{
    Event as CtEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Interval;

/// TUI 事件:键盘 / 鼠标 / agent 事件流 / tick / 退出
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Engine(EngineEvent),
    /// 终端重新获得焦点(Windows 失焦回焦可能重置 console mode,需重设鼠标捕获)
    FocusGained,
    /// spinner 动画 tick
    Tick,
    Quit,
}

/// 合并键盘事件流、agent 事件流、tick interval
///
/// 用 `select!` 三路竞争:谁先来谁返回。
/// 非键盘/鼠标事件(Resize 等)和 Release/Repeat 帧被忽略继续等。
pub async fn next_event(
    events: &mut EventStream,
    event_rx: &mut mpsc::Receiver<EngineEvent>,
    tick: &mut Interval,
) -> Event {
    loop {
        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                    return Event::Key(key);
                }
                Some(Ok(CtEvent::Mouse(m))) => return Event::Mouse(m),
                Some(Ok(CtEvent::FocusGained)) => return Event::FocusGained,
                Some(Ok(_)) => continue,  // 忽略 Resize/Release/Repeat/FocusLost
                _ => return Event::Quit,
            },
            ev = event_rx.recv() => {
                return match ev {
                    Some(e) => Event::Engine(e),
                    None => Event::Quit,  // agent task 退出 → 退出 TUI
                };
            }
            _ = tick.tick() => return Event::Tick,
        }
    }
}
