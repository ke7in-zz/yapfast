use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, anyhow};
use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use x11rb::{
    connection::Connection as _,
    protocol::{
        Event,
        xinput::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};
use yapfast_core::runtime::{BackgroundRuntime, BackgroundTask};
use zbus::{
    Connection, MessageStream,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

use crate::push_to_talk::matcher::{Matcher, ModifierLayout};

use super::{Actions, Binding, Bindings, Chord, Command, Reach, StatusReporter};

pub(super) type Configure = mpsc::UnboundedSender<()>;

pub(super) enum Global {
    Portal { _session: BackgroundTask<()> },
    X11 { _events: X11 },
}

pub(super) fn is_wayland_session() -> bool {
    wayland_session(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_SESSION_TYPE").as_deref(),
    )
}

fn wayland_session(
    wayland_display: Option<&std::ffi::OsStr>,
    session_type: Option<&std::ffi::OsStr>,
) -> bool {
    wayland_display.is_some_and(|value| !value.is_empty())
        || session_type.is_some_and(|value| value == "wayland")
}
impl Global {
    pub fn start(
        bindings: &Bindings,
        runtime: &BackgroundRuntime,
        statuses: StatusReporter,
        actions: Actions,
    ) -> (Option<Self>, Option<Configure>) {
        if !any_binding(bindings) {
            return (None, None);
        }
        if is_wayland_session() {
            let (configure, requests) = mpsc::unbounded_channel();
            let bindings = bindings.clone();
            let session = runtime.spawn(async move {
                portal_worker(bindings, statuses, actions, requests).await;
            });
            return (Some(Self::Portal { _session: session }), Some(configure));
        }
        if std::env::var_os("DISPLAY").is_none() {
            report_x11_unavailable(bindings, &statuses);
            return (None, None);
        }
        match X11::start(bindings.clone(), statuses.clone(), actions) {
            Ok(events) => (Some(Self::X11 { _events: events }), None),
            Err(error) => {
                tracing::info!(%error, "no global shortcuts through XInput");
                report_x11_unavailable(bindings, &statuses);
                (None, None)
            }
        }
    }
}

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";
const TALK_ID: &str = "push-to-talk";
const STREAM_ID: &str = "toggle-stream";
const LEAVE_ID: &str = "leave-voice";

fn any_binding(bindings: &Bindings) -> bool {
    bindings.talk.is_some() || bindings.toggle_stream.is_some() || bindings.leave_voice.is_some()
}

fn requested(bindings: &Bindings) -> Vec<(Binding, &'static str, &'static str, Option<String>)> {
    let mut requested = Vec::with_capacity(3);
    if let Some(key) = &bindings.talk {
        requested.push((Binding::Talk, TALK_ID, "Push to talk", key.trigger.clone()));
    }
    if let Some(chord) = &bindings.toggle_stream {
        requested.push((
            Binding::ToggleStream,
            STREAM_ID,
            "Toggle game stream",
            portal_trigger(chord),
        ));
    }
    if let Some(chord) = &bindings.leave_voice {
        requested.push((
            Binding::LeaveVoice,
            LEAVE_ID,
            "Leave voice channel",
            portal_trigger(chord),
        ));
    }
    requested
}

fn portal_trigger(chord: &Chord) -> Option<String> {
    let key = chord.key.trigger.as_deref()?;
    let mut trigger = String::with_capacity(key.len() + 24);
    for (modifier, name) in [
        (super::ModifierMask::CTRL, "CTRL"),
        (super::ModifierMask::ALT, "ALT"),
        (super::ModifierMask::SHIFT, "SHIFT"),
        (super::ModifierMask::SUPER, "LOGO"),
    ] {
        if chord.modifiers.bits() & modifier.bits() != 0 {
            if !trigger.is_empty() {
                trigger.push('+');
            }
            trigger.push_str(name);
        }
    }
    if !trigger.is_empty() {
        trigger.push('+');
    }
    trigger.push_str(key);
    Some(trigger)
}

#[derive(Debug)]
enum PortalFailure {
    Denied,
    Transient(anyhow::Error),
}

enum PortalExit {
    Stopped,
    Returned,
}

async fn portal_worker(
    bindings: Bindings,
    statuses: StatusReporter,
    actions: Actions,
    mut requests: mpsc::UnboundedReceiver<()>,
) {
    let mut backoff = Duration::from_millis(250);
    statuses.asking(&bindings);
    loop {
        match portal_session(&bindings, &statuses, &actions, &mut requests).await {
            Ok(PortalExit::Stopped) => return,
            Ok(PortalExit::Returned) => {
                report_portal_unavailable(
                    &bindings,
                    &statuses,
                    "The desktop's global-shortcut service closed this session",
                );
            }
            Err(PortalFailure::Denied) => {
                report_portal_denied(&bindings, &statuses);
                return;
            }
            Err(PortalFailure::Transient(error)) => {
                tracing::info!(%error, "the global-shortcut service is unavailable");
                report_portal_unavailable(
                    &bindings,
                    &statuses,
                    "The desktop's global-shortcut service is unavailable",
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn portal_session(
    bindings: &Bindings,
    statuses: &StatusReporter,
    actions: &Actions,
    requests: &mut mpsc::UnboundedReceiver<()>,
) -> Result<PortalExit, PortalFailure> {
    let connection = Connection::session()
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let _ = connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some("org.freedesktop.host.portal.Registry"),
            "Register",
            &(crate::icon::APP_ID, HashMap::<&str, Value>::new()),
        )
        .await;

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(PORTAL)
        .map_err(|error| PortalFailure::Transient(error.into()))?
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &connection, Some(32))
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let dbus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, PORTAL)])
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let configuration_supported = configuration_supported(&connection).await;

    let sender = connection
        .unique_name()
        .context("no name on the session bus")
        .map_err(PortalFailure::Transient)?
        .trim_start_matches(':')
        .replace('.', "_");
    let token = format!("yapfast_{}", std::process::id());
    let request = |step: &str| format!("{PATH}/request/{sender}/{token}_{step}");

    let options = HashMap::from([
        ("handle_token", Value::from(format!("{token}_session"))),
        ("session_handle_token", Value::from(token.clone())),
    ]);
    connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some(SHORTCUTS),
            "CreateSession",
            &(options,),
        )
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let results = response(&mut signals, &request("session")).await?;
    let session = match results.get("session_handle").map(|value| &**value) {
        Some(Value::Str(handle)) => OwnedObjectPath::try_from(handle.as_str())
            .map_err(|error| PortalFailure::Transient(error.into()))?,
        Some(Value::ObjectPath(handle)) => handle.clone().into(),
        _ => {
            return Err(PortalFailure::Transient(anyhow!(
                "the portal returned no session handle"
            )));
        }
    };

    let requested = requested(bindings);
    let mut portal_shortcuts = Vec::with_capacity(requested.len());
    for (_, id, description, preferred) in &requested {
        let mut details = HashMap::from([("description", Value::from(*description))]);
        if let Some(preferred) = preferred {
            details.insert("preferred_trigger", Value::from(preferred.as_str()));
        }
        portal_shortcuts.push((*id, details));
    }
    let options = HashMap::from([("handle_token", Value::from(format!("{token}_bind")))]);
    connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some(SHORTCUTS),
            "BindShortcuts",
            &(&session, portal_shortcuts, "", options),
        )
        .await
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    let results = response(&mut signals, &request("bind")).await?;
    let mut events = PortalEvents {
        session: &session,
        bindings,
        requested: &requested,
        statuses,
        actions,
        state: PortalState::default(),
    };
    statuses.set_configuration_available(configuration_supported);
    events.refresh_grants(&shortcuts(&results));

    loop {
        futures_util::select! {
            incoming = signals.next().fuse() => {
                let Some(incoming) = incoming else {
                    events.clear();
                    return Err(PortalFailure::Transient(anyhow!("the session bus closed")));
                };
                let message = incoming.map_err(|error| {
                    events.clear();
                    PortalFailure::Transient(error.into())
                })?;
                if let Some(exit) = events.signal(&message) {
                    return Ok(exit);
                }
            },
            owner = owners.next().fuse() => {
                if let Some(exit) = handle_owner_change(&mut events, owner, &mut owners).await? {
                    return Ok(exit);
                }
            },
            requested_configuration = requests.recv().fuse() => {
                let Some(()) = requested_configuration else {
                    events.clear();
                    return Ok(PortalExit::Stopped);
                };
                if configure_shortcuts(&connection, &session).await {
                    match list_shortcuts(
                        &connection,
                        &mut signals,
                        &mut owners,
                        &token,
                        &mut events,
                    )
                    .await
                    {
                        Ok(ShortcutList::Updated(list)) => events.refresh_grants(&list),
                        Ok(ShortcutList::Exit(exit)) => return Ok(exit),
                        Err(PortalFailure::Denied) => {}
                        Err(error @ PortalFailure::Transient(_)) => return Err(error),
                    }
                }
            },
        }
    }
}

