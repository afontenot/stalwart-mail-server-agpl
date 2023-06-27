/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart IMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::sync::Arc;

use imap_proto::{
    protocol::append::Arguments, receiver::Request, Command, ResponseCode, StatusResponse,
};

use jmap::email::ingest::IngestEmail;
use jmap_proto::types::{acl::Acl, keyword::Keyword, state::StateChange, type_state::TypeState};
use mail_parser::Message;
use tokio::io::AsyncRead;

use crate::core::{MailboxId, SelectedMailbox, Session, SessionData};

impl<T: AsyncRead> Session<T> {
    pub async fn handle_append(&mut self, request: Request<Command>) -> Result<(), ()> {
        match request.parse_append() {
            Ok(arguments) => {
                let (data, selected_mailbox) = self.state.session_mailbox_state();

                // Refresh mailboxes
                if let Err(err) = data.synchronize_mailboxes(false).await {
                    return self
                        .write_bytes(err.with_tag(arguments.tag).into_bytes())
                        .await;
                }

                // Obtain mailbox
                let mailbox =
                    if let Some(mailbox) = data.get_mailbox_by_name(&arguments.mailbox_name) {
                        if mailbox.mailbox_id.is_some() {
                            mailbox
                        } else {
                            return self
                                .write_bytes(
                                    StatusResponse::no(
                                        "Appending messages to this mailbox is not allowed.",
                                    )
                                    .with_tag(arguments.tag)
                                    .with_code(ResponseCode::Cannot)
                                    .into_bytes(),
                                )
                                .await;
                        }
                    } else {
                        return self
                            .write_bytes(
                                StatusResponse::no("Mailbox does not exist.")
                                    .with_tag(arguments.tag)
                                    .with_code(ResponseCode::TryCreate)
                                    .into_bytes(),
                            )
                            .await;
                    };
                let is_qresync = self.is_qresync;

                tokio::spawn(async move {
                    data.write_bytes(
                        match data
                            .append_messages(arguments, selected_mailbox, mailbox, is_qresync)
                            .await
                        {
                            Ok(response) => response,
                            Err(response) => response,
                        }
                        .into_bytes(),
                    )
                    .await;
                });
                Ok(())
            }
            Err(response) => self.write_bytes(response.into_bytes()).await,
        }
    }
}

impl SessionData {
    async fn append_messages(
        &self,
        arguments: Arguments,
        selected_mailbox: Option<Arc<SelectedMailbox>>,
        mailbox: MailboxId,
        is_qresync: bool,
    ) -> crate::op::Result<StatusResponse> {
        // Verify ACLs
        let account_id = mailbox.account_id;
        let mailbox_id = mailbox.mailbox_id.unwrap();
        if !self
            .check_mailbox_acl(account_id, mailbox_id, Acl::AddItems)
            .await
            .map_err(|r| r.with_tag(&arguments.tag))?
        {
            return Ok(StatusResponse::no(
                "You do not have the required permissions to append messages to this mailbox.",
            )
            .with_tag(arguments.tag)
            .with_code(ResponseCode::NoPerm));
        }

        // Obtain quota
        let account_quota = self
            .get_access_token()
            .await
            .map_err(|r| r.with_tag(&arguments.tag))?
            .quota as i64;

        // Append messages
        let mut response = StatusResponse::completed(Command::Append);
        let mut created_ids = Vec::with_capacity(arguments.messages.len());
        let mut last_change_id = None;
        for message in arguments.messages {
            match self
                .jmap
                .email_ingest(IngestEmail {
                    raw_message: &message.message,
                    message: Message::parse(&message.message),
                    account_id,
                    account_quota,
                    mailbox_ids: vec![mailbox_id],
                    keywords: message.flags.into_iter().map(Keyword::from).collect(),
                    received_at: message.received_at.map(|d| d as u64),
                    skip_duplicates: false,
                })
                .await
            {
                Ok(email) => {
                    created_ids.push(email.id.document_id());
                    last_change_id = Some(email.change_id);
                }
                Err(err) => {
                    match err {
                        jmap::IngestError::Temporary => {
                            response = StatusResponse::database_failure();
                        }
                        jmap::IngestError::OverQuota => {
                            response = StatusResponse::no("Disk quota exceeded.")
                                .with_code(ResponseCode::OverQuota);
                        }
                        jmap::IngestError::Permanent { reason, .. } => {
                            response = StatusResponse::no(reason);
                        }
                    }
                    break;
                }
            }
        }

        // Broadcast changes
        if let Some(change_id) = last_change_id {
            self.jmap
                .broadcast_state_change(
                    StateChange::new(account_id)
                        .with_change(TypeState::Email, change_id)
                        .with_change(TypeState::Mailbox, change_id)
                        .with_change(TypeState::Thread, change_id),
                )
                .await;
        }

        if !created_ids.is_empty() {
            let (uids, uid_validity) = match selected_mailbox {
                Some(selected_mailbox) if selected_mailbox.id == mailbox => {
                    self.synchronize_messages(&selected_mailbox, is_qresync, true)
                        .await
                        .map_err(|r| r.with_tag(&arguments.tag))?;
                    let mailbox = selected_mailbox.state.lock();
                    (
                        created_ids
                            .into_iter()
                            .filter_map(|id| mailbox.id_to_imap.get(&id))
                            .map(|id| id.uid)
                            .collect(),
                        mailbox.uid_validity,
                    )
                }

                _ => {
                    let mailbox = self
                        .fetch_messages(&mailbox)
                        .await
                        .map_err(|r| r.with_tag(&arguments.tag))?;
                    (
                        created_ids
                            .into_iter()
                            .filter_map(|id| mailbox.id_to_imap.get(&id))
                            .map(|id| id.uid)
                            .collect(),
                        mailbox.uid_validity,
                    )
                }
            };
            response = response.with_code(ResponseCode::AppendUid { uid_validity, uids });
        }

        Ok(response.with_tag(arguments.tag))
    }
}
