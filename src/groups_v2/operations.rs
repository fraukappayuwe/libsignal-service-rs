use std::convert::TryInto;

use base64::prelude::*;
use bytes::Bytes;
use libsignal_protocol::{Aci, ServiceId};
use prost::Message;
use zkgroup::{
    groups::GroupSecretParams,
    profiles::{AnyProfileKeyCredentialPresentation, ProfileKey},
};

use crate::{
    groups_v2::model::Timer,
    proto::{
        self, group_attribute_blob, GroupAttributeBlob,
        Member as EncryptedMember,
    },
    utils::BASE64_RELAXED,
};

use super::{
    model::{Member, PendingMember, RequestingMember},
    Group, GroupChange, GroupChanges,
};

pub(crate) struct GroupOperations {
    pub group_secret_params: GroupSecretParams,
}

#[derive(Debug, thiserror::Error)]
pub enum GroupDecodingError {
    #[error("zero-knowledge group deserialization failure")]
    ZkGroupDeserializationFailure,
    #[error("zero-knowledge group verification failure")]
    ZkGroupVerificationFailure,
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error("protobuf message decoding error: {0}")]
    ProtobufDecodeError(#[from] prost::DecodeError),
    #[error("wrong group attribute blob")]
    WrongBlob,
    #[error("wrong enum value")]
    WrongEnumValue,
    #[error("wrong service ID type: should be ACI")]
    NotAci,
}

impl From<zkgroup::ZkGroupDeserializationFailure> for GroupDecodingError {
    fn from(_: zkgroup::ZkGroupDeserializationFailure) -> Self {
        GroupDecodingError::ZkGroupDeserializationFailure
    }
}

impl From<zkgroup::ZkGroupVerificationFailure> for GroupDecodingError {
    fn from(_: zkgroup::ZkGroupVerificationFailure) -> Self {
        GroupDecodingError::ZkGroupVerificationFailure
    }
}

impl GroupOperations {
    fn decrypt_service_id(
        &self,
        ciphertext: &[u8],
    ) -> Result<ServiceId, GroupDecodingError> {
        match self
            .group_secret_params
            .decrypt_service_id(bincode::deserialize(ciphertext)?)?
        {
            ServiceId::Aci(aci) => Ok(ServiceId::from(aci)),
            ServiceId::Pni(pni) => Ok(ServiceId::from(pni)),
        }
    }

    fn decrypt_aci(
        &self,
        ciphertext: &[u8],
    ) -> Result<Aci, GroupDecodingError> {
        match self
            .group_secret_params
            .decrypt_service_id(bincode::deserialize(ciphertext)?)?
        {
            ServiceId::Aci(aci) => Ok(aci),
            ServiceId::Pni(_pni) => Err(GroupDecodingError::NotAci),
        }
    }

    fn decrypt_profile_key(
        &self,
        encrypted_profile_key: &[u8],
        decrypted_aci: libsignal_protocol::Aci,
    ) -> Result<ProfileKey, GroupDecodingError> {
        Ok(self.group_secret_params.decrypt_profile_key(
            bincode::deserialize(encrypted_profile_key)?,
            decrypted_aci,
        )?)
    }

    fn decrypt_profile_key_presentation(
        &self,
        presentation: &[u8],
    ) -> Result<(Aci, ProfileKey), GroupDecodingError> {
        let profile_key_credential_presentation =
            AnyProfileKeyCredentialPresentation::new(presentation)?;

        match self.group_secret_params.decrypt_service_id(
            profile_key_credential_presentation.get_uuid_ciphertext(),
        )? {
            ServiceId::Aci(aci) => {
                let profile_key =
                    self.group_secret_params.decrypt_profile_key(
                        profile_key_credential_presentation
                            .get_profile_key_ciphertext(),
                        aci,
                    )?;
                Ok((aci, profile_key))
            },
            _ => Err(GroupDecodingError::NotAci),
        }
    }

    fn decrypt_member(
        &self,
        member: EncryptedMember,
    ) -> Result<Member, GroupDecodingError> {
        let (aci, profile_key) = if member.presentation.is_empty() {
            let aci = self.decrypt_aci(&member.user_id)?;
            let profile_key =
                self.decrypt_profile_key(&member.profile_key, aci)?;
            (aci, profile_key)
        } else {
            self.decrypt_profile_key_presentation(&member.presentation)?
        };
        Ok(Member {
            uuid: aci.into(),
            profile_key,
            role: member.role.try_into()?,
            joined_at_revision: member.joined_at_revision,
        })
    }

    fn decrypt_pending_member(
        &self,
        member: proto::PendingMember,
    ) -> Result<PendingMember, GroupDecodingError> {
        let inner_member =
            member.member.ok_or(GroupDecodingError::WrongBlob)?;
        let service_id = self.decrypt_service_id(&inner_member.user_id)?;
        let added_by_uuid = self.decrypt_aci(&member.added_by_user_id)?;

        Ok(PendingMember {
            address: service_id,
            role: inner_member.role.try_into()?,
            added_by_uuid: added_by_uuid.into(),
            timestamp: member.timestamp,
        })
    }

