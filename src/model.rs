use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::config::{BINARY, Config, NAME};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeRef {
    pub id: u32,
    pub serial: String,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub reference: NodeRef,
    pub class: String,
    pub name: String,
    pub props: BTreeMap<String, String>,
}

impl Node {
    pub fn is_playback(&self) -> bool {
        self.class == "Stream/Output/Audio"
    }
    pub fn is_capture(&self) -> bool {
        self.class == "Stream/Input/Audio"
    }
    pub fn is_stream(&self) -> bool {
        self.is_playback() || self.is_capture()
    }
    pub fn is_sink(&self) -> bool {
        self.class == "Audio/Sink"
    }
    pub fn is_source(&self) -> bool {
        matches!(self.class.as_str(), "Audio/Source" | "Audio/Source/Virtual")
    }

    pub fn is_card(&self) -> bool {
        self.class == "Audio/Device"
    }

    pub fn identity(&self) -> Option<(&str, &str)> {
        [BINARY, NAME].into_iter().find_map(|key| {
            self.props
                .get(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key, value.as_str()))
        })
    }

    #[cfg(test)]
    pub fn test(id: u32, class: &str, props: BTreeMap<String, String>) -> Self {
        Self {
            reference: NodeRef {
                id,
                serial: id.to_string(),
            },
            class: class.into(),
            name: "Test".into(),
            props,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub level: f64,
    pub muted: bool,
}

#[derive(Clone, Default)]
pub struct Graph {
    pub objects: BTreeMap<u32, Value>,
}

impl Graph {
    pub fn apply(&mut self, changes: Vec<Value>) -> BTreeSet<u32> {
        let mut dirty = BTreeSet::new();

        for change in changes {
            let Some(id) = change.get("id").and_then(number) else {
                continue;
            };

            if change.get("info").is_some_and(Value::is_null) {
                self.objects.remove(&id);
                dirty.insert(id);
                continue;
            }

            let kind = change
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| self.objects.get(&id)?.get("type")?.as_str())
                .unwrap_or("");

            if ![
                "PipeWire:Interface:Node",
                "PipeWire:Interface:Device",
                "PipeWire:Interface:Link",
                "PipeWire:Interface:Metadata",
                "PipeWire:Interface:Client",
            ]
            .contains(&kind)
            {
                continue;
            }

            let previous = self.objects.entry(id).or_insert(Value::Null);
            let serial = change.pointer("/info/props/object.serial");

            if serial.is_some() && serial != previous.pointer("/info/props/object.serial") {
                *previous = Value::Null;
            }

            // Metadata arrays contain changed keys, while parameter arrays replace old values.
            if let Some(updates) = change.get("metadata").and_then(Value::as_array) {
                let metadata = previous.get_mut("metadata").and_then(Value::as_array_mut);

                if let Some(metadata) = metadata {
                    for update in updates {
                        let subject = update.get("subject");
                        let key = update.get("key");
                        metadata.retain(|entry| {
                            entry.get("subject") != subject
                                || (key.is_some_and(|k| !k.is_null()) && entry.get("key") != key)
                        });

                        if update.get("value").is_some_and(|v| !v.is_null()) {
                            metadata.push(update.clone());
                        }
                    }
                } else {
                    if previous.is_null() {
                        *previous = serde_json::json!({});
                    }
                    previous["metadata"] = Value::Array(updates.clone());
                }
            }

            let mut change = change;
            if change
                .pointer("/info/change-mask")
                .and_then(Value::as_array)
                .is_some_and(|mask| mask.iter().any(|value| value.as_str() == Some("props")))
                && let Some(props) = change.pointer("/info/props")
                && let Some(info) = previous.get_mut("info").and_then(Value::as_object_mut)
            {
                info.insert("props".into(), props.clone());
            }

            change
                .as_object_mut()
                .map(|object| object.remove("metadata"));
            merge(previous, change);
            dirty.insert(id);
        }

        dirty
    }

    pub fn nodes(&self) -> Vec<Node> {
        let mut nodes: Vec<Node> = self
            .objects
            .iter()
            .filter_map(|(&id, object)| {
                if object.get("type")?.as_str()? != "PipeWire:Interface:Node" {
                    return None;
                }
                let info = object.get("info")?;
                let props = info.get("props")?.as_object()?;
                let class = props.get("media.class")?.as_str()?.to_string();
                let serial = scalar(props.get("object.serial")?).unwrap_or_default();
                let mut properties = BTreeMap::new();

                if let Some(client) = props
                    .get("client.id")
                    .and_then(number)
                    .and_then(|id| self.objects.get(&id))
                    && let Some(props) = client.pointer("/info/props").and_then(Value::as_object)
                {
                    for (key, value) in props {
                        if let Some(value) = scalar(value) {
                            properties.insert(key.clone(), value);
                        }
                    }
                }

                for (key, value) in props {
                    if let Some(value) = scalar(value) {
                        properties.insert(key.clone(), value);
                    }
                }

                let name = if class.starts_with("Stream/") {
                    [
                        "application.name",
                        "node.description",
                        "media.name",
                        "node.name",
                    ]
                    .as_slice()
                } else {
                    [
                        "node.description",
                        "node.nick",
                        "node.name",
                        "device.description",
                    ]
                    .as_slice()
                }
                .iter()
                .find_map(|key| properties.get(*key))
                .cloned()
                .unwrap_or_else(|| format!("Node {id}"));
                let node = Node {
                    reference: NodeRef { id, serial },
                    class,
                    name,
                    props: properties,
                };
                (node.is_stream() || node.is_sink() || node.is_source()).then_some(node)
            })
            .collect();

        // Keep cards with an Off profile accessible even when they expose no audio nodes.
        for (&id, object) in &self.objects {
            if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Device")
                || nodes.iter().any(|node| {
                    node.props
                        .get("device.id")
                        .and_then(|v| v.parse::<u32>().ok())
                        == Some(id)
                })
            {
                continue;
            }

            let Some(props) = object.pointer("/info/props").and_then(Value::as_object) else {
                continue;
            };

            if props.get("media.class").and_then(Value::as_str) != Some("Audio/Device") {
                continue;
            }

            let Some(serial) = props.get("object.serial").and_then(scalar) else {
                continue;
            };
            let mut props: BTreeMap<_, _> = props
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), scalar(value)?)))
                .collect();
            props.insert("device.id".into(), id.to_string());
            let name = props
                .get("device.description")
                .or_else(|| props.get("device.name"))
                .cloned()
                .unwrap_or_else(|| format!("Audio card {id}"));
            nodes.push(Node {
                reference: NodeRef { id, serial },
                class: "Audio/Device".into(),
                name,
                props,
            });
        }

        nodes
    }

    pub fn contains(&self, reference: &NodeRef) -> bool {
        self.objects
            .get(&reference.id)
            .and_then(|object| object.pointer("/info/props/object.serial"))
            .and_then(scalar)
            .as_ref()
            == Some(&reference.serial)
    }

    pub fn is_default(&self, node: &Node) -> bool {
        let key = if node.is_sink() {
            "default.audio.sink"
        } else {
            "default.audio.source"
        };
        self.objects
            .values()
            .filter(|object| {
                object
                    .pointer("/props/metadata.name")
                    .and_then(Value::as_str)
                    == Some("default")
            })
            .filter_map(|object| object.get("metadata")?.as_array())
            .flatten()
            .any(|entry| {
                if entry.get("key").and_then(Value::as_str) != Some(key) {
                    return false;
                }
                let value = entry.get("value").cloned().unwrap_or(Value::Null);
                let value = value
                    .as_str()
                    .and_then(|v| serde_json::from_str::<Value>(v).ok())
                    .unwrap_or(value);
                value.get("name").and_then(Value::as_str)
                    == node.props.get("node.name").map(String::as_str)
            })
    }

    pub fn targets(&self, node: &Node) -> Vec<String> {
        let nodes = self.nodes();
        let mut targets = BTreeSet::new();

        for object in self.objects.values() {
            if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Link") {
                continue;
            }
            let output = object.pointer("/info/output-node-id").and_then(number);
            let input = object.pointer("/info/input-node-id").and_then(number);
            let target = if node.is_playback() && output == Some(node.reference.id) {
                input
            } else if node.is_capture() && input == Some(node.reference.id) {
                output
            } else {
                None
            };

            if let Some(target) = nodes.iter().find(|n| Some(n.reference.id) == target) {
                targets.insert(target.name.clone());
            }
        }

        targets.into_iter().collect()
    }

    pub fn profiles(&self, node: &Node) -> Vec<Profile> {
        let Some(device) = node
            .props
            .get("device.id")
            .and_then(|id| id.parse::<u32>().ok())
            .and_then(|id| self.objects.get(&id).map(|object| (id, object)))
        else {
            return vec![];
        };
        let current = device
            .1
            .pointer("/info/params/Profile/0/index")
            .and_then(number);
        device
            .1
            .pointer("/info/params/EnumProfile")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|profile| {
                let index = profile.get("index").and_then(number)?;
                let available = profile.get("available").and_then(Value::as_str) != Some("no");
                Some(Profile {
                    device: device.0,
                    serial: device
                        .1
                        .pointer("/info/props/object.serial")
                        .and_then(scalar)?,
                    index,
                    name: profile
                        .get("description")
                        .or_else(|| profile.get("name"))
                        .and_then(Value::as_str)?
                        .into(),
                    current: current == Some(index),
                    available,
                })
            })
            .collect()
    }
}

