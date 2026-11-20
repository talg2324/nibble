//! `nib --headless`: the same `App` and update loop as the window, with no window. Messages come
//! from the control socket and from the tasks `update` returns (the daemon's messages are one); the tasks `update` returns run on a local executor.
//! Window, clipboard and widget actions have nothing to act on and are dropped (a task waiting on
//! one just ends). Exits when a task asks to (`quit`).

use crate::app::{App, Init, Message};
use iced::futures::channel::{mpsc, oneshot};
use iced::futures::executor::LocalPool;
use iced::futures::task::LocalSpawnExt;
use iced::futures::StreamExt;
use iced::Task;
use iced_runtime::Action;

/// `None` = a task asked the app to exit.
type Tx = mpsc::UnboundedSender<Option<Message>>;

pub fn run(
    init: Init,
    syn_rx: oneshot::Receiver<()>,
    ctl_rx: mpsc::UnboundedReceiver<Message>,
) {
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    let (tx, mut rx) = mpsc::unbounded::<Option<Message>>();

    let spawn = |task: Task<Message>, tx: &Tx| {
        let Some(mut stream) = iced_runtime::task::into_stream(task) else { return };
        let tx = tx.clone();
        let _ = sp.spawn_local(async move {
            while let Some(action) = stream.next().await {
                match action {
                    Action::Output(m) => {
                        let _ = tx.unbounded_send(Some(m));
                    }
                    Action::Exit => {
                        let _ = tx.unbounded_send(None);
                    }
                    _ => {}
                }
            }
        });
    };

    {
        let tx = tx.clone();
        let _ = sp.spawn_local(async move {
            let mut src = ctl_rx;
            while let Some(m) = src.next().await {
                let _ = tx.unbounded_send(Some(m));
            }
        });
    }
    let _ = sp.spawn_local({
        let tx = tx.clone();
        async move {
            if syn_rx.await.is_ok() {
                let _ = tx.unbounded_send(Some(Message::SyntaxReady));
            }
        }
    });

    let (mut app, t) = App::boot(init);
    spawn(t, &tx);
    while let Some(Some(msg)) = pool.run_until(rx.next()) {
        let t = app.update(msg);
        spawn(t, &tx);
    }
}
