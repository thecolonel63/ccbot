extern crate core;

mod discord;
mod tcp;

use anyhow::{anyhow, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const USERS_FILE: &str = "./users.json";

macro_rules! log {
    ($($arg:tt)*) => {{
        let time = chrono::offset::Local::now();
        println!("{} {}", time.format("[%Y-%m-%d %H:%M:%S]"), format!($($arg)*));
    }};
}

pub(crate) use log;

#[tokio::main]
async fn main() -> Result<()> {
    let mut random = rand::rng();

    let (main_tx, mut main_rx) = unbounded_channel();

    let discord_tx = main_tx.clone();
    tokio::spawn(async move {
        if let Err(why) = discord::start_discord(discord_tx).await {
            log!("Error in discord handler: {why:?}")
        }
    });

    let tcp_tx = main_tx.clone();
    tokio::spawn(async move {
        if let Err(why) = tcp::start_tcp(tcp_tx).await {
            log!("Error in tcp handler: {why:?}");
        }
    });

    let mut user_states = if let Ok(mut file) = File::open(USERS_FILE) {
        serde_json::from_reader(&mut file)?
    } else {
        Vec::<UserState>::new()
    };

    let mut dirty = true;

    log!("Waiting for clients...");

    loop {
        if let Ok(mut channel) = main_rx.try_recv() {
            let packet = channel
                .receiver
                .recv()
                .await
                .ok_or(anyhow!("Main packet channel closed!"))?;

            match packet {
                Packet::ConnectQuery(name, uuid) => {
                    // Insert a new code if there isn't one already
                    if !user_states.iter().any(|state| state.uuid == uuid) {
                        let mut code;
                        loop {
                            code = random.random_range(100000..1000000);
                            if !user_states
                                .iter()
                                .any(|state| state.verify_code == Some(code))
                            {
                                break;
                            }
                        }
                        user_states.push(UserState::new(&name, &uuid, code));
                    }

                    // Send the verification message back. If the user is verified, send nothing.
                    let state = user_states.iter().find(|state| state.uuid == uuid).unwrap();
                    match state.verify_state {
                        VerifyState::NEW => {
                            let code = state.verify_code.unwrap();
                            let response = format!(
                                "Please type the following code into the #verification channel:\n{code}"
                            );
                            log!("Disconnecting user {name} [{uuid}]: {response}");
                            channel.sender.send(Packet::ConnectResponse(response))?;
                        }

                        VerifyState::PENDING => {
                            let response = "Your account is currently pending admin approval. Please try again later.".to_owned();
                            log!("Disconnecting user {name} [{uuid}]: {response}");
                            channel.sender.send(Packet::ConnectResponse(response))?;
                        }

                        VerifyState::APPROVED => {
                            log!("User {name} [{uuid}] is verified.");
                            channel
                                .sender
                                .send(Packet::ConnectResponse(String::new()))?;
                        }
                    }
                }

                Packet::DiscordCode(code, user) => {
                    // Prevent duplicate registrations per discord user
                    if user_states
                        .iter()
                        .any(|state| state.discord_id == Some(user))
                    {
                        channel.sender.send(Packet::AlreadyLinked)?;
                        continue;
                    }

                    // If we found a matching code, send the info back, otherwise send an error.
                    match user_states.iter_mut().find(|state| {
                        state.verify_code == Some(code) && state.verify_state == VerifyState::NEW
                    }) {
                        Some(state) => {
                            log!(
                                "User {} [{}] is linking to discord account with ID {user}",
                                state.name,
                                state.uuid
                            );
                            state.discord_id = Some(user);
                            state.verify_state = VerifyState::PENDING;
                            state.verify_code = None;
                            state.code_expires = None;
                            channel.sender.send(Packet::VerifyPending(
                                state.uuid.to_owned(),
                                state.name.to_owned(),
                            ))?;

                            // Read verification message ID that got created
                            let Packet::LinkVerifyMessage(message_id) =
                                channel.receiver.recv().await.ok_or(anyhow!(
                                    "Thread did not send linked verify message id!"
                                ))?
                            else {
                                return Err(anyhow!(
                                    "Unexpected packet received instead of linked verify message id!"
                                ));
                            };
                            state.verify_message = Some(message_id);

                            dirty = true;
                        }

                        None => {
                            channel.sender.send(Packet::VerifyCodeInvalid)?;
                        }
                    }
                }

                // Set state to approved.
                Packet::DiscordApproval(uuid) => {
                    if let Some(state) = user_states.iter_mut().find(|state| state.uuid == uuid) {
                        log!(
                            "Successfully linked user {} [{}] to discord account with ID {}",
                            state.name,
                            state.uuid,
                            state.discord_id.unwrap()
                        );
                        channel.sender.send(Packet::ApprovalSuccess)?;
                        state.verify_state = VerifyState::APPROVED;
                        dirty = true;
                    } else {
                        channel.sender.send(Packet::ApprovalFailure)?;
                    }
                }

                // Remove the verification message
                Packet::RemoveUser(id) => {
                    if let Some(state) = user_states
                        .iter()
                        .filter(|state| state.discord_id == Some(id))
                        .next()
                    {
                        log!(
                            "Unlinking user {} [{}] from discord account with ID {}",
                            state.name,
                            state.uuid,
                            state.discord_id.unwrap()
                        );
                        channel
                            .sender
                            .send(Packet::RemoveMessage(state.verify_message.unwrap()))?;
                    }

                    user_states.retain(|state| state.discord_id != Some(id));
                    dirty = true;
                }

                x => return Err(anyhow!("Unexpected packet {x:?} received in main loop!")),
            }
        }

        // Remove expired codes
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();
        user_states.retain(|state| state.code_expires.is_none_or(|expired| expired > time));

        // Update the config file
        if dirty {
            let _ = std::fs::remove_file(USERS_FILE);
            let mut file = File::create_new(USERS_FILE)?;
            serde_json::to_writer_pretty(
                &mut file,
                &user_states
                    .iter()
                    .filter(|state| {
                        matches!(
                            state.verify_state,
                            VerifyState::PENDING | VerifyState::APPROVED
                        )
                    })
                    .collect::<Vec<&UserState>>(),
            )?;
            dirty = false;
        }
    }
}

struct ChannelPair<T> {
    sender: UnboundedSender<T>,
    receiver: UnboundedReceiver<T>,
}

impl<T> ChannelPair<T> {
    fn new() -> Self {
        let (sender, receiver) = unbounded_channel();
        Self { sender, receiver }
    }

    // Generate an entangled partner that can be sent over a channel.
    fn entangle(&mut self) -> ChannelPair<T> {
        let mut other = ChannelPair::<T>::new();
        std::mem::swap(&mut self.receiver, &mut other.receiver);
        other
    }
}

#[derive(Serialize, Deserialize)]
struct UserState {
    name: String,
    uuid: String,
    discord_id: Option<u64>,
    verify_state: VerifyState,
    verify_message: Option<u64>,

    #[serde(skip_serializing, skip_deserializing)]
    verify_code: Option<i32>,
    #[serde(skip_serializing, skip_deserializing)]
    code_expires: Option<u128>,
}

impl UserState {
    fn new(name: &String, uuid: &String, code: i32) -> Self {
        Self {
            name: name.clone(),
            uuid: uuid.clone(),
            discord_id: None,
            verify_state: VerifyState::NEW,
            verify_message: None,
            verify_code: Some(code),
            code_expires: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis()
                    + (1000 * 30),
            ),
        }
    }
}

#[derive(Eq, PartialEq, Serialize, Deserialize)]
enum VerifyState {
    NEW,
    PENDING,
    APPROVED,
}

#[derive(Debug)]
enum Packet {
    ConnectQuery(String, String),
    ConnectResponse(String),
    DiscordCode(i32, u64),
    DiscordApproval(String),
    VerifyPending(String, String),
    LinkVerifyMessage(u64),
    AlreadyLinked,
    VerifyCodeInvalid,
    RemoveUser(u64),
    RemoveMessage(u64),
    ApprovalSuccess,
    ApprovalFailure,
}
