use futures::future::{BoxFuture, Future, FutureExt};
use std::convert::{TryFrom, TryInto};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_std::task::sleep;

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use js_int::UInt;
use reqwest::header::{HeaderValue, InvalidHeaderValue};
use url::Url;

use ruma_api::{Endpoint, Outgoing};
use ruma_events::collections::all::RoomEvent;
use ruma_events::room::message::MessageEventContent;
use ruma_events::EventResult;
pub use ruma_events::EventType;
use ruma_identifiers::RoomId;

use crate::api;
use crate::base_client::Client as BaseClient;
use crate::base_client::Room;
use crate::error::{Error, InnerError};
use crate::session::Session;
use crate::VERSION;

type RoomEventCallback =
    Box<dyn FnMut(Arc<RwLock<Room>>, Arc<EventResult<RoomEvent>>) -> BoxFuture<'static, ()> + Send>;

#[derive(Clone)]
/// An async/await enabled Matrix client.
pub struct AsyncClient {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    http_client: reqwest::Client,
    /// User session data.
    base_client: Arc<RwLock<BaseClient>>,
    /// The transaction id.
    transaction_id: Arc<AtomicU64>,
    /// Event callbacks
    event_callbacks: Arc<Mutex<Vec<RoomEventCallback>>>,
}

#[derive(Default, Debug)]
/// Configuration for the creation of the `AsyncClient`.
///
/// # Example
///
/// ```
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
/// use matrix_nio::AsyncClientConfig;
///
/// let client_config = AsyncClientConfig::new()
///     .proxy("http://localhost:8080")
///     .unwrap()
///     .disable_ssl_verification();
/// ```
pub struct AsyncClientConfig {
    proxy: Option<reqwest::Proxy>,
    user_agent: Option<HeaderValue>,
    disable_ssl_verification: bool,
}

impl AsyncClientConfig {
    /// Create a new default `AsyncClientConfig`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the proxy through which all the HTTP requests should go.
    ///
    /// Note, only HTTP proxies are supported.
    ///
    /// # Arguments
    ///
    /// * `proxy` - The HTTP URL of the proxy.
    ///
    /// # Example
    ///
    /// ```
    /// use matrix_nio::AsyncClientConfig;
    ///
    /// let client_config = AsyncClientConfig::new()
    ///     .proxy("http://localhost:8080")
    ///     .unwrap();
    /// ```
    pub fn proxy(mut self, proxy: &str) -> Result<Self, Error> {
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    /// Disable SSL verification for the HTTP requests.
    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }

    /// Set a custom HTTP user agent for the client.
    pub fn user_agent(mut self, user_agent: &str) -> Result<Self, InvalidHeaderValue> {
        self.user_agent = Some(HeaderValue::from_str(user_agent)?);
        Ok(self)
    }
}

#[derive(Debug, Default, Clone)]
/// Settings for a sync call.
pub struct SyncSettings {
    pub(crate) timeout: Option<UInt>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: Option<bool>,
}

impl SyncSettings {
    /// Create new default sync settings.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the sync token.
    ///
    /// # Arguments
    ///
    /// * `token` - The sync token that should be used for the sync call.
    pub fn token<S: Into<String>>(mut self, token: S) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the maximum time the server can wait, in milliseconds, before
    /// responding to the sync request.
    ///
    /// # Arguments
    ///
    /// * `timeout` - The time the server is allowed to wait.
    pub fn timeout<T: TryInto<UInt>>(mut self, timeout: T) -> Result<Self, js_int::TryFromIntError>
    where
        js_int::TryFromIntError:
            std::convert::From<<T as std::convert::TryInto<js_int::UInt>>::Error>,
    {
        self.timeout = Some(timeout.try_into()?);
        Ok(self)
    }

    /// Should the server return the full state from the start of the timeline.
    ///
    /// This does nothing if no sync token is set.
    ///
    /// # Arguments
    /// * `full_state` - A boolean deciding if the server should return the full
    ///     state or not.
    pub fn full_state(mut self, full_state: bool) -> Self {
        self.full_state = Some(full_state);
        self
    }
}

use api::r0::send::send_message_event;
use api::r0::session::login;
use api::r0::sync::sync_events;

impl AsyncClient {
    /// Creates a new client for making HTTP requests to the given homeserver.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `session` - If a previous login exists, the access token can be
    ///     reused by giving a session object here.
    pub fn new<U: TryInto<Url>>(
        homeserver_url: U,
        session: Option<Session>,
    ) -> Result<Self, Error> {
        let config = AsyncClientConfig::new();
        AsyncClient::new_with_config(homeserver_url, session, config)
    }

