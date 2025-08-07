use crate::{log, ChannelPair, Packet};
use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serenity::all::{ButtonStyle, ChannelId, ComponentInteraction, Context, CreateButton, CreateChannel, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EditChannel, EventHandler, GatewayIntents, GuildId, Http, Interaction, Member, Message, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId, User, UserId};
use serenity::{async_trait, Client};
use std::fs::File;
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

const DISCORD_CONFIG_PATH: &str = "./discord_config.json";
pub(crate) const PRIMARY_COLOR: u32 = 0x30F4B0;
const SECONDARY_COLOR: u32 = 0x50F3F1;
const ERROR_COLOR: u32 = 0xEF1E02;

struct Handler {
    sender: UnboundedSender<ChannelPair<Packet>>,
    config: DiscordConfig,
}

impl Handler {
    fn new(sender: UnboundedSender<ChannelPair<Packet>>, config: DiscordConfig) -> Self {
        Self {
            sender,
            config,
        }
    }

    async fn handle_verify_message(&self, ctx: Context, msg: Message) -> Result<()> {
        // Create a new message when told.
        if msg.content == "!msg" && msg.author.has_role(&ctx.http, self.config.guild_id, self.config.moderator_role_id).await? {
            msg.channel_id.send_message(&ctx.http, CreateMessage::new()
                .embed(CreateEmbed::new()
                    .title("CloverCraft SMP")
                    .description("Welcome to the CloverCraft SMP! To verify your account, please join the Minecraft server and type the code it gives you into this channel. You will not be able to play until you have verified your account and an admin has approved it. The bot will DM you in order to confirm your verification statuses.")
                    .color(PRIMARY_COLOR)
                ),
            ).await?;
        }

        // Parse a code - we can't verify it here, so send it to the main thread.
        if let Ok(code) = i32::from_str(&msg.content) && matches!(code, (100000..1000000)) {
            let mut local_pair = ChannelPair::new();
            self.sender.send(local_pair.entangle())?;

            local_pair.sender.send(Packet::DiscordCode(code, msg.author.id.get()))?;

            // Check if the code worked.
            let packet = local_pair.receiver.recv().await.ok_or(anyhow!("Main thread did not reply to discord bot!"))?;
            match packet {
                // The code was valid - send the user a direct message and send the approval message in the members channel.
                Packet::VerifyPending(uuid, name) => {
                    let _ = msg.author.direct_message(&ctx.http, CreateMessage::new().embed(
                        CreateEmbed::new()
                            .title("CloverCraft SMP")
                            .description("Your whitelist status has been updated.")
                            .field("Status", "Pending", false)
                            .color(SECONDARY_COLOR)
                    )).await;

                    let channels = msg.guild_id.unwrap().channels(&ctx.http).await?;
                    let channel = channels.get(&ChannelId::new(self.config.member_channel_id)).unwrap();
                    let discord_id = msg.author.id.get();
                    let message = channel.send_message(&ctx.http,
                                                       CreateMessage::new()
                                                           .embed(
                                                               CreateEmbed::new()
                                                                   .thumbnail(format!("https://www.mc-heads.net/head/{uuid}.png"))
                                                                   .title("CloverCraft SMP")
                                                                   .field("Minecraft Name", &name, true)
                                                                   .field("Minecraft UUID", &uuid, true)
                                                                   .field("", "", true)
                                                                   .field("Discord User", format!("<@{discord_id}>"), true)
                                                                   .field("Discord ID", format!("{discord_id}"), true)
                                                                   .field("", "", true)
                                                                   .color(PRIMARY_COLOR)
                                                           )
                                                           .button(CreateButton::new(format!("approve-account-{discord_id}-{uuid}")).label("Approve")),
                    ).await?;
                    local_pair.sender.send(Packet::LinkVerifyMessage(message.id.get()))?;
                }

                // The code was invalid
                Packet::VerifyCodeInvalid => {
                    let _ = msg.author.direct_message(&ctx.http, CreateMessage::new().embed(
                        CreateEmbed::new()
                            .title("CloverCraft SMP")
                            .description("You did not send a valid verification code. Please ensure that you have typed the code exactly as it appeared in Minecraft.")
                            .color(ERROR_COLOR)
                    )).await;
                }

                // The user is already verifying
                Packet::AlreadyLinked => {
                    let _ = msg.author.direct_message(&ctx.http, CreateMessage::new().embed(
                        CreateEmbed::new()
                            .title("CloverCraft SMP")
                            .description("You cannot link more than one Minecraft account.")
                            .color(ERROR_COLOR)
                    )).await;
                }

                x => return Err(anyhow!("Unexpected packet {x:?} received on discord thread!")),
            }
        }

        // Delete non-bot messages.
        if !msg.author.bot {
            msg.delete(&ctx.http).await?;
        }

        Ok(())
    }