    fn decrypt_requesting_member(
        &self,
        member: proto::RequestingMember,
    ) -> Result<RequestingMember, GroupDecodingError> {
        let (aci, profile_key) = if member.presentation.is_empty() {
            let aci = self.decrypt_aci(&member.user_id)?;
            let profile_key =
                self.decrypt_profile_key(&member.profile_key, aci)?;
            (aci, profile_key)
        } else {
            self.decrypt_profile_key_presentation(&member.presentation)?
        };
        Ok(RequestingMember {
            profile_key,
            uuid: aci.into(),
            timestamp: member.timestamp,
        })
    }

    fn decrypt_blob(&self, bytes: &[u8]) -> GroupAttributeBlob {
        if bytes.is_empty() {
            GroupAttributeBlob::default()
        } else if bytes.len() < 29 {
            tracing::warn!("bad encrypted blob length");
            GroupAttributeBlob::default()
        } else {
            self.group_secret_params
                .decrypt_blob(bytes)
                .map_err(GroupDecodingError::from)
                .and_then(|b| {
                    GroupAttributeBlob::decode(Bytes::copy_from_slice(&b[4..]))
                        .map_err(GroupDecodingError::ProtobufDecodeError)
                })
                .unwrap_or_else(|e| {
                    tracing::warn!("bad encrypted blob: {}", e);
                    GroupAttributeBlob::default()
                })
        }
    }

    fn decrypt_title(&self, ciphertext: &[u8]) -> String {
        use group_attribute_blob::Content;
        match self.decrypt_blob(ciphertext).content {
            Some(Content::Title(title)) => title,
            _ => "".into(),
        }
    }

    fn decrypt_description(&self, ciphertext: &[u8]) -> Option<String> {
        use group_attribute_blob::Content;
        match self.decrypt_blob(ciphertext).content {
            Some(Content::Description(d)) => Some(d).filter(|d| !d.is_empty()),
            _ => None,
        }
    }

    fn decrypt_disappearing_message_timer(
        &self,
        ciphertext: &[u8],
    ) -> Option<Timer> {
        use group_attribute_blob::Content;
        match self.decrypt_blob(ciphertext).content {
            Some(Content::DisappearingMessagesDuration(duration)) => {
                Some(Timer { duration })
            },
            _ => None,
        }
    }

    pub fn new(group_secret_params: GroupSecretParams) -> Self {
        Self {
            group_secret_params,
        }
    }

    pub fn decrypt_group(
        &self,
        group: proto::Group,
    ) -> Result<Group, GroupDecodingError> {
        let title = self.decrypt_title(&group.title);

        let description = self.decrypt_description(&group.description);

        let disappearing_messages_timer = self
            .decrypt_disappearing_message_timer(
                &group.disappearing_messages_timer,
            );

        let members = group
            .members
            .into_iter()
            .map(|m| self.decrypt_member(m))
            .collect::<Result<_, _>>()?;

        let pending_members = group
            .pending_members
            .into_iter()
            .map(|m| self.decrypt_pending_member(m))
            .collect::<Result<_, _>>()?;

        let requesting_members = group
            .requesting_members
            .into_iter()
            .map(|m| self.decrypt_requesting_member(m))
            .collect::<Result<_, _>>()?;

        Ok(Group {
            title,
            avatar: group.avatar,
            disappearing_messages_timer,
            access_control: group
                .access_control
                .map(TryInto::try_into)
                .transpose()?,
            revision: group.revision,
            members,
            pending_members,
            requesting_members,
            invite_link_password: group.invite_link_password,
            description,
        })
    }