    /// Create a new client with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `session` - If a previous login exists, the access token can be
    ///     reused by giving a session object here.
    /// * `config` - Configuration for the client.
    pub fn new_with_config<U: TryInto<Url>>(
        homeserver_url: U,
        session: Option<Session>,
        config: AsyncClientConfig,
    ) -> Result<Self, Error> {
        let homeserver: Url = match homeserver_url.try_into() {
            Ok(u) => u,
            Err(_e) => panic!("Error parsing homeserver url"),
        };

        let http_client = reqwest::Client::builder();

        let http_client = if config.disable_ssl_verification {
            http_client.danger_accept_invalid_certs(true)
        } else {
            http_client
        };

        let http_client = match config.proxy {
            Some(p) => http_client.proxy(p),
            None => http_client,
        };

        let mut headers = reqwest::header::HeaderMap::new();

        let user_agent = match config.user_agent {
            Some(a) => a,
            None => HeaderValue::from_str(&format!("nio-rust {}", VERSION)).unwrap(),
        };

        headers.insert(reqwest::header::USER_AGENT, user_agent);

        let http_client = http_client.default_headers(headers).build()?;

        Ok(Self {
            homeserver,
            http_client,
            base_client: Arc::new(RwLock::new(BaseClient::new(session))),
            transaction_id: Arc::new(AtomicU64::new(0)),
            event_callbacks: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.base_client.read().unwrap().logged_in()
    }

    /// The Homeserver of the client.
    pub fn homeserver(&self) -> &Url {
        &self.homeserver
    }

    /// Add a callback that will be called every time the client receives a room
    /// event
    ///
    /// # Arguments
    ///
    /// * `callback` - The callback that should be called once a RoomEvent is
    ///     received.
    ///
    /// # Examples
    /// ```noexecute
    /// async fn async_cb(room: Arc<RwLock<Room>>, event: Arc<EventResult<RoomEvent>>) {
    ///     let room = room.read().unwrap();
    ///     let event = if let EventResult::Ok(event) = &*event {
    ///         event
    ///     } else {
    ///         return;
    ///     };
    ///     if let RoomEvent::RoomMessage(MessageEvent {
    ///         content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
    ///         sender,
    ///         ..
    ///     }) = event
    ///     {
    ///         let user = room.members.get(&sender.to_string()).unwrap();
    ///         println!(
    ///             "{}: {}",
    ///             user.display_name.as_ref().unwrap_or(&sender.to_string()),
    ///             msg_body
    ///         );
    ///     }
    /// }
    ///
    /// async fn main(client: AsyncClient) {
    ///     client.add_event_callback(async_cb);
    /// }
    ///```
    pub fn add_event_callback<C: 'static>(
        &mut self,
        mut callback: impl FnMut(Arc<RwLock<Room>>, Arc<EventResult<RoomEvent>>) -> C + 'static + Send,
    ) where
        C: Future<Output = ()> + Send,
    {
        let mut futures = self.event_callbacks.lock().unwrap();

        let future = move |room, event| callback(room, event).boxed();

        futures.push(Box::new(future));
    }

    /// Login to the server.
    ///
    /// # Arguments
    ///
    /// `user` - The user that should be logged in to the homeserver.
    /// `password` - The password of the user.
    /// `device_id` - A unique id that will be associated with this session. If
    ///     not given the homeserver will create one. Can be an exising
    ///     device_id from a previous login call. Note that this should be done
    ///     only if the client also holds the encryption keys for this devcie.
    pub async fn login<S: Into<String>>(
        &mut self,
        user: S,
        password: S,
        device_id: Option<S>,
    ) -> Result<login::Response, Error> {
        let request = login::Request {
            address: None,
            login_type: login::LoginType::Password,
            medium: None,
            device_id: device_id.map(|d| d.into()),
            password: password.into(),
            user: user.into(),
        };

        let response = self.send(request).await?;
        let mut client = self.base_client.write().unwrap();
        client.receive_login_response(&response);

        Ok(response)
    }

    /// Synchronise the client's state with the latest state on the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call.
    pub async fn sync(
        &mut self,
        sync_settings: SyncSettings,
    ) -> Result<sync_events::IncomingResponse, Error> {
        let request = sync_events::Request {
            filter: None,
            since: sync_settings.token,
            full_state: sync_settings.full_state,
            set_presence: None,
            timeout: sync_settings.timeout,
        };

        let response = self.send(request).await?;

        for (room_id, room) in &response.rooms.join {
            let room_id = room_id.to_string();

            let matrix_room = {
                let mut client = self.base_client.write().unwrap();

                for event in &room.state.events {
                    if let EventResult::Ok(e) = event {
                        client.receive_joined_state_event(&room_id, &e);
                    }
                }

                client.joined_rooms.get(&room_id).unwrap().clone()
            };

            for event in &room.timeline.events {
                {
                    let mut client = self.base_client.write().unwrap();
                    client.receive_joined_timeline_event(&room_id, &event);
                }

                let event = Arc::new(event.clone());

                let callbacks = {
                    let mut cb_futures = self.event_callbacks.lock().unwrap();
                    let mut callbacks = Vec::new();

                    for cb in &mut cb_futures.iter_mut() {
                        callbacks.push(cb(matrix_room.clone(), event.clone()));
                    }

                    callbacks
                };

                for cb in callbacks {
                    cb.await;
                }
            }

            let mut client = self.base_client.write().unwrap();
            client.receive_sync_response(&response);
        }

        Ok(response)
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. Note that those settings
    ///     will be only used for the first sync call.
    /// * `callback` - A callback that will be called every time a successful
    ///     response has been fetched from the server.
    ///
    /// # Examples
    ///
    /// The following example demonstrates how to sync forever while sending all
    /// the interesting events through a mpsc channel to another thread e.g. a
    /// UI thread.
    ///
    /// ```noexecute
    /// client
    ///     .sync_forever(sync_settings, async move |response| {
    ///         let channel = sync_channel;
    ///         for (room_id, room) in response.rooms.join {
    ///             for event in room.state.events {
    ///                 if let EventResult::Ok(e) = event {
    ///                     channel.send(e).await;
    ///                 }
    ///             }
    ///             for event in room.timeline.events {
    ///                 if let EventResult::Ok(e) = event {
    ///                     channel.send(e).await;
    ///                 }
    ///             }
    ///         }
    ///     })
    ///     .await;
    /// ```
    pub async fn sync_forever<C>(
        &mut self,
        sync_settings: SyncSettings,
        callback: impl Fn(sync_events::IncomingResponse) -> C + Send,
    ) where
        C: Future<Output = ()>,
    {
        let mut sync_settings = sync_settings;

        loop {
            let response = self.sync(sync_settings.clone()).await;

            // TODO query keys here.
            // TODO upload keys here
            // TODO send out to-device messages here

            let response = if let Ok(r) = response {
                r
            } else {
                sleep(Duration::from_secs(1)).await;
                continue;
            };

            callback(response).await;

            sync_settings = SyncSettings::new()
                .timeout(30000)
                .unwrap()
                .token(self.sync_token().unwrap());
        }
    }

    async fn send<Request: Endpoint>(
        &self,
        request: Request,
    ) -> Result<<Request::Response as Outgoing>::Incoming, Error>
    where
        Request::Incoming: TryFrom<http::Request<Vec<u8>>, Error = ruma_api::Error>,
        <Request::Response as Outgoing>::Incoming:
            TryFrom<http::Response<Vec<u8>>, Error = ruma_api::Error>,
    {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let url = request.uri();
        let url = self
            .homeserver
            .join(url.path_and_query().unwrap().as_str())
            .unwrap();

        let request_builder = match Request::METADATA.method {
            HttpMethod::GET => self.http_client.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.http_client.post(url).body(body)
            }
            HttpMethod::PUT => {
                let body = request.body().clone();
                self.http_client.put(url).body(body)
            }
            HttpMethod::DELETE => unimplemented!(),
            _ => panic!("Unsuported method"),
        };

        let request_builder = if Request::METADATA.requires_authentication {
            let client = self.base_client.read().unwrap();

            if let Some(ref session) = client.session {
                request_builder.bearer_auth(&session.access_token)
            } else {
                return Err(Error(InnerError::AuthenticationRequired));
            }
        } else {
            request_builder
        };

        let response = request_builder.send().await?;

        let status = response.status();
        let body = response.bytes().await?.as_ref().to_owned();
        let response = HttpResponse::builder().status(status).body(body).unwrap();
        let response = <Request::Response as Outgoing>::Incoming::try_from(response)?;

        Ok(response)
    }

    /// Get a new unique transaction id for the client.
    fn transaction_id(&self) -> u64 {
        self.transaction_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Send a room message to the homeserver.
    ///
    /// # Arguments
    ///
    /// `room_id` -  The id of the room that should receive the message.
    /// `data` - The content of the message.
    ///
    /// Returns the parsed response from the server.
    pub async fn room_send(
        &mut self,
        room_id: &str,
        data: MessageEventContent,
    ) -> Result<send_message_event::Response, Error> {
        let request = send_message_event::Request {
            room_id: RoomId::try_from(room_id).unwrap(),
            event_type: EventType::RoomMessage,
            txn_id: self.transaction_id().to_string(),
            data,
        };

        let response = self.send(request).await?;
        Ok(response)
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub fn sync_token(&self) -> Option<String> {
        self.base_client.read().unwrap().sync_token.clone()
    }
}
