use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    audio::{self, Commands, Monitor, MonitorEvent, Runner},
    config::{Config, Paths},
    model::{Graph, Node, NodeRef, Profile, Volume, common_volume},
};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub graph: Graph,
    pub volumes: BTreeMap<NodeRef, Volume>,
    pub config: Config,
    pub connected: bool,
    pub daemon_running: bool,
    pub message: String,
    pub config_error: Option<String>,
}

pub enum Action {
    Volume(Vec<NodeRef>, f64),
    Mute(Vec<NodeRef>),
    Default(NodeRef),
    Route(Vec<NodeRef>, NodeRef),
    Profile(Profile),
    Persistent(Vec<NodeRef>, (String, String)),
    Refresh,
}

pub struct Worker {
    pub state: Arc<Mutex<Snapshot>>,
    pub actions: Sender<Action>,
    pub stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(paths: Paths, stop: Arc<AtomicBool>, daemon: bool) -> Self {
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let (actions, receiver) = mpsc::channel();
        let shared = state.clone();
        let stopping = stop.clone();
        let thread =
            thread::spawn(move || Engine::new(paths, daemon).run(receiver, shared, stopping));
        Self {
            state,
            actions,
            stop,
            thread: Some(thread),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn send(&self, action: Action) {
        let _ = self.actions.send(action);
    }

    pub fn wait(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Engine {
    paths: Paths,
    daemon: bool,
    state: Snapshot,
    monitor: Option<Monitor>,
    reconnect: Instant,
    backoff: u64,
    config_checked: Instant,
    contents: Option<String>,
    dirty: BTreeSet<u32>,
    attempted: BTreeMap<u32, Instant>,
}

impl Engine {
    fn new(paths: Paths, daemon: bool) -> Self {
        Self {
            paths,
            daemon,
            state: Snapshot::default(),
            monitor: None,
            reconnect: Instant::now(),
            backoff: 1,
            config_checked: Instant::now() - Duration::from_secs(1),
            contents: None,
            dirty: BTreeSet::new(),
            attempted: BTreeMap::new(),
        }
    }

    fn run(
        mut self,
        actions: Receiver<Action>,
        shared: Arc<Mutex<Snapshot>>,
        stop: Arc<AtomicBool>,
    ) {
        let mut reported = String::new();

        while !stop.load(Ordering::Relaxed) {
            if self.config_checked.elapsed() >= Duration::from_millis(500) {
                self.reload();
            }

            if self.monitor.is_none() && Instant::now() >= self.reconnect {
                match Monitor::start() {
                    Ok(monitor) => self.monitor = Some(monitor),
                    Err(error) => self.disconnected(error),
                }
            }

            self.receive_changes();

            if let Ok(action) = actions.try_recv()
                && let Err(error) = self.act(action, &Commands)
            {
                self.state.message = error;
            }

            self.read_volume(&Commands);
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = self.state.clone();

            if self.daemon {
                let message = format!(
                    "{}{}",
                    self.state.message,
                    self.state
                        .config_error
                        .as_ref()
                        .map(|e| format!(" | {e}"))
                        .unwrap_or_default()
                );

                if message != reported {
                    eprintln!("pwaudio: {message}");
                    reported = message;
                }
            }

            thread::sleep(Duration::from_millis(20));
        }
    }

    fn reload(&mut self) {
        self.config_checked = Instant::now();
        let previously_running = self.state.daemon_running;
        self.state.daemon_running = self.daemon;

        if !self.daemon {
            match self.paths.daemon_running() {
                Ok(running) => self.state.daemon_running = running,
                Err(error) => self.state.message = error,
            }
        }

        match self.paths.contents() {
            Ok(contents)
                if self.contents.as_ref() != Some(&contents)
                    || self.state.config_error.is_some() =>
            {
                match Config::parse(&contents) {
                    Ok(config) => {
                        self.state.config = config;
                        self.state.config_error = None;
                        self.dirty.extend(
                            self.state
                                .graph
                                .nodes()
                                .iter()
                                .map(|node| node.reference.id),
                        );
                    }
                    Err(error) => self.state.config_error = Some(error),
                }

                self.contents = Some(contents);
            }
            Err(error) => self.state.config_error = Some(error),
            _ => {}
        }

        if previously_running && !self.state.daemon_running {
            self.dirty.extend(
                self.state
                    .graph
                    .nodes()
                    .iter()
                    .map(|node| node.reference.id),
            );
        }
    }

    fn receive_changes(&mut self) {
        for _ in 0..128 {
            let event = self
                .monitor
                .as_ref()
                .and_then(|monitor| monitor.events.try_recv().ok());

            match event {
                Some(MonitorEvent::Changes(changes)) => {
                    let changed = self.state.graph.apply(changes);
                    let nodes = self.state.graph.nodes();

                    for node in &nodes {
                        let client_changed = node
                            .props
                            .get("client.id")
                            .and_then(|id| id.parse::<u32>().ok())
                            .is_some_and(|id| changed.contains(&id));

                        if client_changed || changed.contains(&node.reference.id) {
                            self.dirty.insert(node.reference.id);
                        }
                    }

                    self.state
                        .volumes
                        .retain(|key, _| self.state.graph.contains(key));
                    self.dirty
                        .retain(|id| nodes.iter().any(|node| node.reference.id == *id));
                    self.attempted.retain(|id, _| self.dirty.contains(id));

                    if !self.state.connected {
                        self.state.message = "PipeWire connected".into();
                    }
                    self.state.connected = true;
                    self.backoff = 1;
                }
                Some(MonitorEvent::Failed(error)) => {
                    let details = self
                        .monitor
                        .as_ref()
                        .map(Monitor::error)
                        .unwrap_or_default();
                    self.disconnected(format!(
                        "{error}{}",
                        if details.is_empty() {
                            String::new()
                        } else {
                            format!(": {details}")
                        }
                    ));
                    break;
                }
                None => break,
            }
        }
    }

    fn disconnected(&mut self, error: String) {
        self.monitor = None;
        self.state.connected = false;
        self.state.graph = Graph::default();
        self.state.volumes.clear();
        self.dirty.clear();
        self.attempted.clear();
        self.state.message = format!("{error}. Retrying in {}s", self.backoff);
        self.reconnect = Instant::now() + Duration::from_secs(self.backoff);
        self.backoff = (self.backoff * 2).min(5);
    }

    fn read_volume(&mut self, runner: &impl Runner) {
        let id = self
            .dirty
            .iter()
            .find(|id| {
                self.attempted
                    .get(id)
                    .is_none_or(|time| time.elapsed() >= Duration::from_secs(1))
            })
            .copied();
        let Some(id) = id else { return };
        self.dirty.remove(&id);
        let Some(node) = self
            .state
            .graph
            .nodes()
            .into_iter()
            .find(|n| n.reference.id == id)
        else {
            return;
        };

        if node.is_card() {
            return;
        }

        let result = (|| {
            let mut volume = audio::volume(runner, &node)?;

            if (self.daemon || !self.state.daemon_running)
                && audio::enforce(runner, &self.state.config, &node, volume)?
            {
                volume.muted = true;
            }

            Ok::<_, String>(volume)
        })();

        match result {
            Ok(volume) => {
                self.state.volumes.insert(node.reference, volume);
                self.attempted.remove(&id);
            }
            Err(error) => {
                self.state.message = error;
                self.dirty.insert(id);
                self.attempted.insert(id, Instant::now());
            }
        }
    }

    fn resolve(&self, references: &[NodeRef]) -> Result<Vec<Node>, String> {
        let nodes = self.state.graph.nodes();
        references
            .iter()
            .map(|reference| {
                nodes
                    .iter()
                    .find(|node| node.reference == *reference)
                    .cloned()
                    .ok_or_else(|| "Audio object disappeared. Select it again".into())
            })
            .collect()
    }

    fn act(&mut self, action: Action, runner: &impl Runner) -> Result<(), String> {
        if let Action::Refresh = action {
            self.disconnected("Refreshing".into());
            self.reconnect = Instant::now();
            self.contents = None;
            self.reload();
            return Ok(());
        }

        if let Action::Persistent(references, identity) = action {
            let nodes = self.resolve(&references)?;

            if nodes.iter().any(|node| !node.is_playback()) {
                return Err("Persistent mute is available for playback applications only".into());
            }

            self.state.config = self.paths.toggle(&nodes, (&identity.0, &identity.1))?;
            self.contents = None;
            self.reload();
            self.state.message =
                "Persistent mute rule saved. Disabling a rule does not unmute streams".into();
            return Ok(());
        }

        if !self.state.connected {
            return Err("PipeWire is disconnected".into());
        }

        match action {
            Action::Volume(references, delta) => {
                let nodes = self.resolve(&references)?;

                if nodes.is_empty() {
                    return Err("No active streams".into());
                }

                if nodes.iter().any(Node::is_card) {
                    return Err(
                        "This card has no active audio nodes. Press p to choose a profile".into(),
                    );
                }

                for node in &nodes {
                    if !self.state.volumes.contains_key(&node.reference)
                        || self.dirty.contains(&node.reference.id)
                    {
                        self.state
                            .volumes
                            .insert(node.reference.clone(), audio::volume(runner, node)?);
                    }
                }

                let (current, _) = common_volume(&nodes, &self.state.volumes);
                let level = (current.ok_or("Volume is unavailable")?.level + delta).clamp(0.0, 1.0);

                for node in nodes {
                    runner.run(
                        "wpctl",
                        &[
                            "set-volume".into(),
                            node.reference.id.to_string(),
                            format!("{level:.4}"),
                        ],
                    )?;

                    if let Some(volume) = self.state.volumes.get_mut(&node.reference) {
                        volume.level = level;
                    }
                    self.dirty.insert(node.reference.id);
                }
            }
            Action::Mute(references) => {
                let nodes = self.resolve(&references)?;

                if nodes.is_empty() {
                    return Err("No active streams".into());
                }
                if nodes.iter().any(Node::is_card) {
                    return Err(
                        "This card has no active audio nodes. Press p to choose a profile".into(),
                    );
                }
                if nodes.iter().any(|node| self.state.config.persistent(node)) {
                    return Err("Persistent mute is enabled. Press M on the application to disable it first".into());
                }

                let mut all_muted = true;

                for node in &nodes {
                    let volume = audio::volume(runner, node)?;
                    all_muted &= volume.muted;
                    self.state.volumes.insert(node.reference.clone(), volume);
                }

                for node in nodes {
                    audio::set_mute(runner, &node, !all_muted)?;

                    if let Some(volume) = self.state.volumes.get_mut(&node.reference) {
                        volume.muted = !all_muted;
                    }
                    self.dirty.insert(node.reference.id);
                }
            }
            Action::Default(reference) => {
                let nodes = self.resolve(&[reference])?;
                let node = &nodes[0];

                if !node.is_sink() && !node.is_source() {
                    return Err("Select an output or input device".into());
                }

                runner.run(
                    "wpctl",
                    &["set-default".into(), node.reference.id.to_string()],
                )?;
            }
            Action::Route(references, target) => {
                let nodes = self.resolve(&references)?;
                let targets = self.resolve(&[target])?;
                let target = &targets[0];
                let name = target
                    .props
                    .get("node.name")
                    .ok_or("Target has no node.name")?;

                for node in nodes {
                    if !(node.is_playback() && target.is_sink()
                        || node.is_capture() && target.is_source())
                    {
                        return Err("Target does not match stream direction".into());
                    }

                    if node
                        .props
                        .get("node.dont-move")
                        .is_some_and(|value| value == "true")
                    {
                        return Err(format!("{} does not allow moving streams", node.name));
                    }

                    runner.run(
                        "pw-metadata",
                        &[
                            "-n".into(),
                            "default".into(),
                            node.reference.id.to_string(),
                            "target.object".into(),
                            name.clone(),
                            "Spa:String".into(),
                        ],
                    )?;
                }
            }
            Action::Profile(profile) => {
                if !self.state.graph.contains(&NodeRef {
                    id: profile.device,
                    serial: profile.serial,
                }) {
                    return Err("Audio card disappeared. Select it again".into());
                }
                if !profile.available {
                    return Err("This profile is unavailable".into());
                }

                runner.run(
                    "wpctl",
                    &[
                        "set-profile".into(),
                        profile.device.to_string(),
                        profile.index.to_string(),
                    ],
                )?;
            }
            Action::Persistent(..) | Action::Refresh => unreachable!(),
        }

        self.state.message = "Command accepted. Audio state updates automatically".into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        cell::RefCell,
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[derive(Default)]
    struct FakeCommands {
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Runner for FakeCommands {
        fn run(&self, program: &str, arguments: &[String]) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((program.into(), arguments.to_vec()));
            Ok("Volume: 0.80".into())
        }
    }

    fn connected_engine() -> Engine {
        let mut engine = Engine::new(Paths::in_directory(Path::new("/unused")), false);
        engine.state.connected = true;

        for (id, class, name) in [
            (42, "Stream/Output/Audio", "stream-a"),
            (43, "Stream/Output/Audio", "stream-b"),
            (44, "Audio/Sink", "speakers"),
            (45, "Audio/Source", "microphone"),
        ] {
            engine.state.graph.apply(vec![json!({
                "id": id,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "object.serial": id,
                        "media.class": class,
                        "node.name": name,
                        "application.process.binary": "player"
                    }
                }
            })]);
        }

        engine
    }

    fn playback_streams(engine: &Engine) -> Vec<NodeRef> {
        engine
            .state
            .graph
            .nodes()
            .into_iter()
            .filter(Node::is_playback)
            .map(|node| node.reference)
            .collect()
    }

    #[test]
    fn group_volume_uses_the_highest_stream_volume_as_its_starting_point() {
        let mut engine = connected_engine();
        let streams = playback_streams(&engine);
        engine.state.volumes.insert(
            streams[0].clone(),
            Volume {
                level: 0.4,
                muted: false,
            },
        );
        engine.state.volumes.insert(
            streams[1].clone(),
            Volume {
                level: 0.8,
                muted: false,
            },
        );
        let commands = FakeCommands::default();

        engine
            .act(Action::Volume(streams, 0.05), &commands)
            .unwrap();

        let calls = commands.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "wpctl");
        assert_eq!(calls[0].1, ["set-volume", "42", "0.8500"]);
        assert_eq!(calls[1].0, "wpctl");
        assert_eq!(calls[1].1, ["set-volume", "43", "0.8500"]);
    }

    #[test]
    fn playback_routing_rejects_inputs_and_routes_each_stream_by_target_name() {
        let mut engine = connected_engine();
        let streams = playback_streams(&engine);
        let nodes = engine.state.graph.nodes();
        let microphone = nodes.iter().find(|node| node.is_source()).unwrap();
        let speakers = nodes.iter().find(|node| node.is_sink()).unwrap();
        let commands = FakeCommands::default();

        let result = engine.act(
            Action::Route(streams.clone(), microphone.reference.clone()),
            &commands,
        );

        assert!(result.is_err());
        assert!(commands.calls.borrow().is_empty());

        engine
            .act(
                Action::Route(streams, speakers.reference.clone()),
                &commands,
            )
            .unwrap();

        let calls = commands.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "pw-metadata");
        assert_eq!(
            calls[0].1,
            [
                "-n",
                "default",
                "42",
                "target.object",
                "speakers",
                "Spa:String"
            ]
        );
        assert_eq!(calls[1].0, "pw-metadata");
        assert_eq!(
            calls[1].1,
            [
                "-n",
                "default",
                "43",
                "target.object",
                "speakers",
                "Spa:String"
            ]
        );
    }

    #[test]
    fn mute_key_cannot_override_a_persistent_mute_rule() {
        let mut engine = connected_engine();
        engine.state.config = Config::parse(
            r#"
[[applications]]
match = "application.process.binary"
value = "player"
muted = true
"#,
        )
        .unwrap();
        let streams = playback_streams(&engine);
        let commands = FakeCommands::default();

        let result = engine.act(Action::Mute(streams), &commands);

        assert!(result.unwrap_err().contains("Press M"));
        assert!(commands.calls.borrow().is_empty());
    }

    #[test]
    fn a_queued_command_cannot_change_a_new_stream_that_reuses_an_old_id() {
        let mut engine = connected_engine();
        let original_streams = playback_streams(&engine);
        let commands = FakeCommands::default();
        engine.state.graph.apply(vec![
            json!({ "id": 42, "info": null }),
            json!({
                "id": 42,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "object.serial": 999,
                        "media.class": "Stream/Output/Audio"
                    }
                }
            }),
        ]);