async fn wait_for_service_return<S>(owners: &mut S) -> Result<(), PortalFailure>
where
    S: futures_util::Stream<Item = zbus::fdo::NameOwnerChanged> + Unpin,
{
    while let Some(owner) = owners.next().await {
        let (name, _, new_owner) = owner
            .message()
            .body()
            .deserialize::<(String, String, String)>()
            .map_err(|error| PortalFailure::Transient(error.into()))?;
        if name == PORTAL && !new_owner.is_empty() {
            return Ok(());
        }
    }
    Err(PortalFailure::Transient(anyhow!("the session bus closed")))
}

async fn handle_owner_change<S>(
    events: &mut PortalEvents<'_>,
    owner: Option<zbus::fdo::NameOwnerChanged>,
    owners: &mut S,
) -> Result<Option<PortalExit>, PortalFailure>
where
    S: futures_util::Stream<Item = zbus::fdo::NameOwnerChanged> + Unpin,
{
    let Some(owner) = owner else {
        events.clear();
        return Err(PortalFailure::Transient(anyhow!("the session bus closed")));
    };
    let (name, old_owner, new_owner) = match owner
        .message()
        .body()
        .deserialize::<(String, String, String)>()
    {
        Ok(args) => args,
        Err(error) => {
            events.clear();
            return Err(PortalFailure::Transient(error.into()));
        }
    };
    if name == PORTAL && !old_owner.is_empty() && old_owner != new_owner {
        events.clear();
        report_portal_unavailable(
            events.bindings,
            events.statuses,
            "The desktop's global-shortcut service restarted",
        );
        if new_owner.is_empty() {
            wait_for_service_return(owners).await?;
        }
        return Ok(Some(PortalExit::Returned));
    }
    Ok(None)
}

async fn configuration_supported(connection: &Connection) -> bool {
    let version = connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(SHORTCUTS, "version"),
        )
        .await
        .ok()
        .and_then(|message| message.body().deserialize::<OwnedValue>().ok())
        .and_then(|value| match &*value {
            Value::U32(version) => Some(*version),
            _ => None,
        });
    version.is_some_and(|version| version >= 2)
}

async fn configure_shortcuts(connection: &Connection, session: &OwnedObjectPath) -> bool {
    let options = HashMap::<&str, Value>::new();
    connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some(SHORTCUTS),
            "ConfigureShortcuts",
            &(session, "", options),
        )
        .await
        .is_ok()
}

enum ShortcutList {
    Updated(Vec<(String, HashMap<String, OwnedValue>)>),
    Exit(PortalExit),
}

