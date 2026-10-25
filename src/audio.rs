use std::{
    io::{BufReader, Read},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    config::Config,
    model::{Node, Volume},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

// One boundary for all control commands, also used by tests without touching audio.
pub trait Runner {
    fn run(&self, program: &str, arguments: &[String]) -> Result<String, String>;
}

pub struct Commands;

impl Runner for Commands {
    fn run(&self, program: &str, arguments: &[String]) -> Result<String, String> {
        let mut child = Command::new(program)
            .args(arguments)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot start {program}: {e}"))?;
        let stdout = child.stdout.take().ok_or("Missing command stdout")?;
        let stderr = child.stderr.take().ok_or("Missing command stderr")?;
        // Drain both pipes while waiting, so a full pipe cannot deadlock the timeout.
        let output = thread::spawn(move || read_output(stdout));
        let errors = thread::spawn(move || read_output(stderr));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if started.elapsed() < COMMAND_TIMEOUT => {
                    thread::sleep(Duration::from_millis(10))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(match result {
                        Err(e) => format!("Cannot wait for {program}: {e}"),
                        _ => format!("{program} timed out after 2 seconds"),
                    });
                }
            }
        };
        let output = output.join().unwrap_or_default();
        let errors = errors.join().unwrap_or_default();

        if status?.success() {
            Ok(output)
        } else {
            Err(format!(
                "{program} {} failed: {}",
                arguments.join(" "),
                errors.trim()
            ))
        }
    }
}

fn read_output(mut reader: impl Read) -> String {
    let mut result = Vec::new();
    let mut buffer = [0; 4096];

    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        let remaining = 65536_usize.saturating_sub(result.len());
        result.extend_from_slice(&buffer[..count.min(remaining)]);
    }

    String::from_utf8_lossy(&result).into_owned()
}

pub fn volume(runner: &impl Runner, node: &Node) -> Result<Volume, String> {
    let output = runner.run(
        "wpctl",
        &["get-volume".into(), node.reference.id.to_string()],
    )?;
    let level = output
        .strip_prefix("Volume:")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| format!("Invalid volume response: {}", output.trim()))?;
    Ok(Volume {
        level,
        muted: output.contains("[MUTED]"),
    })
}

pub fn set_mute(runner: &impl Runner, node: &Node, muted: bool) -> Result<(), String> {
    runner.run(
        "wpctl",
        &[
            "set-mute".into(),
            node.reference.id.to_string(),
            if muted { "1" } else { "0" }.into(),
        ],
    )?;
    Ok(())
}

pub fn enforce(
    runner: &impl Runner,
    config: &Config,
    node: &Node,
    current: Volume,
) -> Result<bool, String> {
    if config.persistent(node) && !current.muted {
        set_mute(runner, node, true)?;
        return Ok(true);
    }

    Ok(false)
}

pub enum MonitorEvent {
    Changes(Vec<Value>),
    Failed(String),
}

pub struct Monitor {
    child: Child,
    pub events: Receiver<MonitorEvent>,
    stderr: Arc<Mutex<String>>,
}

impl Monitor {
    pub fn start() -> Result<Self, String> {
        let mut child = Command::new("pw-dump")
            .args(["--monitor", "--no-colors"])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot start pw-dump: {e}"))?;
        let stdout = child.stdout.take().ok_or("Missing pw-dump stdout")?;
        let stderr = child.stderr.take().ok_or("Missing pw-dump stderr")?;
        let (sender, events) = mpsc::channel();
        let error_text = Arc::new(Mutex::new(String::new()));
        let error_target = error_text.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut bytes = [0; 1024];

            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }

                if let Ok(mut text) = error_target.lock() {
                    text.push_str(&String::from_utf8_lossy(&bytes[..count]));

                    if text.len() > 4096 {
                        text.clear();
                    }
                }
            }
        });
        thread::spawn(move || {
            let stream = serde_json::Deserializer::from_reader(BufReader::new(stdout))
                .into_iter::<Vec<Value>>();

            for batch in stream {
                match batch {
                    Ok(changes) => {
                        if sender.send(MonitorEvent::Changes(changes)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ =
                            sender.send(MonitorEvent::Failed(format!("Invalid pw-dump JSON: {e}")));
                        return;
                    }
                }
            }

            let _ = sender.send(MonitorEvent::Failed("PipeWire monitor disconnected".into()));
        });
        Ok(Self {
            child,
            events,
            stderr: error_text,
        })
    }

    pub fn error(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BINARY;
    use std::{cell::RefCell, collections::BTreeMap};

    #[derive(Default)]
    struct FakeCommands {
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Runner for FakeCommands {
        fn run(&self, program: &str, arguments: &[String]) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((program.into(), arguments.to_vec()));
            Ok(String::new())
        }
    }

    #[test]
    fn persistent_mute_is_reapplied_only_after_a_stream_is_unmuted() {
        let playback = Node::test(
            42,
            "Stream/Output/Audio",
            BTreeMap::from([(BINARY.into(), "firefox".into())]),
        );
        let config = Config::parse(
            r#"
[[applications]]
match = "application.process.binary"
value = "firefox"
muted = true
"#,
        )
        .unwrap();
        let commands = FakeCommands::default();
        let unmuted = Volume {
            level: 1.0,
            muted: false,
        };
        let muted = Volume {
            level: 1.0,
            muted: true,
        };

        assert!(enforce(&commands, &config, &playback, unmuted).unwrap());
        assert_eq!(commands.calls.borrow().len(), 1);

        assert!(!enforce(&commands, &config, &playback, muted).unwrap());
        assert_eq!(commands.calls.borrow().len(), 1);

        assert!(enforce(&commands, &config, &playback, unmuted).unwrap());
        let calls = commands.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "wpctl");
        assert_eq!(calls[0].1, ["set-mute", "42", "1"]);
        assert_eq!(calls[1], calls[0]);
    }

    #[test]
    fn monitor_parser_reads_successive_arrays_and_rejects_truncated_json() {
        let input = r#"
[
    { "id": 1 }
]
[
    { "id": 1, "info": null }
]
[
    {
"#;
        let mut stream =
            serde_json::Deserializer::from_reader(input.as_bytes()).into_iter::<Vec<Value>>();

        let initial_objects = stream.next().unwrap().unwrap();
        let removed_objects = stream.next().unwrap().unwrap();
        let truncated_update = stream.next().unwrap();

        assert_eq!(initial_objects[0]["id"], 1);
        assert_eq!(removed_objects[0]["id"], 1);
        assert!(removed_objects[0]["info"].is_null());
        assert!(truncated_update.is_err());
    }
}
