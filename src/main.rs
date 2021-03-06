#![feature(hash_drain_filter)]

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures::Future;
use teloxide::{
    adaptors::{throttle::Limits, Throttle},
    error_handlers::OnError,
    prelude::*,
    types::{Me, MessageKind, MessageNewChatMembers, User},
    ApiError, RequestError,
};
use tokio::sync::mpsc::{self, Sender};

type Bot = AutoSend<Throttle<teloxide::Bot>>;
type WorkerTx = mpsc::Sender<(ChatId, (i32, Instant))>;

const START_MSG: &str = "\
I automatically unpin messages sent from the linked channel. Don't forget to grant me Pin Messages permission to work.

Source code: https://github.com/WaffleLapkin/unpinnerbot";

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "INFO");
    }
    pretty_env_logger::init();
    log::info!("Starting unpinnerbot...");

    let bot = teloxide::Bot::from_env()
        .throttle(Limits::default())
        .auto_send();
    let Me { user: bot_user, .. } = bot.get_me().await.unwrap();

    let (worker_tx, worker_rx) = mpsc::channel(100);
    let unpin_task = tokio::spawn(worker(&bot, worker_rx));

    // A tree with only one branch, how sad
    let dispatch_tree = dptree::entry().branch(Update::filter_message().endpoint(
        |message: Message, bot: Bot, bot_user: User, worker_tx: WorkerTx| {
            let (worker_tx, bot_user) = (worker_tx.clone(), bot_user.clone());

            async move {
                if message.chat.is_private() {
                    handle_private(message, bot).await.log_on_error().await;
                } else if message.chat.is_supergroup() {
                    handle_supergroup(message, bot, worker_tx, bot_user).await;
                }

                Ok::<_, ()>(())
            }
        },
    ));

    Dispatcher::builder(bot, dispatch_tree)
        // Ignore anything but messages
        .default_handler(|_| async {})
        .dependencies(dptree::deps![bot_user, worker_tx])
        .build()
        .setup_ctrlc_handler()
        .dispatch()
        .await;

    unpin_task.await.unwrap();
}

async fn handle_private(message: Message, bot: Bot) -> Result<(), RequestError> {
    bot.send_message(message.chat.id, START_MSG)
        .disable_web_page_preview(true)
        .await?;

    Ok(())
}

async fn handle_supergroup(
    message: Message,
    bot: Bot,
    worker_tx: Sender<(ChatId, (i32, Instant))>,
    bot_user: User,
) {
    match message.kind {
        MessageKind::NewChatMembers(MessageNewChatMembers { new_chat_members }) => {
            if new_chat_members.iter().any(|u| u.id == bot_user.id) {
                bot.send_message(message.chat.id, START_MSG)
                    .disable_web_page_preview(true)
                    .await
                    .log_on_error()
                    .await;
            }
        }
        MessageKind::Common(message_common) => {
            if message_common.from.filter(User::is_telegram).is_some() {
                worker_tx
                    .send((message.chat.id, (message.id, Instant::now())))
                    .await
                    .unwrap();
            }
        }
        _ => {}
    }
}

fn worker(bot: &Bot, mut rx: mpsc::Receiver<(ChatId, (i32, Instant))>) -> impl Future<Output = ()> {
    let bot = bot.clone();

    async move {
        let mut rx_closed = false;
        const TIMEOUT: Duration = Duration::from_secs(1);

        // Chat id => (id of the last message from linked channel, time it was received)
        let mut tasks: HashMap<ChatId, (i32, Instant)> = HashMap::with_capacity(64);

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
    rx: &mut mpsc::Receiver<(ChatId, (i32, Instant))>,
    tasks: &mut HashMap<ChatId, (i32, Instant)>,
    rx_is_closed: &mut bool,
) {
    if tasks.is_empty() {
        match rx.recv().await {
            Some((chat_id, value)) => {
                tasks.insert(chat_id, value);
            }
            None => {
                *rx_is_closed = true;
                return;
            }
        }
    }

    // Don't grow queue bigger than the capacity to limit DOS possibility
    while tasks.len() < tasks.capacity() {
        match rx.try_recv() {
            Ok((chat_id, new_task)) => {
                // Insert the latest message id into jobs list
                let task = tasks.entry(chat_id).or_insert(new_task);
                *task = std::cmp::max_by_key(*task, new_task, |(mid, _)| *mid);
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                *rx_is_closed = true;
                break;
            }
            // There are no items in queue.
            Err(mpsc::error::TryRecvError::Empty) => break,
        }
    }
}

async fn unpin(bot: &AutoSend<Throttle<teloxide::Bot>>, group: ChatId, msg_id: i32) {
    if let Err(e) = bot.unpin_chat_message(group).message_id(msg_id).await {
        match e {
            RequestError::Api(ApiError::NotEnoughRightsToManagePins) => {
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