async fn list_shortcuts<S>(
    connection: &Connection,
    signals: &mut MessageStream,
    owners: &mut S,
    token: &str,
    events: &mut PortalEvents<'_>,
) -> Result<ShortcutList, PortalFailure>
where
    S: futures_util::Stream<Item = zbus::fdo::NameOwnerChanged> + Unpin,
{
    let token = format!("{token}_list");
    let options = HashMap::from([("handle_token", Value::from(token.clone()))]);
    connection
        .call_method(
            Some(PORTAL),
            PATH,
            Some(SHORTCUTS),
            "ListShortcuts",
            &(events.session, options),
        )
        .await
        .map_err(|error| {
            events.clear();
            PortalFailure::Transient(error.into())
        })?;
    let request = connection
        .unique_name()
        .context("no name on the session bus")
        .map_err(|error| {
            events.clear();
            PortalFailure::Transient(error)
        })?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("{PATH}/request/{request}/{token}");
    loop {
        futures_util::select! {
            incoming = signals.next().fuse() => {
                let Some(incoming) = incoming else {
                    events.clear();
                    return Err(PortalFailure::Transient(anyhow!("the session bus closed")));
                };
                let message = incoming.map_err(|error| {
                    events.clear();
                    PortalFailure::Transient(error.into())
                })?;
                if let Some(exit) = events.signal(&message) {
                    return Ok(ShortcutList::Exit(exit));
                }
                match request_response(&message, &path) {
                    Ok(Some(results)) => return Ok(ShortcutList::Updated(shortcuts(&results))),
                    Ok(None) => {}
                    Err(PortalFailure::Denied) => return Err(PortalFailure::Denied),
                    Err(error @ PortalFailure::Transient(_)) => {
                        events.clear();
                        return Err(error);
                    }
                }
            },
            owner = owners.next().fuse() => {
                if let Some(exit) = handle_owner_change(events, owner, owners).await? {
                    return Ok(ShortcutList::Exit(exit));
                }
            }
        }
    }
}

fn request_response(
    message: &zbus::message::Message,
    request: &str,
) -> Result<Option<HashMap<String, OwnedValue>>, PortalFailure> {
    let header = message.header();
    if header
        .member()
        .is_none_or(|member| member.as_str() != "Response")
        || header.path().is_none_or(|path| path.as_str() != request)
    {
        return Ok(None);
    }
    let (code, results) = message
        .body()
        .deserialize::<(u32, HashMap<String, OwnedValue>)>()
        .map_err(|error| PortalFailure::Transient(error.into()))?;
    if code == 0 {
        Ok(Some(results))
    } else {
        Err(PortalFailure::Denied)
    }
}

async fn response(
    signals: &mut MessageStream,
    request: &str,
) -> Result<HashMap<String, OwnedValue>, PortalFailure> {
    while let Some(message) = signals.next().await {
        let message = message.map_err(|error| PortalFailure::Transient(error.into()))?;
        if let Some(results) = request_response(&message, request)? {
            return Ok(results);
        }
    }
    Err(PortalFailure::Transient(anyhow!("the session bus closed")))
}

