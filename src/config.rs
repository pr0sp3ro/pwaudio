use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::model::Node;

pub const BINARY: &str = "application.process.binary";
pub const NAME: &str = "application.name";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub applications: Vec<Rule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(rename = "match")]
    pub property: String,
    pub value: String,
    pub muted: bool,
}

impl Rule {
    pub fn matches(&self, node: &Node) -> bool {
        node.props.get(&self.property) == Some(&self.value)
    }
}

impl Config {
    pub fn parse(contents: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(contents).map_err(|e| format!("Invalid config: {e}"))?;

        for rule in &config.applications {
            if ![BINARY, NAME].contains(&rule.property.as_str()) || rule.value.is_empty() {
                return Err("Rules require match = application.process.binary or application.name and a nonempty value".into());
            }
        }

        Ok(config)
    }

    pub fn persistent(&self, node: &Node) -> bool {
        node.is_playback()
            && self
                .applications
                .iter()
                .any(|rule| rule.muted && rule.matches(node))
    }

    pub fn toggle(&mut self, nodes: &[Node], identity: (&str, &str)) {
        let active = self.applications.iter().any(|rule| {
            rule.muted
                && (nodes.iter().any(|node| rule.matches(node))
                    || (rule.property == identity.0 && rule.value == identity.1))
        });

        if active {
            for rule in &mut self.applications {
                if nodes.iter().any(|node| rule.matches(node))
                    || (rule.property == identity.0 && rule.value == identity.1)
                {
                    rule.muted = false;
                }
            }
        } else if let Some(rule) = self
            .applications
            .iter_mut()
            .find(|rule| rule.property == identity.0 && rule.value == identity.1)
        {
            rule.muted = true;
        } else {
            self.applications.push(Rule {
                property: identity.0.into(),
                value: identity.1.into(),
                muted: true,
            });
        }
    }
}

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    daemon: PathBuf,
}

impl Paths {
    #[cfg(test)]
    pub fn in_directory(directory: &Path) -> Self {
        Self {
            config: directory.join("config.toml"),
            daemon: directory.join("daemon.lock"),
        }
    }

    pub fn discover() -> Result<Self, String> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or("Neither XDG_CONFIG_HOME nor HOME is set")?;
        let directory = base.join("pwaudio");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|e| format!("Cannot create {}: {e}", directory.display()))?;
        Ok(Self {
            config: directory.join("config.toml"),
            daemon: directory.join("daemon.lock"),
        })
    }

    pub fn contents(&self) -> Result<String, String> {
        match fs::read_to_string(&self.config) {
            Ok(contents) => Ok(contents),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("Cannot read {}: {e}", self.config.display())),
        }
    }

    pub fn lock_daemon(&self) -> Result<File, String> {
        let file = lock_file(&self.daemon)?;
        FileExt::try_lock_exclusive(&file).map_err(|e| {
            format!("Cannot acquire daemon lock (is pwaudio daemon already running?): {e}")
        })?;
        Ok(file)
    }

    pub fn daemon_running(&self) -> Result<bool, String> {
        let file = lock_file(&self.daemon)?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(true),
            Err(e) => Err(format!("Cannot inspect daemon lock: {e}")),
        }
    }

    pub fn toggle(&self, nodes: &[Node], identity: (&str, &str)) -> Result<Config, String> {
        let lock = lock_file(&self.config.with_extension("lock"))?;
        let start = Instant::now();

        loop {
            match FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_secs(2) =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(format!("Cannot lock config: {e}")),
            }
        }

        let mut config = Config::parse(&self.contents()?)?;
        config.toggle(nodes, identity);
        let contents = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let temporary = self
            .config
            .with_extension(format!("{}.{}.tmp", std::process::id(), nonce));
        let result = (|| -> Result<(), String> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            file.write_all(contents.as_bytes())
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temporary, &self.config).map_err(|e| e.to_string())?;
            File::open(
                self.config
                    .parent()
                    .ok_or("Config has no parent directory")?,
            )
            .and_then(|dir| dir.sync_all())
            .map_err(|e| e.to_string())?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }

        result?;
        Ok(config)
    }
}

fn lock_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const FIREFOX_RULES: &str = r#"
[[applications]]
match = "application.name"
value = "Firefox"
muted = true

[[applications]]
match = "application.process.binary"
value = "firefox"
muted = true
"#;

    #[test]
    fn persistent_mute_matches_exact_properties_of_playback_streams_only() {
        let config = Config::parse(FIREFOX_RULES).unwrap();
        let playback = Node::test(
            1,
            "Stream/Output/Audio",
            BTreeMap::from([(BINARY.into(), "firefox".into())]),
        );
        let mut recording = playback.clone();
        recording.class = "Stream/Input/Audio".into();
        let mut different_case = playback.clone();
        different_case.props.insert(BINARY.into(), "Firefox".into());

        assert!(config.persistent(&playback));
        assert!(!config.persistent(&recording));
        assert!(!config.persistent(&different_case));
    }

    #[test]
    fn disabling_persistent_mute_disables_all_matching_rules() {
        let mut config = Config::parse(FIREFOX_RULES).unwrap();
        let playback = Node::test(
            1,
            "Stream/Output/Audio",
            BTreeMap::from([
                (NAME.into(), "Firefox".into()),
                (BINARY.into(), "firefox".into()),
            ]),
        );

        config.toggle(std::slice::from_ref(&playback), (BINARY, "firefox"));

        assert!(config.applications.iter().all(|rule| !rule.muted));
        assert!(!config.persistent(&playback));

        config.toggle(std::slice::from_ref(&playback), (BINARY, "firefox"));

        assert!(config.persistent(&playback));
    }
}