    async fn handle_ticket_message(&self, ctx: Context, msg: Message) -> Result<()> {
        // Create a new message when told.
        if msg.content == "!msg" && msg.author.has_role(&ctx.http, self.config.guild_id, self.config.moderator_role_id).await? {
            msg.channel_id.send_message(&ctx.http, CreateMessage::new()
                .embed(CreateEmbed::new()
                    .title("CloverCraft Tickets")
                    .description("If you need to discuss something in private with the team, this is the place. Simply press the 'Create Ticket' button below to open a new ticket. Be prepared to describe your issue once the ticket is open.")
                    .color(PRIMARY_COLOR)
                )
                .button(CreateButton::new("create-ticket")
                    .label("Create Ticket")
                ),
            ).await?;
        }

        // Delete non-bot messages.
        if !msg.author.bot {
            msg.delete(&ctx.http).await?;
        }

        Ok(())
    }

    async fn open_ticket(&self, http: &Arc<Http>, user: &User, component: &ComponentInteraction) -> Result<()> {
        // Create the new ticket channel and give the creator permission to see it.
        let ticket_channel = GuildId::new(self.config.guild_id).create_channel(http, CreateChannel::new(format!("ticket-{}", user.name)).category(self.config.active_ticket_category_id)).await?;
        ticket_channel.create_permission(http, PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Default::default(),
            kind: PermissionOverwriteType::Member(user.id),
        }).await?;

        // Create the initial message / close ticket button
        let initial_message = CreateMessage::new()
            .content(
                format!("<@{}> <@&{}>", user.id, self.config.moderator_role_id)
            )
            .embed(
                CreateEmbed::new()
                    .title("CloverCraft Ticket")
                    .description("Thank you for opening a ticket. Please describe your issue below. A staff member will reach out to help as soon as possible.")
                    .color(PRIMARY_COLOR)
            )
            .button(
                CreateButton::new(format!("close-ticket-{}", ticket_channel.id))
                    .label("Close Ticket")
            );