fn shortcuts(results: &HashMap<String, OwnedValue>) -> Vec<(String, HashMap<String, OwnedValue>)> {
    results
        .get("shortcuts")
        .and_then(|value| {
            Vec::<(String, HashMap<String, OwnedValue>)>::try_from(value.try_clone().ok()?).ok()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct PortalState {
    granted: [bool; 3],
    active: [bool; 3],
}

impl PortalState {
    fn clear(&mut self) {
        if self.active[Binding::Talk.index()] {
            yapfast_core::push_to_talk::hold(false);
        }
        self.active = [false; 3];
    }

    fn activation(
        &mut self,
        id: &str,
        pressed: bool,
        requested: &[(Binding, &'static str, &'static str, Option<String>)],
        actions: &Actions,
    ) {
        let Some((binding, ..)) = requested.iter().find(|(_, shortcut, ..)| *shortcut == id) else {
            return;
        };
        let index = binding.index();
        if !self.granted[index] || self.active[index] == pressed {
            return;
        }
        self.active[index] = pressed;
        match (binding, pressed) {
            (Binding::Talk, held) => yapfast_core::push_to_talk::hold(held),
            (Binding::ToggleStream, true) => actions.emit(Command::ToggleStream),
            (Binding::LeaveVoice, true) => actions.emit(Command::LeaveVoice),
            (_, false) => {}
        }
    }
}

fn update_grants(
    requested: &[(Binding, &'static str, &'static str, Option<String>)],
    bound: &[(String, HashMap<String, OwnedValue>)],
    statuses: &StatusReporter,
    state: &mut PortalState,
) {
    let mut next = [false; 3];
    for (binding, id, _, _) in requested {
        let details = bound
            .iter()
            .find(|(bound_id, _)| bound_id == id)
            .map(|(_, details)| details);
        if let Some(details) = details {
            next[binding.index()] = true;
            let description = details
                .get("trigger_description")
                .and_then(|value| match &**value {
                    Value::Str(text) if !text.is_empty() => Some(text.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            statuses.set(*binding, Reach::Everywhere(Some(description)));
        } else if *binding == Binding::Talk {
            statuses.set(
                *binding,
                Reach::Focused("Your desktop hasn't granted global push to talk"),
            );
        } else {
            statuses.set(
                *binding,
                Reach::Unavailable("Your desktop hasn't granted this global shortcut"),
            );
        }
    }
    for binding in [Binding::Talk, Binding::ToggleStream, Binding::LeaveVoice] {
        let index = binding.index();
        if state.active[index] && !next[index] && binding == Binding::Talk {
            yapfast_core::push_to_talk::hold(false);
        }
        if !next[index] {
            state.active[index] = false;
        }
    }
    state.granted = next;
}

struct PortalEvents<'a> {
    session: &'a OwnedObjectPath,
    bindings: &'a Bindings,
    requested: &'a [(Binding, &'static str, &'static str, Option<String>)],
    statuses: &'a StatusReporter,
    actions: &'a Actions,
    state: PortalState,
}

impl PortalEvents<'_> {
    fn clear(&mut self) {
        self.state.clear();
    }

    fn refresh_grants(&mut self, bound: &[(String, HashMap<String, OwnedValue>)]) {
        update_grants(self.requested, bound, self.statuses, &mut self.state);
    }

    fn signal(&mut self, message: &zbus::message::Message) -> Option<PortalExit> {
        match message.header().member().map(|member| member.as_str()) {
            Some("Activated") | Some("Deactivated") => {
                if let Ok((path, id, ..)) =
                    message
                        .body()
                        .deserialize::<(ObjectPath, &str, u64, HashMap<String, OwnedValue>)>()
                    && path == **self.session
                {
                    self.state.activation(
                        id,
                        message
                            .header()
                            .member()
                            .is_some_and(|member| member.as_str() == "Activated"),
                        self.requested,
                        self.actions,
                    );
                }
            }
            Some("ShortcutsChanged") => {
                if let Ok((path, changed)) = message
                    .body()
                    .deserialize::<(ObjectPath, Vec<(String, HashMap<String, OwnedValue>)>)>()
                    && path == **self.session
                {
                    self.refresh_grants(&changed);
                }
            }
            Some("Closed")
                if message
                    .header()
                    .path()
                    .is_some_and(|path| *path == **self.session) =>
            {
                self.clear();
                report_portal_unavailable(
                    self.bindings,
                    self.statuses,
                    "The desktop's global-shortcut service closed this session",
                );
                return Some(PortalExit::Stopped);
            }
            _ => {}
        }
        None
    }
}

fn report_portal_denied(bindings: &Bindings, statuses: &StatusReporter) {
    statuses.set_configuration_available(false);
    if bindings.talk.is_some() {
        statuses.set(
            Binding::Talk,
            Reach::Focused("Your desktop declined global push to talk"),
        );
    }
    for binding in [Binding::ToggleStream, Binding::LeaveVoice] {
        if binding.configured(bindings) {
            statuses.set(
                binding,
                Reach::Unavailable("Your desktop declined this global shortcut"),
            );
        }
    }
}

fn report_portal_unavailable(bindings: &Bindings, statuses: &StatusReporter, reason: &'static str) {
    statuses.set_configuration_available(false);
    if bindings.talk.is_some() {
        statuses.set(Binding::Talk, Reach::Focused(reason));
    }
    for binding in [Binding::ToggleStream, Binding::LeaveVoice] {
        if binding.configured(bindings) {
            statuses.set(binding, Reach::Unavailable(reason));
        }
    }
}

fn report_x11_available(bindings: &Bindings, statuses: &StatusReporter) {
    for binding in [Binding::Talk, Binding::ToggleStream, Binding::LeaveVoice] {
        if binding.configured(bindings) {
            statuses.set(binding, Reach::Everywhere(None));
        }
    }
}

fn report_x11_unavailable(bindings: &Bindings, statuses: &StatusReporter) {
    if bindings.talk.is_some() {
        statuses.set(
            Binding::Talk,
            Reach::Focused("Yapfast couldn't watch keys in other apps through XInput"),
        );
    }
    for binding in [Binding::ToggleStream, Binding::LeaveVoice] {
        if binding.configured(bindings) {
            statuses.set(
                binding,
                Reach::Unavailable("Yapfast couldn't watch shortcuts in other apps through XInput"),
            );
        }
    }
}

fn dispatch(edges: crate::push_to_talk::matcher::Edges, actions: &Actions) {
    if let Some(held) = edges.talk {
        yapfast_core::push_to_talk::hold(held);
    }
    for command in edges.commands() {
        actions.emit(command);
    }
}

struct Stop {
    stopped: AtomicBool,
    mutex: Mutex<()>,
    changed: Condvar,
}

impl Stop {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            mutex: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    fn stop(&self) {
        let _lock = self
            .mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stopped.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }

    fn wait(&self, delay: Duration) -> bool {
        let lock = self
            .mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.stopped.load(Ordering::Relaxed) {
            return true;
        }
        let _ = self
            .changed
            .wait_timeout(lock, delay)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stopped.load(Ordering::Relaxed)
    }
}

struct WakeTarget {
    connection: Arc<RustConnection>,
    window: xproto::Window,
}

struct X11Session {
    connection: Arc<RustConnection>,
    window: xproto::Window,
    seeded: Vec<u32>,
}

enum X11Failure {
    Unsupported(anyhow::Error),
    Transient(anyhow::Error),
}

impl X11Session {
    fn connect() -> Result<Self, X11Failure> {
        let (connection, screen) =
            x11rb::connect(None).map_err(|error| X11Failure::Transient(error.into()))?;
        let root = connection.setup().roots[screen].root;
        let cookie = connection
            .xinput_xi_query_version(2, 1)
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let version = cookie
            .reply()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        if version.major_version < 2 || (version.major_version == 2 && version.minor_version < 1) {
            return Err(X11Failure::Unsupported(anyhow!(
                "the X server does not provide XInput 2.1 raw-key delivery"
            )));
        }
        let window = connection
            .generate_id()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let cookie = connection
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new(),
            )
            .map_err(|error| X11Failure::Transient(error.into()))?;
        cookie
            .check()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let cookie = connection
            .xinput_xi_select_events(
                root,
                &[
                    xinput::EventMask {
                        deviceid: xinput::Device::ALL_MASTER.into(),
                        mask: vec![
                            xinput::XIEventMask::RAW_KEY_PRESS
                                | xinput::XIEventMask::RAW_KEY_RELEASE,
                        ],
                    },
                    xinput::EventMask {
                        deviceid: xinput::Device::ALL.into(),
                        mask: vec![xinput::XIEventMask::HIERARCHY],
                    },
                ],
            )
            .map_err(|error| X11Failure::Transient(error.into()))?;
        cookie
            .check()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let cookie = connection
            .query_keymap()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let keymap = cookie
            .reply()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        connection
            .flush()
            .map_err(|error| X11Failure::Transient(error.into()))?;
        let seeded = keymap
            .keys
            .iter()
            .enumerate()
            .flat_map(|(byte_index, byte)| {
                (0..8).filter_map(move |bit| {
                    let keycode = byte_index * 8 + bit;
                    ((*byte & (1 << bit) != 0) && keycode >= 8).then_some((keycode - 8) as u32)
                })
            })
            .collect();
        Ok(Self {
            connection: Arc::new(connection),
            window,
            seeded,
        })
    }

    fn wake_target(&self) -> WakeTarget {
        WakeTarget {
            connection: self.connection.clone(),
            window: self.window,
        }
    }
}

/// XI raw input is process-wide, does not consume the target app's keys, and
/// remains available during active grabs only from negotiated XInput 2.1.
pub struct X11 {
    stop: Arc<Stop>,
    wake: Arc<Mutex<Option<WakeTarget>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl X11 {
    fn start(
        bindings: Bindings,
        statuses: StatusReporter,
        actions: Actions,
    ) -> anyhow::Result<Self> {
        let (initial, initial_failure) = match X11Session::connect() {
            Ok(session) => (Some(session), None),
            Err(X11Failure::Unsupported(error)) => return Err(error),
            Err(X11Failure::Transient(error)) => (None, Some(error)),
        };
        let stop = Arc::new(Stop::new());
        let wake = Arc::new(Mutex::new(initial.as_ref().map(X11Session::wake_target)));
        let (thread_stop, thread_wake) = (stop.clone(), wake.clone());
        let worker = thread::Builder::new()
            .name("yapfast-global-keys".into())
            .spawn(move || {
                x11_worker(
                    initial,
                    initial_failure,
                    bindings,
                    statuses,
                    actions,
                    thread_stop,
                    thread_wake,
                )
            })?;
        Ok(Self {
            stop,
            wake,
            thread: Some(worker),
        })
    }
}

fn x11_worker(
    mut session: Option<X11Session>,
    initial_failure: Option<anyhow::Error>,
    bindings: Bindings,
    statuses: StatusReporter,
    actions: Actions,
    stop: Arc<Stop>,
    wake: Arc<Mutex<Option<WakeTarget>>>,
) {
    let mut matcher = Matcher::new(bindings.clone(), ModifierLayout::Evdev);
    let mut backoff = Duration::from_millis(250);
    if let Some(error) = initial_failure {
        tracing::info!(%error, "XInput is not ready; waiting before reconnecting");
        report_x11_unavailable(&bindings, &statuses);
    }
    loop {
        if stop.stopped.load(Ordering::Relaxed) {
            break;
        }
        if let Some(current) = &session {
            matcher.clear();
            matcher.seed_down(current.seeded.iter().copied());
            *wake.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(current.wake_target());
            report_x11_available(&bindings, &statuses);
            backoff = Duration::from_millis(250);
        } else {
            if stop.wait(backoff) {
                break;
            }
            match X11Session::connect() {
                Ok(current) => {
                    matcher.clear();
                    matcher.seed_down(current.seeded.iter().copied());
                    *wake.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(current.wake_target());
                    report_x11_available(&bindings, &statuses);
                    session = Some(current);
                    backoff = Duration::from_millis(250);
                    continue;
                }
                Err(X11Failure::Unsupported(error)) => {
                    tracing::info!(%error, "XInput raw-key delivery is unavailable");
                    report_x11_unavailable(&bindings, &statuses);
                    break;
                }
                Err(X11Failure::Transient(error)) => {
                    tracing::info!(%error, "XInput reconnect failed");
                    report_x11_unavailable(&bindings, &statuses);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        let current = session.as_ref().expect("session was just established");
        let outcome: Option<anyhow::Error> = loop {
            if stop.stopped.load(Ordering::Relaxed) {
                break None;
            }
            match current.connection.wait_for_event() {
                Ok(Event::XinputRawKeyPress(event)) => {
                    if !event.flags.contains(xinput::KeyEventFlags::KEY_REPEAT)
                        && let Some(scancode) = event.detail.checked_sub(8)
                    {
                        dispatch(matcher.event(event.sourceid, scancode, true), &actions);
                    }
                }
                Ok(Event::XinputRawKeyRelease(event)) => {
                    if !event.flags.contains(xinput::KeyEventFlags::KEY_REPEAT)
                        && let Some(scancode) = event.detail.checked_sub(8)
                    {
                        dispatch(matcher.event(event.sourceid, scancode, false), &actions);
                    }
                }
                Ok(Event::XinputHierarchy(_)) => {
                    dispatch(matcher.clear(), &actions);
                    let keymap = match current.connection.query_keymap() {
                        Ok(cookie) => match cookie.reply() {
                            Ok(keymap) => keymap,
                            Err(error) => break Some(error.into()),
                        },
                        Err(error) => break Some(error.into()),
                    };
                    let seeded = keymap
                        .keys
                        .iter()
                        .enumerate()
                        .flat_map(|(byte_index, byte)| {
                            (0..8).filter_map(move |bit| {
                                let keycode = byte_index * 8 + bit;
                                ((*byte & (1 << bit) != 0) && keycode >= 8)
                                    .then_some((keycode - 8) as u32)
                            })
                        });
                    matcher.seed_down(seeded);
                }
                Ok(Event::ClientMessage(_)) if stop.stopped.load(Ordering::Relaxed) => break None,
                Ok(_) => {}
                Err(error) => break Some(error.into()),
            }
        };
        if stop.stopped.load(Ordering::Relaxed) || outcome.is_none() {
            break;
        }
        dispatch(matcher.clear(), &actions);
        if let Some(error) = outcome {
            tracing::info!(%error, "lost the XInput connection; waiting before reconnecting");
        }
        report_x11_unavailable(&bindings, &statuses);
        session = None;
    }
    dispatch(matcher.clear(), &actions);
    *wake.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

impl Drop for X11 {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(target) = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let wake =
                xproto::ClientMessageEvent::new(32, target.window, xproto::AtomEnum::NONE, [0; 5]);
            let _ = target.connection.send_event(
                false,
                target.window,
                xproto::EventMask::NO_EVENT,
                wake,
            );
            let _ = target.connection.flush();
        }
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::push_to_talk::Keybind;

    fn key(scancode: u32, trigger: Option<&str>) -> Keybind {
        Keybind {
            scancode,
            name: "key".into(),
            trigger: trigger.map(str::to_owned),
        }
    }

    #[test]
    fn portal_trigger_uses_xdg_modifier_names() {
        let chord = Chord {
            key: key(47, Some("v")),
            modifiers: super::super::ModifierMask::CTRL
                | super::super::ModifierMask::ALT
                | super::super::ModifierMask::SUPER,
        };
        assert_eq!(portal_trigger(&chord).as_deref(), Some("CTRL+ALT+LOGO+v"));
    }

    #[test]
    fn wayland_session_type_selects_the_portal_without_a_wayland_socket() {
        use std::ffi::OsStr;

        assert!(wayland_session(None, Some(OsStr::new("wayland"))));
        assert!(wayland_session(Some(OsStr::new("wayland-0")), None));
        assert!(!wayland_session(None, Some(OsStr::new("x11"))));
    }

    fn wait_held(held: bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while yapfast_core::push_to_talk::held() != held {
            assert!(Instant::now() < until, "the key wasn't held: {held}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Needs DISPLAY on a private X server, such as Xvfb, and xdotool.
    #[test]
    #[ignore = "needs DISPLAY on a private X server and xdotool"]
    fn x11_delivers_chords_and_ptt_without_consuming_the_key() {
        assert_eq!(
            std::env::var("YAPFAST_TEST_PRIVATE_X11").as_deref(),
            Ok("1")
        );
        let bindings = Bindings {
            talk: Some(key(47, None)),
            toggle_stream: Some(Chord {
                key: key(46, None),
                modifiers: super::super::ModifierMask::CTRL,
            }),
            ..Bindings::default()
        };
        let commands = Arc::new(Mutex::new(Vec::new()));
        let heard = commands.clone();
        let actions = Actions::new(move |command| heard.lock().unwrap().push(command));
        actions.set_enabled(true);
        let statuses = StatusReporter::new(&bindings, |_, _| {});
        let x11 = X11::start(bindings, statuses.clone(), actions).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !matches!(statuses.get(Binding::Talk), Reach::Everywhere(_)) {
            assert!(Instant::now() < until, "XInput did not become available");
            thread::sleep(Duration::from_millis(10));
        }
        // A separate X client owns an active grab, as fullscreen games do.
        let (grabber, screen) = x11rb::connect(None).unwrap();
        let root = grabber.setup().roots[screen].root;
        let grab = grabber
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(grab.status, xproto::GrabStatus::SUCCESS);
        let xdotool = |args: &[&str]| {
            let status = std::process::Command::new("xdotool")
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        xdotool(&["keydown", "v"]);
        wait_held(true);
        xdotool(&["keyup", "v"]);
        wait_held(false);
        xdotool(&["keydown", "ctrl"]);
        xdotool(&["keydown", "c"]);
        xdotool(&["keydown", "c"]);
        xdotool(&["keyup", "c"]);
        // PTT is an ordered barrier: all preceding raw C events were handled.
        xdotool(&["keydown", "v"]);
        wait_held(true);
        assert_eq!(*commands.lock().unwrap(), [Command::ToggleStream]);
        xdotool(&["keyup", "v"]);
        wait_held(false);
        xdotool(&["keyup", "ctrl"]);
        drop(x11);
    }

    #[cfg(test)]
    mod portal_protocol_tests {
        use std::{
            sync::{Arc, mpsc},
            time::Instant,
        };

        use super::*;

        struct FakePortal {
            version: u32,
            list_requests: Option<mpsc::Sender<OwnedObjectPath>>,
        }

        #[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
        impl FakePortal {
            #[zbus(property, name = "version")]
            fn version(&self) -> u32 {
                self.version
            }

            async fn create_session(
                &self,
                options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
                #[zbus(connection)] connection: &Connection,
            ) -> OwnedObjectPath {
                let request = request_path(&header, &options);
                let session = format!("{PATH}/session/test/1");
                let results = HashMap::from([("session_handle", Value::from(session))]);
                respond(connection, &request, results).await;
                request
            }

            async fn bind_shortcuts(
                &self,
                session: OwnedObjectPath,
                shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
                _parent_window: String,
                options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
                #[zbus(connection)] connection: &Connection,
            ) -> OwnedObjectPath {
                let request = request_path(&header, &options);
                let bound = shortcuts
                    .iter()
                    .map(|(id, details)| {
                        if id == LEAVE_ID {
                            (id.clone(), HashMap::new())
                        } else {
                            let preferred = details
                                .get("preferred_trigger")
                                .and_then(|value| match &**value {
                                    Value::Str(value) => Some(value.to_uppercase()),
                                    _ => None,
                                })
                                .unwrap_or_else(|| "V".into());
                            (
                                id.clone(),
                                HashMap::from([(
                                    "trigger_description".to_owned(),
                                    Value::from(preferred),
                                )]),
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                respond(
                    connection,
                    &request,
                    HashMap::from([("shortcuts", Value::from(bound))]),
                )
                .await;
                if self.version < 2 {
                    let connection = connection.clone();
                    let (session, talk, toggle) = (
                        session.clone(),
                        shortcuts[0].0.clone(),
                        shortcuts[1].0.clone(),
                    );
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(100));
                        zbus::block_on(press(&connection, &session, &talk, "Activated"));
                        thread::sleep(Duration::from_millis(100));
                        zbus::block_on(press(&connection, &session, &talk, "Deactivated"));
                        zbus::block_on(press(&connection, &session, &toggle, "Activated"));
                        zbus::block_on(press(&connection, &session, &toggle, "Activated"));
                    });
                }
                request
            }
            async fn configure_shortcuts(
                &self,
                _session: OwnedObjectPath,
                _parent_window: String,
                _options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
            ) -> OwnedObjectPath {
                request_path_with_token(&header, "configure")
            }

            async fn list_shortcuts(
                &self,
                _session: OwnedObjectPath,
                options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
            ) -> OwnedObjectPath {
                let request = request_path(&header, &options);
                if let Some(requests) = &self.list_requests {
                    requests.send(request.clone()).unwrap();
                }
                request
            }
        }

        fn request_path(
            header: &zbus::message::Header<'_>,
            options: &HashMap<String, OwnedValue>,
        ) -> OwnedObjectPath {
            let Some(Value::Str(token)) = options.get("handle_token").map(|v| &**v) else {
                panic!("no handle token");
            };
            request_path_with_token(header, token)
        }

        fn request_path_with_token(
            header: &zbus::message::Header<'_>,
            token: &str,
        ) -> OwnedObjectPath {
            let sender = header
                .sender()
                .unwrap()
                .trim_start_matches(':')
                .replace('.', "_");
            OwnedObjectPath::try_from(format!("{PATH}/request/{sender}/{token}")).unwrap()
        }

        async fn respond(
            connection: &Connection,
            request: &OwnedObjectPath,
            results: HashMap<&str, Value<'_>>,
        ) {
            connection
                .emit_signal(
                    None::<()>,
                    request,
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, results),
                )
                .await
                .unwrap();
        }

        async fn press(connection: &Connection, session: &OwnedObjectPath, id: &str, signal: &str) {
            let options = HashMap::<&str, Value>::new();
            connection
                .emit_signal(
                    None::<()>,
                    PATH,
                    SHORTCUTS,
                    signal,
                    &(session, id, 0u64, options),
                )
                .await
                .unwrap();
        }

        fn granted(description: &str) -> HashMap<String, OwnedValue> {
            HashMap::from([(
                "trigger_description".to_owned(),
                Value::from(description).try_to_owned().unwrap(),
            )])
        }

        fn all_grants(
            leave: HashMap<String, OwnedValue>,
        ) -> Vec<(String, HashMap<String, OwnedValue>)> {
            all_grants_with_talk("V", leave)
        }

        fn all_grants_with_talk(
            talk: &str,
            leave: HashMap<String, OwnedValue>,
        ) -> Vec<(String, HashMap<String, OwnedValue>)> {
            vec![
                (TALK_ID.to_owned(), granted(talk)),
                (STREAM_ID.to_owned(), granted("CTRL+C")),
                (LEAVE_ID.to_owned(), leave),
            ]
        }

        fn list_results(description: &'static str) -> HashMap<&'static str, Value<'static>> {
            let shortcuts = vec![
                (
                    TALK_ID.to_owned(),
                    HashMap::from([("trigger_description".to_owned(), Value::from(description))]),
                ),
                (
                    STREAM_ID.to_owned(),
                    HashMap::from([("trigger_description".to_owned(), Value::from("CTRL+C"))]),
                ),
                (LEAVE_ID.to_owned(), HashMap::new()),
            ];
            HashMap::from([("shortcuts", Value::from(shortcuts))])
        }

        async fn shortcuts_changed(
            connection: &Connection,
            session: &OwnedObjectPath,
            shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        ) {
            connection
                .emit_signal(
                    None::<()>,
                    PATH,
                    SHORTCUTS,
                    "ShortcutsChanged",
                    &(session, shortcuts),
                )
                .await
                .unwrap();
        }

        async fn close_session(connection: &Connection, session: &OwnedObjectPath) {
            connection
                .emit_signal(None::<()>, session, SHORTCUTS, "Closed", &())
                .await
                .unwrap();
        }

        fn wait_status(watcher: &crate::push_to_talk::Watcher, binding: Binding, expected: Reach) {
            let until = Instant::now() + Duration::from_secs(2);
            while watcher.status(binding) != expected {
                assert!(Instant::now() < until, "portal status did not update");
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        #[ignore = "needs a private session bus and a Wayland session"]
        fn one_portal_session_binds_all_ids_and_emits_command_edges() {
            assert!(
                is_wayland_session(),
                "set WAYLAND_DISPLAY or XDG_SESSION_TYPE=wayland for the private portal scenario"
            );
            yapfast_core::push_to_talk::hold(false);

            let server = zbus::blocking::connection::Builder::session()
                .unwrap()
                .name(PORTAL)
                .unwrap()
                .serve_at(
                    PATH,
                    FakePortal {
                        version: 1,
                        list_requests: None,
                    },
                )
                .unwrap()
                .build()
                .unwrap();
            let bindings = Bindings {
                talk: Some(key(47, Some("v"))),
                toggle_stream: Some(Chord {
                    key: key(46, Some("c")),
                    modifiers: crate::push_to_talk::ModifierMask::CTRL,
                }),
                leave_voice: Some(Chord {
                    key: key(45, Some("x")),
                    modifiers: crate::push_to_talk::ModifierMask::ALT,
                }),
            };
            let commands = Arc::new(Mutex::new(Vec::new()));
            let heard_commands = commands.clone();
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let heard_statuses = statuses.clone();
            let runtime = BackgroundRuntime::new().unwrap();
            let watcher = crate::push_to_talk::Watcher::start(
                bindings,
                &runtime,
                move |command| heard_commands.lock().unwrap().push(command),
                move |binding, reach| heard_statuses.lock().unwrap().push((binding, reach)),
            );
            watcher.set_commands_enabled(true);
            let until = Instant::now() + Duration::from_secs(5);
            while !yapfast_core::push_to_talk::held() {
                assert!(
                    Instant::now() < until,
                    "portal did not activate the PTT shortcut"
                );
                thread::sleep(Duration::from_millis(10));
            }
            assert!(commands.lock().unwrap().is_empty());
            assert!(statuses.lock().unwrap().iter().any(|(binding, reach)| {
                *binding == Binding::Talk && matches!(reach, Reach::Everywhere(Some(_)))
            }));
            wait_held(false);
            let until = Instant::now() + Duration::from_secs(2);
            while commands.lock().unwrap().is_empty() {
                assert!(
                    Instant::now() < until,
                    "portal did not activate the stream command"
                );
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(commands.lock().unwrap().as_slice(), [Command::ToggleStream]);

            wait_status(
                &watcher,
                Binding::LeaveVoice,
                Reach::Everywhere(Some(String::new())),
            );
            let session = OwnedObjectPath::try_from(format!("{PATH}/session/test/1")).unwrap();
            zbus::block_on(shortcuts_changed(
                server.inner(),
                &session,
                all_grants(granted("ALT+X")),
            ));
            wait_status(
                &watcher,
                Binding::LeaveVoice,
                Reach::Everywhere(Some("ALT+X".into())),
            );

            zbus::block_on(shortcuts_changed(
                server.inner(),
                &session,
                all_grants(granted("")),
            ));
            wait_status(
                &watcher,
                Binding::LeaveVoice,
                Reach::Everywhere(Some(String::new())),
            );
            zbus::block_on(shortcuts_changed(
                server.inner(),
                &session,
                all_grants(HashMap::new()),
            ));
            wait_status(
                &watcher,
                Binding::LeaveVoice,
                Reach::Everywhere(Some(String::new())),
            );

            assert!(
                !watcher.configure_shortcuts(),
                "a live version-1 portal cannot open the version-2 editor"
            );
            drop(server);
        }

        #[test]
        #[ignore = "needs a private session bus and a Wayland session"]
        fn v2_list_wait_processes_session_signals() {
            assert!(
                is_wayland_session(),
                "set WAYLAND_DISPLAY or XDG_SESSION_TYPE=wayland for the private portal scenario"
            );
            yapfast_core::push_to_talk::hold(false);
            yapfast_core::push_to_talk::set_on(true);
            yapfast_core::push_to_talk::set_release_delay(0);

            let (list_requests, requested_lists) = mpsc::channel();
            let server = zbus::blocking::connection::Builder::session()
                .unwrap()
                .name(PORTAL)
                .unwrap()
                .serve_at(
                    PATH,
                    FakePortal {
                        version: 2,
                        list_requests: Some(list_requests),
                    },
                )
                .unwrap()
                .build()
                .unwrap();
            let bindings = Bindings {
                talk: Some(key(47, Some("v"))),
                toggle_stream: Some(Chord {
                    key: key(46, Some("c")),
                    modifiers: crate::push_to_talk::ModifierMask::CTRL,
                }),
                leave_voice: Some(Chord {
                    key: key(45, Some("x")),
                    modifiers: crate::push_to_talk::ModifierMask::ALT,
                }),
            };
            let commands = Arc::new(Mutex::new(Vec::new()));
            let heard_commands = commands.clone();
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let heard_statuses = statuses.clone();
            let runtime = BackgroundRuntime::new().unwrap();
            let watcher = crate::push_to_talk::Watcher::start(
                bindings,
                &runtime,
                move |command| heard_commands.lock().unwrap().push(command),
                move |binding, reach| heard_statuses.lock().unwrap().push((binding, reach)),
            );
            watcher.set_commands_enabled(true);
            wait_status(&watcher, Binding::Talk, Reach::Everywhere(Some("V".into())));
            let session = OwnedObjectPath::try_from(format!("{PATH}/session/test/1")).unwrap();
            zbus::block_on(press(server.inner(), &session, TALK_ID, "Activated"));
            wait_held(true);
            assert!(yapfast_core::push_to_talk::sending());

            assert!(watcher.configure_shortcuts());
            let first_list = requested_lists
                .recv_timeout(Duration::from_secs(2))
                .expect("the portal did not request its first shortcut list");
            zbus::block_on(press(server.inner(), &session, TALK_ID, "Deactivated"));
            zbus::block_on(press(server.inner(), &session, STREAM_ID, "Activated"));
            zbus::block_on(press(server.inner(), &session, STREAM_ID, "Activated"));
            zbus::block_on(shortcuts_changed(
                server.inner(),
                &session,
                all_grants_with_talk("SHIFT+V", granted("ALT+X")),
            ));
            wait_held(false);
            assert!(!yapfast_core::push_to_talk::sending());
            wait_status(
                &watcher,
                Binding::Talk,
                Reach::Everywhere(Some("SHIFT+V".into())),
            );
            assert_eq!(commands.lock().unwrap().as_slice(), [Command::ToggleStream]);

            zbus::block_on(respond(
                server.inner(),
                &first_list,
                list_results("SHIFT+V"),
            ));
            assert!(watcher.configure_shortcuts());
            let second_list = requested_lists
                .recv_timeout(Duration::from_secs(2))
                .expect("the portal did not request its second shortcut list");
            zbus::block_on(press(server.inner(), &session, TALK_ID, "Activated"));
            wait_held(true);
            assert!(yapfast_core::push_to_talk::sending());

            let release_reply = zbus::block_on(server.inner().call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ReleaseName",
                &(PORTAL,),
            ))
            .unwrap();
            assert_eq!(release_reply.body().deserialize::<u32>().unwrap(), 1);
            wait_held(false);
            assert!(!yapfast_core::push_to_talk::sending());
            wait_status(
                &watcher,
                Binding::Talk,
                Reach::Focused("The desktop's global-shortcut service restarted"),
            );

            let request_reply = zbus::block_on(server.inner().call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "RequestName",
                &(PORTAL, 0u32),
            ))
            .unwrap();
            assert_eq!(request_reply.body().deserialize::<u32>().unwrap(), 1);
            wait_status(
                &watcher,
                Binding::Talk,
                Reach::Focused("The desktop's global-shortcut service closed this session"),
            );
            // An old list response from the restarted service must not restore its grants.
            zbus::block_on(respond(server.inner(), &second_list, list_results("STALE")));
            wait_status(&watcher, Binding::Talk, Reach::Everywhere(Some("V".into())));
            let stale_grant = Reach::Everywhere(Some("STALE".into()));
            assert!(
                !statuses
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(binding, reach)| { *binding == Binding::Talk && reach == &stale_grant }),
                "a stale ListShortcuts response restored a grant after the portal restarted"
            );

            assert!(watcher.configure_shortcuts());
            let _third_list = requested_lists
                .recv_timeout(Duration::from_secs(2))
                .expect("the portal did not request its third shortcut list");
            zbus::block_on(press(server.inner(), &session, TALK_ID, "Activated"));
            wait_held(true);
            assert!(yapfast_core::push_to_talk::sending());
            zbus::block_on(close_session(server.inner(), &session));
            wait_held(false);
            assert!(!yapfast_core::push_to_talk::sending());
            wait_status(
                &watcher,
                Binding::Talk,
                Reach::Focused("The desktop's global-shortcut service closed this session"),
            );

            drop(watcher);
            yapfast_core::push_to_talk::set_on(false);
            yapfast_core::push_to_talk::set_release_delay(
                yapfast_core::push_to_talk::DEFAULT_RELEASE_DELAY,
            );
            drop(server);
        }
    }
}
