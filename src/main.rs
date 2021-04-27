use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use teloxide::{ApiError, RequestError, adaptors::{throttle::Limits, Throttle}, payloads::SendMessageSetters};
use teloxide::prelude::*;
use teloxide::types::MessageKind;
use tokio::sync::{mpsc, Mutex};

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "INFO");
    }
    pretty_env_logger::init();
    log::info!("Starting unpinnerbot...");

    let (debounce_tx, mut debounce_rx) = mpsc::channel(100);

    let bot = Bot::from_env().throttle(Limits::default()).auto_send();
    let bot_user = bot.get_me().await.unwrap();

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
                        is_there_work = true;
                        log::debug!("debouncing");
                    }
                    Ok(None) => {
                        log::debug!("stopping");
                        break;
                    }
                    Err(_) if is_there_work => {
                        log::debug!("done debouncing");

                        let mut channel_map = channel_map.lock().await;
                        for (&group, &msg_id) in channel_map.iter() {
                            match bot.unpin_chat_message(group).message_id(msg_id).await {
                                Ok(..) => {}
                                Err(e) => match &e {
                                    RequestError::ApiError { kind , .. } => match kind {
                                        ApiError::Unknown(s) if s == "Bad Request: not enough rights to manage pinned messages in the chat" => {
                                            bot.send_message(group, "Failed to unpin this message. Please, grant me Pin Messages permission to work properly.").reply_to_message_id(msg_id).await.log_on_error().await;
                                        }
                                        _ => {
                                            log::error!("Error: {}", e);
                                        }
                                    }
                                    _ => {
                                        log::error!("Error: {}", e);
                                    }
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
        .messages_handler(
            move |mut rx: DispatcherHandlerRx<AutoSend<Throttle<Bot>>, Message>| {
                let channel_map = Arc::clone(&channel_map);

                async move {
                    while let Some(m) = rx.recv().await {
                        let update = &m.update;

                        if !update.chat.is_supergroup() {
                            continue;
                        }

                        match &update.kind {
                            MessageKind::NewChatMembers(message) => {
                                if message.new_chat_members.iter().any(|u| u.id == bot_user.user.id) {
                                    m.answer("I automatically unpin messages sent from the linked channel. Don't forget to grant me Pin Messages permission to work.").await.log_on_error().await;
                                }
                            }
                            MessageKind::Common(message) => {
                                if update.chat.is_supergroup()
                                    && message.from.as_ref().filter(|u| u.id == 777000).is_some()
                                {
                                    log::debug!("{:#?}", message);
                                    debounce_tx.send(()).await.unwrap();
                                    let mut channel_map = channel_map.lock().await;
                                    channel_map.insert(update.chat.id, update.id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            },
        )
        .dispatch()
        .await;

    debounce_task.await.unwrap();
}