fn merge(target: &mut Value, update: Value) {
    match (target, update) {
        (Value::Object(target), Value::Object(update)) => {
            for (key, value) in update {
                if value.is_null() {
                    target.remove(&key);
                } else {
                    merge(target.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        (target, update) => *target = update,
    }
}

fn number(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

#[derive(Clone)]
pub struct Profile {
    pub device: u32,
    pub serial: String,
    pub index: u32,
    pub name: String,
    pub current: bool,
    pub available: bool,
}

#[derive(Clone)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub nodes: Vec<Node>,
    pub identity: Option<(String, String)>,
    pub persistent: bool,
    pub capture: bool,
}

pub fn rows(nodes: &[Node], config: &Config) -> [Vec<Row>; 3] {
    let mut sections: [Vec<Row>; 3] = Default::default();
    let mut groups = BTreeMap::<String, Row>::new();

    for node in nodes {
        let identity = node
            .identity()
            .map(|(key, value)| (key.to_string(), value.to_string()));

        if node.is_stream() {
            let key = format!(
                "{:?}:{}",
                identity
                    .as_ref()
                    .map(|(key, value)| format!("{key}:{value}"))
                    .unwrap_or_else(|| format!("node:{}", node.reference.serial)),
                node.is_capture()
            );
            let row = groups.entry(key.clone()).or_insert_with(|| Row {
                key,
                name: node.name.clone(),
                nodes: vec![],
                identity,
                persistent: false,
                capture: node.is_capture(),
            });
            row.persistent |= config.persistent(node);
            row.nodes.push(node.clone());
        } else {
            sections[usize::from(node.is_source())].push(Row {
                key: format!("node:{}", node.reference.serial),
                name: node.name.clone(),
                nodes: vec![node.clone()],
                identity: None,
                persistent: false,
                capture: false,
            });
        }
    }

    for rule in &config.applications {
        if !nodes
            .iter()
            .any(|node| node.is_playback() && rule.matches(node))
        {
            let key = format!("inactive:{}:{}", rule.property, rule.value);
            groups.entry(key.clone()).or_insert(Row {
                key,
                name: rule.value.clone(),
                nodes: vec![],
                identity: Some((rule.property.clone(), rule.value.clone())),
                persistent: rule.muted,
                capture: false,
            });
        }
    }

    sections[2] = groups.into_values().collect();

    for section in &mut sections {
        section.sort_by_key(|row| (row.name.to_lowercase(), row.key.clone()));
    }

    sections
}

pub fn common_volume(
    nodes: &[Node],
    volumes: &BTreeMap<NodeRef, Volume>,
) -> (Option<Volume>, bool) {
    let values: Vec<_> = nodes
        .iter()
        .filter_map(|node| volumes.get(&node.reference).copied())
        .collect();
    let mixed = values.len() != nodes.len()
        || values.windows(2).any(|pair| {
            (pair[0].level - pair[1].level).abs() > 0.005 || pair[0].muted != pair[1].muted
        });
    let maximum = values.iter().map(|v| v.level).reduce(f64::max);
    (
        maximum.map(|level| Volume {
            level,
            muted: values.iter().all(|v| v.muted),
        }),
        mixed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changing_one_default_preserves_the_other_and_null_removes_it() {
        let mut graph = Graph::default();
        graph.apply(vec![json!({
            "id": 1,
            "type": "PipeWire:Interface:Metadata",
            "props": { "metadata.name": "default" },
            "metadata": [
                {
                    "subject": 0,
                    "key": "default.audio.sink",
                    "value": { "name": "speakers" }
                },
                {
                    "subject": 0,
                    "key": "default.audio.source",
                    "value": { "name": "mic" }
                }
            ]
        })]);
        let headset = Node::test(
            3,
            "Audio/Sink",
            BTreeMap::from([("node.name".into(), "headset".into())]),
        );
        let microphone = Node::test(
            4,
            "Audio/Source",
            BTreeMap::from([("node.name".into(), "mic".into())]),
        );

        graph.apply(vec![json!({
            "id": 1,
            "metadata": [{
                "subject": 0,
                "key": "default.audio.sink",
                "value": { "name": "headset" }
            }]
        })]);

        assert!(graph.is_default(&headset));
        assert!(graph.is_default(&microphone));

        graph.apply(vec![json!({
            "id": 1,
            "metadata": [{
                "subject": 0,
                "key": "default.audio.source",
                "value": null
            }]
        })]);

        assert!(!graph.is_default(&microphone));
        assert!(graph.is_default(&headset));
    }

    #[test]
    fn a_card_with_an_off_profile_remains_available_for_profile_selection() {
        let mut graph = Graph::default();
        graph.apply(vec![json!({
            "id": 2,
            "type": "PipeWire:Interface:Device",
            "info": {
                "props": {
                    "object.serial": 20,
                    "media.class": "Audio/Device",
                    "device.description": "USB card"
                },
                "params": {
                    "Profile": [{ "index": 0 }],
                    "EnumProfile": [
                        { "index": 0, "name": "off", "available": "yes" },
                        { "index": 1, "name": "stereo", "available": "yes" }
                    ]
                }
            }
        })]);

        let card = graph.nodes().into_iter().find(Node::is_card).unwrap();
        let profiles = graph.profiles(&card);

        assert_eq!(card.name, "USB card");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "off");
        assert!(profiles[0].current);
        assert_eq!(profiles[1].name, "stereo");
        assert!(!profiles[1].current);
    }

    #[test]
    fn partial_updates_preserve_properties_but_reused_ids_start_with_fresh_state() {
        let mut graph = Graph::default();
        graph.apply(vec![json!({
            "id": 9,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "object.serial": 10,
                    "media.class": "Audio/Sink",
                    "node.name": "speakers"
                },
                "params": { "Props": [{ "mute": false }] }
            }
        })]);
        let original_device = graph.nodes().remove(0).reference;

        graph.apply(vec![json!({
            "id": 9,
            "info": { "params": { "Props": [{ "mute": true }] } }
        })]);

        assert_eq!(graph.nodes()[0].name, "speakers");
        assert_eq!(
            graph.objects[&9]["info"]["params"]["Props"][0]["mute"],
            true
        );

        graph.apply(vec![json!({ "id": 9, "info": null })]);
        graph.apply(vec![json!({
            "id": 9,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "object.serial": 11,
                    "media.class": "Audio/Sink",
                    "node.name": "headset"
                }
            }
        })]);

        assert!(!graph.contains(&original_device));
        assert_eq!(graph.nodes()[0].name, "headset");
        assert!(graph.objects[&9].pointer("/info/params/Props").is_none());
    }

    #[test]
    fn playback_streams_are_grouped_separately_from_recording_with_mixed_volume() {
        let properties = BTreeMap::from([(BINARY.into(), "firefox".into())]);
        let first_playback = Node::test(1, "Stream/Output/Audio", properties.clone());
        let second_playback = Node::test(2, "Stream/Output/Audio", properties.clone());
        let recording = Node::test(3, "Stream/Input/Audio", properties);
        let volumes = BTreeMap::from([
            (
                first_playback.reference.clone(),
                Volume {
                    level: 0.4,
                    muted: false,
                },
            ),
            (
                second_playback.reference.clone(),
                Volume {
                    level: 0.8,
                    muted: true,
                },
            ),
        ]);

        let sections = rows(
            &[first_playback, second_playback, recording],
            &Config::default(),
        );
        let applications = &sections[2];
        let playback = applications.iter().find(|row| !row.capture).unwrap();
        let recording = applications.iter().find(|row| row.capture).unwrap();
        let (volume, mixed) = common_volume(&playback.nodes, &volumes);

        assert_eq!(applications.len(), 2);
        assert_eq!(playback.nodes.len(), 2);
        assert_eq!(recording.nodes.len(), 1);
        assert!(mixed);
        assert_eq!(volume.unwrap().level, 0.8);
    }
}