        ticket_channel.send_message(http, initial_message).await?;
        component.create_response(http, CreateInteractionResponse::Acknowledge).await?;
        Ok(())
    }

    async fn close_ticket(&self, http: &Arc<Http>, id: &str, component: &ComponentInteraction) -> Result<()> {
        let channel_id = ChannelId::new(u64::from_str(id.split_at(13).1)?);
        let mut channel = channel_id.to_channel(http).await?.guild().ok_or(anyhow!("Channel was not a guild channel!"))?;

        // Remove all custom permissions
        for permission_overwrite in channel.permission_overwrites.iter()
            .filter_map(|o| match o.kind {
                PermissionOverwriteType::Member(_) => Some(o.kind),
                _ => None,
            }) {
            channel.delete_permission(http, permission_overwrite).await?;
        }

        // Move the ticket into the archived tickets category, disable the close ticket button
        channel.edit(http, EditChannel::new().category(Some(ChannelId::new(self.config.archive_ticket_category_id)))).await?;
        component.create_response(http, CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().button(CreateButton::new("closed-ticket").label("Ticket closed").disabled(true)))).await?;
        Ok(())
    }

    async fn approve_account(&self, http: &Arc<Http>, id: &str, component: &ComponentInteraction) -> Result<()> {
        let regex = Regex::new(r"^approve-account-([0-9]+)-([0-9a-f]{8}-([0-9a-f]{4}-){3}[0-9a-f]{12})$")?;
        let captures = regex.captures(id).ok_or(anyhow!("Invalid button id!"))?;
        let discord_id = UserId::new(u64::from_str(&captures[1])?);
        let uuid = captures[2].to_owned();

        // Notify the main thread
        let mut pair = ChannelPair::new();
        self.sender.send(pair.entangle())?;
        pair.sender.send(Packet::DiscordApproval(uuid.clone()))?;

        // DM the user if it was successful
        if let Packet::ApprovalSuccess = pair.receiver.recv().await.ok_or(anyhow!("Main thread did not acknowledge approval!"))? {
            let _ = discord_id.direct_message(http, CreateMessage::new().embed(
                CreateEmbed::new()
                    .title("CloverCraft SMP")
                    .description("Your whitelist status has been updated.")
                    .field("Status", "Approved", false)
                    .color(PRIMARY_COLOR)
            )).await;

            http.add_member_role(GuildId::new(self.config.guild_id), discord_id, RoleId::new(self.config.verified_role_id), None).await?;
            component.create_response(http, CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .button(
                        CreateButton::new(format!("unlink-account-{discord_id}"))
                            .label("Unlink").style(ButtonStyle::Danger)
                    )
            )).await?;
        }

        Ok(())
    }

    async fn unlink_account(&self, http: &Arc<Http>, id: &str) -> Result<()> {
        let user_id = UserId::new(u64::from_str(id.split_at(15).1)?);
        self.handle_user_leave(http, user_id).await?;
        Ok(())
    }

    async fn handle_user_leave(&self, http: &Arc<Http>, user_id: UserId) -> Result<()> {
        // Tell the main thread to remove the user
        let mut local_pair = ChannelPair::new();
        self.sender.send(local_pair.entangle())?;
        local_pair.sender.send(Packet::RemoveUser(user_id.get()))?;

        // Remove the member message from the members channel
        if let Some(Packet::RemoveMessage(message_id)) = local_pair.receiver.recv().await {
            ChannelId::new(self.config.member_channel_id).delete_message(http, message_id).await?;
        }

        // Try to remove their role
        let _ = http.remove_member_role(GuildId::new(self.config.guild_id), user_id, RoleId::new(self.config.verified_role_id), None).await;
        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn guild_member_removal(&self, ctx: Context, guild_id: GuildId, user: User, _member_data_if_available: Option<Member>) {
        if guild_id == self.config.guild_id && let Err(why) = self.handle_user_leave(&ctx.http, user.id).await {
            log!("Error handling user removal: {why:?}");
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let verification_channel = self.config.verification_channel_id;
        let ticket_channel = self.config.ticket_channel_id;
        let channel_id = msg.channel_id.get();

        if verification_channel == channel_id {
            if let Err(why) = self.handle_verify_message(ctx, msg).await {
                log!("Error handling verification message: {why:?}");
            }
        } else if ticket_channel == channel_id {
            if let Err(why) = self.handle_ticket_message(ctx, msg).await {
                log!("Error handling verification message: {why:?}");
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Component(component) = interaction {
            let id = &component.data.custom_id;

            if id == "create-ticket" && let Err(why) = self.open_ticket(&ctx.http, &component.user, &component).await {
                log!("Error opening ticket: {why:?}");
            }

            if id.starts_with("close-ticket-") && let Err(why) = self.close_ticket(&ctx.http, &id, &component).await {
                log!("Error closing ticket: {why:?}");
            }

            if id.starts_with("approve-account-") && let Err(why) = self.approve_account(&ctx.http, &id, &component).await {
                log!("Error approving account: {why:?}");
            }

            if id.starts_with("unlink-account-") && let Err(why) = self.unlink_account(&ctx.http, &id).await {
                log!("Error unlinking account: {why:?}");
            }
        }
    }
}

pub async fn start_discord(discord_tx: UnboundedSender<ChannelPair<Packet>>) -> Result<()> {
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::GUILD_MEMBERS | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let config = open_config()?;
    if config.token.is_empty() {
        log!("Please complete the discord config before starting the discord bot program.");
        exit(0);
    }

    let mut client = Client::builder(config.token.clone(), intents)
        .event_handler(Handler::new(discord_tx, config))
        .await
        .expect("Error creating client!");

    log!("Starting discord client...");

    client.start().await?;

    Ok(())
}

#[derive(Serialize, Deserialize)]
struct DiscordConfig {
    token: String,
    guild_id: u64,
    verified_role_id: u64,
    moderator_role_id: u64,
    verification_channel_id: u64,
    member_channel_id: u64,
    ticket_channel_id: u64,
    active_ticket_category_id: u64,
    archive_ticket_category_id: u64,
}

impl DiscordConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            guild_id: 0,
            verified_role_id: 0,
            moderator_role_id: 0,
            verification_channel_id: 0,
            member_channel_id: 0,
            ticket_channel_id: 0,
            active_ticket_category_id: 0,
            archive_ticket_category_id: 0,
        }
    }
}

fn open_config() -> Result<DiscordConfig> {
    if let Ok(file) = File::open(DISCORD_CONFIG_PATH) {
        if let Ok(config) = serde_json::from_reader(file) {
            return Ok(config);
        }
    }

    let _ = std::fs::remove_file(DISCORD_CONFIG_PATH);
    let mut file = File::create_new(DISCORD_CONFIG_PATH)?;
    let config = DiscordConfig::default();
    serde_json::to_writer_pretty(&mut file, &config)?;
    Ok(config)
}