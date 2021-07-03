#![feature(hash_drain_filter)]

use futures::{Future, FutureExt};
use std::time::Duration;
use std::{collections::HashMap, time::Instant};
use teloxide::types::{MessageKind, User};
use teloxide::{
    adaptors::{throttle::Limits, Throttle},
    payloads::SendMessageSetters,
    ApiError, RequestError,
};
use teloxide::{prelude::*, types::Me};
use tokio::sync::mpsc::{self, Sender};

type Bot = AutoSend<Throttle<teloxide::Bot>>;

const START_MSG: &str = "\
I automatically unpin messages sent from the linked channel. Don't forget to grant me Pin Messages permission to work.

Source code: https://git.sr.ht/~handlerug/unpinnerbot";

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "INFO");
    }
    pretty_env_logger::init();
    log::info!("Starting unpinnerbot...");

    let (worker_tx, worker_rx) = mpsc::channel(100);

    let bot = teloxide::Bot::from_env()
        .throttle(Limits::default())
        .auto_send();
    let Me { user: bot_user, .. } = bot.get_me().await.unwrap();

    let unpin_task = tokio::spawn(worker(&bot, worker_rx));

    Dispatcher::new(bot)
        .messages_handler(move |mut rx: DispatcherHandlerRx<_, Message>| async move {
            while let Some(update) = rx.recv().await {
                let message = &update.update;
                if message.chat.is_private() {
                    handle_private(update).await.log_on_error().await;
                } else if message.chat.is_supergroup() {
                    handle_supergroup(update, &worker_tx, &bot_user).await;
                }
            }
        })
        .dispatch()
        .await;

    unpin_task.await.unwrap();
}

async fn handle_private(cx: UpdateWithCx<Bot, Message>) -> Result<(), RequestError> {
    cx.answer(START_MSG).disable_web_page_preview(true).await?;
    Ok(())
}

async fn handle_supergroup(
    cx: UpdateWithCx<Bot, Message>,
    worker_tx: &Sender<(i64, (i32, Instant))>,
    bot_user: &User,
) {
    let message = &cx.update;
    match &message.kind {
        MessageKind::NewChatMembers(message) => {
            if message.new_chat_members.iter().any(|u| u.id == bot_user.id) {
                cx.answer(START_MSG)
                    .disable_web_page_preview(true)
                    .await
                    .log_on_error()
                    .await;
            }
        }
        MessageKind::Common(message_common) => {
            if let Some(User { id: 777000, .. }) = message_common.from {
                worker_tx
                    .send((message.chat.id, (message.id, Instant::now())))
                    .await
                    .unwrap();
            }
        }
        _ => {}
    }
}

fn worker(
    bot: &AutoSend<Throttle<teloxide::Bot>>,
    mut rx: mpsc::Receiver<(i64, (i32, Instant))>,
) -> impl Future<Output = ()> {
    let bot = bot.clone();

    async move {
        let mut rx_closed = false;
        const TIMEOUT: Duration = Duration::from_secs(1);

        // Chat id => (id of the last message from linked channel, time it was received)
        let mut tasks: HashMap<i64, (i32, Instant)> = HashMap::with_capacity(64);

        while !rx_closed || !tasks.is_empty() {
            read_from_rx(&mut rx, &mut tasks, &mut rx_closed).await;
            let now = Instant::now();
            let timeout_back = now - TIMEOUT;

            for (group, msg_id) in tasks
                .drain_filter(|_, (_, t)| *t < timeout_back)
                .map(|(g, (m, _))| (g, m))
            {
                unpin(&bot, group, msg_id).await;
            }
        }
    }
}

async fn read_from_rx(
    rx: &mut mpsc::Receiver<(i64, (i32, Instant))>,
    tasks: &mut HashMap<i64, (i32, Instant)>,
    rx_is_closed: &mut bool,
) {
    if tasks.is_empty() {
        match rx.recv().await {
            Some((chat_id, value)) => {
                tasks.insert(chat_id, value);
            }
            None => *rx_is_closed = true,
        }
    }

    // Don't grow queue bigger than the capacity to limit DOS posibility
    while tasks.len() < tasks.capacity() {
        // FIXME: https://github.com/tokio-rs/tokio/issues/3350
        match rx.recv().now_or_never() {
            Some(Some((chat_id, new_task))) => {
                // Insert the latest message id into jobs list
                let task = tasks.entry(chat_id).or_insert(new_task);
                *task = std::cmp::max_by_key(*task, new_task, |(mid, _)| *mid);
            }
            Some(None) => *rx_is_closed = true,
            // There are no items in queue.
            None => break,
        }
    }
}

async fn unpin(bot: &AutoSend<Throttle<teloxide::Bot>>, group: i64, msg_id: i32) {
    if let Err(e) = bot.unpin_chat_message(group).message_id(msg_id).await {
        match e {
            RequestError::ApiError { kind: ApiError::NotEnoughRightsToManagePins, ..} => {
                bot
                    .send_message(group, "Failed to unpin this message. Please, grant me Pin Messages permission to work properly.")
                    .reply_to_message_id(msg_id)
                    .await
                    .log_on_error()
                    .await
            },
            _ => log::error!("Error: {}", e),
        }
    }
}