    pub fn decrypt_group_change(
        &self,
        group_change: proto::GroupChange,
    ) -> Result<GroupChanges, GroupDecodingError> {
        let actions: proto::group_change::Actions =
            Message::decode(Bytes::from(group_change.actions))?;

        let aci = self.decrypt_aci(&actions.source_service_id)?;

        let new_members =
            actions.add_members.into_iter().filter_map(|m| m.added).map(
                |added| Ok(GroupChange::NewMember(self.decrypt_member(added)?)),
            );

        let delete_members = actions.delete_members.into_iter().map(|c| {
            Ok(GroupChange::DeleteMember(
                self.decrypt_aci(&c.deleted_user_id)?.into(),
            ))
        });

        let modify_member_roles =
            actions.modify_member_roles.into_iter().map(|m| {
                Ok(GroupChange::ModifyMemberRole {
                    uuid: self.decrypt_aci(&m.user_id)?.into(),
                    role: m.role.try_into()?,
                })
            });

        let modify_member_profile_keys =
            actions.modify_member_profile_keys.into_iter().map(|m| {
                let (aci, profile_key) =
                    self.decrypt_profile_key_presentation(&m.presentation)?;
                Ok(GroupChange::ModifyMemberProfileKey {
                    uuid: aci.into(),
                    profile_key,
                })
            });

        let add_pending_members = actions
            .add_pending_members
            .into_iter()
            .filter_map(|m| m.added)
            .map(|added| {
                Ok(GroupChange::NewPendingMember(
                    self.decrypt_pending_member(added)?,
                ))
            });

        let delete_pending_members =
            actions.delete_pending_members.into_iter().map(|m| {
                Ok(GroupChange::DeletePendingMember(
                    self.decrypt_aci(&m.deleted_user_id)?.into(),
                ))
            });

        let promote_pending_members =
            actions.promote_pending_members.into_iter().map(|m| {
                let (aci, profile_key) =
                    self.decrypt_profile_key_presentation(&m.presentation)?;
                Ok(GroupChange::PromotePendingMember {
                    uuid: aci.into(),
                    profile_key,
                })
            });

        let modify_title = actions
            .modify_title
            .into_iter()
            .map(|m| Ok(GroupChange::Title(self.decrypt_title(&m.title))));

        let modify_avatar = actions
            .modify_avatar
            .into_iter()
            .map(|m| Ok(GroupChange::Avatar(m.avatar)));

        let modify_description =
            actions.modify_description.into_iter().map(|m| {
                Ok(GroupChange::Description(
                    self.decrypt_description(&m.description),
                ))
            });

        let modify_disappearing_messages_timer = actions
            .modify_disappearing_messages_timer
            .into_iter()
            .map(|m| {
                Ok(GroupChange::Timer(
                    self.decrypt_disappearing_message_timer(&m.timer),
                ))
            });

        let modify_attributes_access =
            actions.modify_attributes_access.into_iter().map(|m| {
                Ok(GroupChange::AttributeAccess(
                    m.attributes_access.try_into()?,
                ))
            });

        let modify_member_access =
            actions.modify_member_access.into_iter().map(|m| {
                Ok(GroupChange::MemberAccess(m.members_access.try_into()?))
            });

        let modify_add_from_invite_link_access = actions
            .modify_add_from_invite_link_access
            .into_iter()
            .map(|m| {
                Ok(GroupChange::InviteLinkAccess(
                    m.add_from_invite_link_access.try_into()?,
                ))
            });

        let add_requesting_members = actions
            .add_requesting_members
            .into_iter()
            .filter_map(|m| m.added)
            .map(|added| {
                Ok(GroupChange::NewRequestingMember(
                    self.decrypt_requesting_member(added)?,
                ))
            });

        let delete_requesting_members =
            actions.delete_requesting_members.into_iter().map(|m| {
                Ok(GroupChange::DeleteRequestingMember(
                    self.decrypt_aci(&m.deleted_user_id)?.into(),
                ))
            });

        let promote_requesting_members =
            actions.promote_requesting_members.into_iter().map(|m| {
                Ok(GroupChange::PromoteRequestingMember {
                    uuid: self.decrypt_aci(&m.user_id)?.into(),
                    role: m.role.try_into()?,
                })
            });

        let modify_invite_link_password =
            actions.modify_invite_link_password.into_iter().map(|m| {
                Ok(GroupChange::InviteLinkPassword(
                    BASE64_RELAXED.encode(m.invite_link_password),
                ))
            });

        let modify_announcements_only = actions
            .modify_announcements_only
            .into_iter()
            .map(|m| Ok(GroupChange::AnnouncementOnly(m.announcements_only)));

        let changes: Result<Vec<GroupChange>, GroupDecodingError> = new_members
            .chain(delete_members)
            .chain(modify_member_roles)
            .chain(modify_member_profile_keys)
            .chain(add_pending_members)
            .chain(delete_pending_members)
            .chain(promote_pending_members)
            .chain(modify_title)
            .chain(modify_avatar)
            .chain(modify_disappearing_messages_timer)
            .chain(modify_attributes_access)
            .chain(modify_description)
            .chain(modify_member_access)
            .chain(modify_add_from_invite_link_access)
            .chain(add_requesting_members)
            .chain(delete_requesting_members)
            .chain(promote_requesting_members)
            .chain(modify_invite_link_password)
            .chain(modify_announcements_only)
            .collect();

        Ok(GroupChanges {
            editor: aci.into(),
            revision: actions.revision,
            changes: changes?,
        })
    }

    pub fn decrypt_avatar(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        use group_attribute_blob::Content;
        match self.decrypt_blob(ciphertext).content {
            Some(Content::Avatar(d)) => Some(d).filter(|d| !d.is_empty()),
            _ => None,
        }
    }
}
