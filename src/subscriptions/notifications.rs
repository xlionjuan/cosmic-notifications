use crate::{config::VERSION, subscriptions::applet};
use cosmic::{
    iced::{
        futures::{self, SinkExt},
        stream,
    },
    iced_futures::Subscription,
};
use cosmic_notifications_util::{ActionId, CloseReason, Notification};
use futures::channel::mpsc;
use std::{collections::HashMap, fmt::Debug, num::NonZeroU32};
use tokio::{
    sync::mpsc::{Receiver, Sender, channel},
    task::JoinHandle,
};
use tracing::error;

use zbus::{
    Connection, connection::Builder as ConnectionBuilder, interface, object_server::SignalEmitter,
};

use super::applet::NotificationsApplet;

#[derive(Debug)]
pub struct Conns {
    notifications: Connection,
    pub tx: Sender<Input>,
    rx: Receiver<Input>,
    _panel: Option<Connection>,
}

impl Conns {
    pub async fn new() -> zbus::Result<Self> {
        let (tx, rx) = channel(100);
        let panel = match applet::setup_panel_conn(tx.clone()).await {
            Ok(conn) => Some(conn),
            Err(err) => {
                error!("Failed to setup panel dbus server {}", err.to_string());
                None
            }
        };

        for _ in 0..5 {
            if let Some(conn) = ConnectionBuilder::session()
                .ok()
                .and_then(|conn| conn.name("org.freedesktop.Notifications").ok())
                .and_then(|conn| {
                    conn.serve_at(
                        "/org/freedesktop/Notifications",
                        Notifications(tx.clone(), NonZeroU32::new(1).unwrap(), Vec::new()),
                    )
                    .ok()
                })
                .map(ConnectionBuilder::build)
            {
                if let Ok(conn) = conn.await {
                    return Ok(Self {
                        tx,
                        notifications: conn,
                        rx,
                        _panel: panel,
                    });
                }
            } else {
                error!("Failed to create connection at /org/freedesktop/Notifications");
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        Err(zbus::Error::Failure(
            "Failed to create the dbus server".to_string(),
        ))
    }
}

struct Start;
struct Waiting;

struct Machine<S> {
    conns: Option<Conns>,
    output: mpsc::Sender<Event>,
    marker: core::marker::PhantomData<S>,
}

impl<S> Machine<S> {
    pub fn new(conns: Option<Conns>, output: mpsc::Sender<Event>) -> Self {
        Self {
            conns,
            output,
            marker: core::marker::PhantomData,
        }
    }

    pub fn transition<Next>(self) -> Machine<Next> {
        Machine::<Next> {
            conns: self.conns,
            output: self.output,
            marker: core::marker::PhantomData,
        }
    }
}

impl Machine<Start> {
    pub async fn exec(mut self) -> Result<(Machine<Waiting>, Conns), ()> {
        let handle: JoinHandle<zbus::Result<_>> = tokio::spawn(async move {
            let conns = Conns::new().await?;
            Ok(conns)
        });

        match handle.await {
            Ok(Ok(conns)) => {
                _ = self.output.send(Event::Ready(conns.tx.clone())).await;
                Ok((self.transition::<Waiting>(), conns))
            }
            Ok(Err(err)) => {
                error!("Failed to create connection {}", err);
                Err(())
            }
            Err(err) => {
                error!("Failed to create connection {}", err);
                Err(())
            }
        }
    }
}

impl Machine<Waiting> {
    pub async fn exec(mut self, mut conns: Conns) {
        loop {
            if let Some(next) = conns.rx.recv().await {
                match next {
                    Input::Activated { token, id, action } => {
                        let object_server = conns.notifications.object_server();
                        let Ok(iface_ref) = object_server
                            .interface::<_, Notifications>("/org/freedesktop/Notifications")
                            .await
                        else {
                            continue;
                        };

                        if let Err(err) =
                            Notifications::activation_token(iface_ref.signal_emitter(), id, &token)
                                .await
                        {
                            error!("Failed to signal notification with token {}", err);
                        }

                        if let Err(err) =
                            Notifications::action_invoked(iface_ref.signal_emitter(), id, &action)
                                .await
                        {
                            error!("Failed to signal activated notification {}", err);
                        }
                        tracing::trace!("Activated application");
                    }
                    Input::Closed(id, reason) => {
                        let object_server = conns.notifications.object_server();
                        if let Ok(iface_ref) = object_server
                            .interface::<_, Notifications>("/org/freedesktop/Notifications")
                            .await
                        {
                            _ = Notifications::notification_closed(
                                iface_ref.signal_emitter(),
                                id,
                                reason as u32,
                            )
                            .await;
                        }
                    }
                    Input::Notification(notification) => {
                        _ = self.output.send(Event::Notification(notification)).await;
                    }
                    Input::Replace(notification) => {
                        _ = self.output.send(Event::Replace(notification)).await;
                    }
                    Input::CloseNotification(id) => {
                        _ = self.output.send(Event::CloseNotification(id)).await;

                        let object_server = conns.notifications.object_server();
                        let Ok(iface_ref) = object_server
                            .interface::<_, Notifications>("/org/freedesktop/Notifications")
                            .await
                        else {
                            continue;
                        };
                        if let Err(err) =
                            Notifications::notification_closed(iface_ref.signal_emitter(), id, 3)
                                .await
                        {
                            error!("Failed to signal close notification {}", err);
                        }
                    }
                    Input::Dismissed(id) => {
                        let object_server = conns.notifications.object_server();
                        let Ok(iface_ref) = object_server
                            .interface::<_, Notifications>("/org/freedesktop/Notifications")
                            .await
                        else {
                            continue;
                        };
                        if let Err(err) =
                            Notifications::notification_closed(iface_ref.signal_emitter(), id, 2)
                                .await
                        {
                            error!("Failed to signal dismissed notification {}", err);
                        }
                    }
                    Input::AppletConn(c) => {
                        let object_server = conns.notifications.object_server();
                        let Ok(iface_ref) = object_server
                            .interface::<_, Notifications>("/org/freedesktop/Notifications")
                            .await
                        else {
                            continue;
                        };
                        let mut iface = iface_ref.get_mut().await;
                        iface.2.push(c);
                    }
                    Input::AppletActivated { id, action } => {
                        if let Err(err) = self
                            .output
                            .send(Event::AppletActivated { id, action })
                            .await
                        {
                            tracing::error!(
                                "Failed to send activation action for {id} to subscription channel {err}"
                            );
                        }
                    }
                }
            } else {
                // The channel was closed, so we are done
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Input {
    Activated {
        token: String,
        id: u32,
        action: String,
    },
    AppletActivated {
        id: u32,
        action: ActionId,
    },
    Notification(Notification),
    Replace(Notification),
    CloseNotification(u32),
    Closed(u32, CloseReason),
    Dismissed(u32),
    AppletConn(Connection),
}

#[derive(Debug, Clone)]
pub enum Event {
    Ready(Sender<Input>),
    Notification(Notification),
    Replace(Notification),
    CloseNotification(u32),
    AppletActivated { id: u32, action: ActionId },
}

pub fn notifications() -> Subscription<Event> {
    struct SomeWorker;

    Subscription::run_with_id(
        std::any::TypeId::of::<SomeWorker>(),
        stream::channel(100, |output| async move {
            let machine = Machine::<Start>::new(None, output);

            if let Ok((waiting, conns)) = machine.exec().await {
                waiting.exec(conns).await;
            };

            futures::pending!();
        }),
    )
}

pub struct Notifications(Sender<Input>, NonZeroU32, Vec<Connection>);

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    async fn close_notification(&self, id: u32) {
        if let Err(err) = self.0.send(Input::CloseNotification(id)).await {
            tracing::error!("Failed to send close notification: {}", err);
        }
    }

    /// "action-icons"	Supports using icons instead of text for displaying actions. Using icons for actions must be enabled on a per-notification basis using the "action-icons" hint.
    /// "actions"	The server will provide the specified actions to the user. Even if this cap is missing, actions may still be specified by the client, however the server is free to ignore them.
    /// "body"	Supports body text. Some implementations may only show the summary (for instance, onscreen displays, marquee/scrollers)
    /// "body-hyperlinks"	The server supports hyperlinks in the notifications.
    /// "body-images"	The server supports images in the notifications.
    /// "body-markup"	Supports markup in the body text. If marked up text is sent to a server that does not give this cap, the markup will show through as regular text so must be stripped clientside.
    /// "icon-multi"	The server will render an animation of all the frames in a given image array. The client may still specify multiple frames even if this cap and/or "icon-static" is missing, however the server is free to ignore them and use only the primary frame.
    /// "icon-static"	Supports display of exactly 1 frame of any given image array. This value is mutually exclusive with "icon-multi", it is a protocol error for the server to specify both.
    /// "persistence"	The server supports persistence of notifications. Notifications will be retained until they are acknowledged or removed by the user or recalled by the sender. The presence of this capability allows clients to depend on the server to ensure a notification is seen and eliminate the need for the client to display a reminding function (such as a status icon) of its own.
    /// "sound"	The server supports sounds on notifications. If returned, the server must support the "sound-file" and "suppress-sound" hints.
    async fn get_capabilities(&self) -> Vec<&'static str> {
        vec![
            "body",
            "icon-static",
            "persistence",
            "actions",
            // TODO support these
            "action-icons",
            "body-markup",
            "body-hyperlinks",
            "sound",
        ]
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    async fn get_server_information(
        &self,
    ) -> (&'static str, &'static str, &'static str, &'static str) {
        ("cosmic-notifications", "System76", VERSION, "1.2")
    }

    ///
    /// app_name	STRING	The optional name of the application sending the notification. Can be blank.
    ///
    /// replaces_id	UINT32	The optional notification ID that this notification replaces. The server must atomically (ie with no flicker or other visual cues) replace the given notification with this one. This allows clients to effectively modify the notification while it's active. A value of value of 0 means that this notification won't replace any existing notifications.
    ///
    /// app_icon	STRING	The optional program icon of the calling application. See Icons and Images. Can be an empty string, indicating no icon.
    ///
    /// summary	STRING	The summary text briefly describing the notification.
    ///
    /// body	STRING	The optional detailed body text. Can be empty.
    ///
    /// actions	as	Actions are sent over as a list of pairs. Each even element in the list (starting at index 0) represents the identifier for the action. Each odd element in the list is the localized string that will be displayed to the user.
    ///
    /// hints	a{sv}	Optional hints that can be passed to the server from the client program. Although clients and servers should never assume each other supports any specific hints, they can be used to pass along information, such as the process PID or window ID, that the server may be able to make use of. See Hints. Can be empty.
    /// expire_timeout	INT32
    ///
    /// The timeout time in milliseconds since the display of the notification at which the notification should automatically close.
    /// If -1, the notification's expiration time is dependent on the notification server's settings, and may vary for the type of notification. If 0, never expire.
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &mut self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            let id = self.1;
            self.1 = match self.1.checked_add(1) {
                Some(id) => id,
                None => {
                    tracing::warn!("Notification ID overflowed");
                    NonZeroU32::new(1).unwrap()
                }
            };
            id.get()
        } else {
            replaces_id
        };
        let hints_clone = hints
            .iter()
            .filter_map(|(k, v)| Some((*k, v.try_clone().ok()?)))
            .collect();
        let n = Notification::new(
            app_name,
            id,
            app_icon,
            summary,
            body,
            actions.clone(),
            hints_clone,
            expire_timeout,
        );

        if !n.transient() {
            let mut new_conns = Vec::with_capacity(self.2.len());
            for c in self.2.drain(..) {
                let object_server = c.object_server();
                let Ok(Ok(iface_ref)) = tokio::time::timeout(
                    tokio::time::Duration::from_millis(100),
                    object_server
                        .interface::<_, NotificationsApplet>("/com/system76/NotificationsApplet"),
                )
                .await
                else {
                    continue;
                };
                let hints_clone = hints
                    .iter()
                    .filter_map(|(k, v)| Some((*k, v.try_clone().ok()?)))
                    .collect();
                match tokio::time::timeout(
                    tokio::time::Duration::from_millis(500),
                    NotificationsApplet::notify(
                        iface_ref.signal_emitter(),
                        app_name,
                        id,
                        app_icon,
                        summary,
                        body,
                        actions.clone(),
                        hints_clone,
                        expire_timeout,
                    ),
                )
                .await
                {
                    Ok(Err(err)) => error!("Failed to notify applet of notification {}", err),
                    Err(err) => error!("Failed to notify applet of notification {}", err),
                    Ok(_) => {}
                }
                new_conns.push(c);
            }
            self.2 = new_conns;
        }

        if let Err(err) = self
            .0
            .send(if replaces_id == 0 {
                Input::Notification(n)
            } else {
                Input::Replace(n)
            })
            .await
        {
            tracing::error!("Failed to send notification: {}", err);
        }

        id
    }

    #[zbus(signal)]
    async fn action_invoked(
        signal_ctxt: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn activation_token(
        signal_ctxt: &SignalEmitter<'_>,
        id: u32,
        activation_token: &str,
    ) -> zbus::Result<()>;

    /// id	UINT32	The ID of the notification that was closed.
    /// reason	UINT32
    ///
    /// The reason the notification was closed.
    ///
    /// 1 - The notification expired.
    ///
    /// 2 - The notification was dismissed by the user.
    ///
    /// 3 - The notification was closed by a call to CloseNotification.
    ///
    /// 4 - Undefined/reserved reasons.
    #[zbus(signal)]
    async fn notification_closed(
        signal_ctxt: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}
