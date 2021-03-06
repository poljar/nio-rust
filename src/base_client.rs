use std::collections::HashMap;

use crate::api::r0 as api;
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::room::member::{MemberEvent, MembershipState};
use crate::events::EventResult;
use crate::session::Session;
use std::sync::{Arc, RwLock};

pub type Token = String;
pub type RoomId = String;
pub type UserId = String;

#[derive(Debug)]
/// A Matrix room member.
pub struct RoomMember {
    /// The unique mxid of the user.
    pub user_id: UserId,
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The users power level.
    pub power_level: u8,
}

#[derive(Debug)]
/// A Matrix rooom.
pub struct Room {
    /// The unique id of the room.
    pub room_id: RoomId,
    /// The mxid of our own user.
    pub own_user_id: UserId,
    /// The mxid of the room creator.
    pub creator: Option<UserId>,
    /// The map of room members.
    pub members: HashMap<UserId, RoomMember>,
    /// A list of users that are currently typing.
    pub typing_users: Vec<UserId>,
    /// A flag indicating if the room is encrypted.
    pub encrypted: bool,
}

impl Room {
    /// Create a new room.
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room.
    /// * `own_user_id` - The mxid of our own user.
    pub fn new(room_id: &str, own_user_id: &str) -> Self {
        Room {
            room_id: room_id.to_string(),
            own_user_id: own_user_id.to_owned(),
            creator: None,
            members: HashMap::new(),
            typing_users: Vec::new(),
            encrypted: false,
        }
    }

    fn add_member(&mut self, event: &MemberEvent) -> bool {
        if self.members.contains_key(&event.state_key) {
            return false;
        }

        let member = RoomMember {
            user_id: event.state_key.clone(),
            display_name: event.content.displayname.clone(),
            avatar_url: event.content.avatar_url.clone(),
            power_level: 0,
        };

        self.members.insert(event.state_key.clone(), member);

        true
    }

    fn remove_member(&mut self, event: &MemberEvent) -> bool {
        if !self.members.contains_key(&event.state_key) {
            return false;
        }

        true
    }

    fn update_joined_member(&mut self, event: &MemberEvent) -> bool {
        if let Some(member) = self.members.get_mut(&event.state_key) {
            member.display_name = event.content.displayname.clone();
            member.avatar_url = event.content.avatar_url.clone();
        }

        false
    }

    fn handle_join(&mut self, event: &MemberEvent) -> bool {
        match &event.prev_content {
            Some(c) => match c.membership {
                MembershipState::Join => self.update_joined_member(event),
                MembershipState::Invite => self.add_member(event),
                MembershipState::Leave => self.remove_member(event),
                _ => false,
            },
            None => self.add_member(event),
        }
    }

    fn handle_leave(&mut self, _event: &MemberEvent) -> bool {
        false
    }

    /// Handle a room.member updating the room state if necessary.
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &MemberEvent) -> bool {
        match event.content.membership {
            MembershipState::Join => self.handle_join(event),
            MembershipState::Leave => self.handle_leave(event),
            MembershipState::Ban => self.handle_leave(event),
            MembershipState::Invite => false,
            MembershipState::Knock => false,
            _ => false,
        }
    }

    /// Receive a timeline event for this room and update the room state.
    /// # Arguments
    ///
    /// `event` - The event of the room.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn receive_timeline_event(&mut self, event: &RoomEvent) -> bool {
        match event {
            RoomEvent::RoomMember(m) => self.handle_membership(m),
            _ => false,
        }
    }

    /// Receive a state event for this room and update the room state.
    /// # Arguments
    ///
    /// `event` - The event of the room.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn receive_state_event(&mut self, event: &StateEvent) -> bool {
        match event {
            StateEvent::RoomMember(m) => self.handle_membership(m),
            _ => false,
        }
    }
}

#[derive(Debug)]
/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates it's state.
pub struct Client {
    /// The current client session containing our user id, device id and access
    /// token.
    pub session: Option<Session>,
    /// The current sync token that should be used for the next sync call.
    pub sync_token: Option<Token>,
    /// A map of the rooms our user is joined in.
    pub joined_rooms: HashMap<RoomId, Arc<RwLock<Room>>>,
}

impl Client {
    /// Create a new client.
    /// # Arguments
    ///
    /// `session` - An optional session if the user already has one from a
    /// previous login call.
    pub fn new(session: Option<Session>) -> Self {
        Client {
            session,
            sync_token: None,
            joined_rooms: HashMap::new(),
        }
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.session.is_some()
    }

    /// Receive a login response and update the session of the client.
    /// # Arguments
    ///
    /// `response` - A successful login response that contains our access token
    /// and device id.
    pub fn receive_login_response(&mut self, response: &api::session::login::Response) {
        let session = Session {
            access_token: response.access_token.clone(),
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };
        self.session = Some(session);
    }

    fn get_or_create_room(&mut self, room_id: &str) -> &mut Arc<RwLock<Room>> {
        #[allow(clippy::or_fun_call)]
        self.joined_rooms
            .entry(room_id.to_string())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id
                    .to_string(),
            ))))
    }

    /// Receive a timeline event for a joined room and update the client state.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room the event belongs to.
    /// `event` - The event that should be handled by the client.
    ///
    /// Returns true if the membership list of the room changed, false
    /// otherwise.
    pub fn receive_joined_timeline_event(
        &mut self,
        room_id: &str,
        event: &EventResult<RoomEvent>,
    ) -> bool {
        match event {
            EventResult::Ok(e) => {
                let mut room = self.get_or_create_room(room_id).write().unwrap();
                room.receive_timeline_event(e)
            }
            _ => false,
        }
    }

    /// Receive a state event for a joined room and update the client state.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room the event belongs to.
    /// `event` - The event that should be handled by the client.
    ///
    /// Returns true if the membership list of the room changed, false
    /// otherwise.
    pub fn receive_joined_state_event(&mut self, room_id: &str, event: &StateEvent) -> bool {
        let mut room = self.get_or_create_room(room_id).write().unwrap();
        room.receive_state_event(event)
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// `response` - The response that we received after a successful sync.
    pub fn receive_sync_response(&mut self, response: &api::sync::sync_events::IncomingResponse) {
        self.sync_token = Some(response.next_batch.clone());
    }
}