        let result = engine.act(Action::Volume(original_streams, 0.05), &commands);

        assert!(result.is_err());
        assert!(commands.calls.borrow().is_empty());
    }

    #[test]
    fn invalid_config_keeps_previous_rules_until_a_valid_edit_is_saved() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("pwaudio-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let paths = Paths::in_directory(&directory);
        let mut engine = Engine::new(paths.clone(), false);
        let valid_config = r#"
[[applications]]
match = "application.name"
value = "Player"
muted = true
"#;

        fs::write(&paths.config, valid_config).unwrap();
        engine.reload();

        assert_eq!(engine.state.config.applications.len(), 1);
        assert!(engine.state.config.applications[0].muted);
        assert!(engine.state.config_error.is_none());

        fs::write(&paths.config, "applications = [").unwrap();
        engine.reload();

        assert!(engine.state.config.applications[0].muted);
        assert!(engine.state.config_error.is_some());
        assert!(paths.toggle(&[], ("application.name", "Player")).is_err());

        fs::write(&paths.config, valid_config).unwrap();
        paths.toggle(&[], ("application.name", "Player")).unwrap();
        engine.reload();

        assert!(!engine.state.config.applications[0].muted);
        assert!(engine.state.config_error.is_none());
        fs::remove_dir_all(directory).unwrap();
    }
}
