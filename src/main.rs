use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use teloxide::types::{MessageKind, User};
use teloxide::{
    adaptors::{throttle::Limits, Throttle},
    payloads::SendMessageSetters,
    ApiError, RequestError,
};
use teloxide::{prelude::*, types::Me};
use tokio::sync::{
    mpsc::{self, Sender},
    Mutex,
};

type Bot = AutoSend<Throttle<teloxide::Bot>>;

const START_MSG: &str = "I automatically unpin messages sent from the linked channel. Don't forget to grant me Pin Messages permission to work.";

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "INFO");
    }
    pretty_env_logger::init();
    log::info!("Starting unpinnerbot...");

    let (debounce_tx, mut debounce_rx) = mpsc::channel(100);

    let bot = teloxide::Bot::from_env()
        .throttle(Limits::default())
        .auto_send();
    let Me { user: bot_user, .. } = bot.get_me().await.unwrap();

    let channel_map: Arc<Mutex<HashMap<i64, i32>>> = Default::default();

    let debounce_task = tokio::spawn({
        let bot = bot.clone();
        let channel_map = Arc::clone(&channel_map);
        let timeout = Duration::from_secs(1);
        let mut is_there_work = false;

        async move {
            loop {
                match tokio::time::timeout(timeout, debounce_rx.recv()).await {
                    Ok(Some(())) => {
                        log::debug!("debouncing");
                        is_there_work = true;
                    }
                    Ok(None) => {
                        log::debug!("stopping");
                        break;
                    }
                    Err(_) if is_there_work => {
                        log::debug!("done debouncing");

                        let mut channel_map = channel_map.lock().await;
                        for (&group, &msg_id) in channel_map.iter() {
                            if let Err(e) = bot.unpin_chat_message(group).message_id(msg_id).await {
                                if let RequestError::ApiError {
                                    kind: ApiError::NotEnoughRightsToManagePins,
                                    ..
                                } = e
                                {
                                    bot.send_message(group, "Failed to unpin this message. Please, grant me Pin Messages permission to work properly.").reply_to_message_id(msg_id).await.log_on_error().await;
                                } else {
                                    log::error!("Error: {}", e);
                                }
                            }
                        }
                        channel_map.clear();

                        is_there_work = false;
                    }
                    _ => {}
                }
            }
        }
    });

    Dispatcher::new(bot)
        .messages_handler(move |mut rx: DispatcherHandlerRx<_, Message>| async move {
            let channel_map = Arc::clone(&channel_map);

            while let Some(update) = rx.recv().await {
                let message = &update.update;
                if message.chat.is_private() {
                    handle_private(update).await.log_on_error().await;
                } else if message.chat.is_supergroup() {
                    handle_supergroup(update, &debounce_tx, &bot_user, &*channel_map).await;
                }
            }
        })
        .dispatch()
        .await;

    debounce_task.await.unwrap();
}

async fn handle_private(cx: UpdateWithCx<Bot, Message>) -> Result<(), RequestError> {
    cx.answer(START_MSG).await?;
    Ok(())
}

async fn handle_supergroup(
    cx: UpdateWithCx<Bot, Message>,
    debounce_tx: &Sender<()>,
    bot_user: &User,
    channel_map: &Mutex<HashMap<i64, i32>>,
) {
    let message = &cx.update;
    match &message.kind {
        MessageKind::NewChatMembers(message) => {
            if message.new_chat_members.iter().any(|u| u.id == bot_user.id) {
                cx.answer(START_MSG).await.log_on_error().await;
            }
        }
        MessageKind::Common(message_common) => {
            if let Some(User { id: 777000, .. }) = message_common.from {
                debounce_tx.send(()).await.unwrap();
                let mut channel_map = channel_map.lock().await;
                channel_map.insert(message.chat.id, message.id);
            }
        }
        _ => {}
    }
}
