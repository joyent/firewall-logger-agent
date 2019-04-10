use crate::parser::{self, CfwEvent};
use crate::zones::{Vmobjs, Zonedid};
use bytes::Bytes;
use crossbeam::channel::{self, select, Receiver, SendError, Sender};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Debug, Serialize)]
struct LogEvent<'a> {
    #[serde(flatten)]
    event: CfwEvent,
    vm: &'a str,
    alias: &'a str,
}

/// A signal that can be sent to the logger
pub enum LoggerSignal {
    /// Tell the thread to flush and shutdown
    Shutdown,
    /// Tell the thread to rotate the log file
    Rotate,
}

pub struct Logger {
    handle: thread::JoinHandle<()>,
    sender: channel::Sender<Bytes>,
    signal: channel::Sender<LoggerSignal>,
}

impl Logger {
    pub fn send(&self, b: Bytes) -> Result<(), SendError<Bytes>> {
        self.sender.send(b)
    }

    pub fn shutdown(&self) -> Result<(), SendError<LoggerSignal>> {
        self.signal.send(LoggerSignal::Shutdown)
    }
}

fn open_file(vm: String, customer: String) -> std::io::Result<File> {
    let path: PathBuf = ["/var/log/firewall", &customer, &vm, "current.log"]
        .iter()
        .collect();

    // we know the unwrap is safe because we just created the path above
    std::fs::create_dir_all(path.parent().unwrap())?;

    // TODO instead of truncating the existing file we should try to stat it first and open it or
    // rotate it before creating the new file
    Ok(File::create(path)?)
}

fn log_event<W: Write>(bytes: Bytes, writer: &mut W, vmobjs: &Vmobjs) {
    // force the event type for now
    let event = parser::traffic_event(&bytes).unwrap().1;
    let vmobjs = vmobjs.read().unwrap();
    let vmobj = vmobjs
        .get(&event.zone())
        .expect("we should have the zonedid:uuid mapping already");
    // Check if the zone has an alias set, if not we provide a default one
    // Note instead of String::as_ref we could also use "|s| &**s"
    let alias = vmobj.alias.as_ref().map_or("", String::as_ref);
    let event = LogEvent {
        event,
        vm: &vmobj.uuid,
        alias: &alias,
    };
    writeln!(writer, "{}", serde_json::to_string(&event).unwrap()).unwrap();
}

fn _start_logger(
    vm: String,
    customer: String,
    vmobjs: Vmobjs,
    events: channel::Receiver<Bytes>,
    signal: channel::Receiver<LoggerSignal>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("logger".to_string())
        .spawn(move || {
            let mut file = match open_file(vm, customer) {
                Ok(file) => file,
                Err(e) => {
                    // XXX CMON?
                    error!("failed to open log file: {}", e);
                    return;
                }
            };

            // TODO figure out how much to buffer before a write. This is completely a guess right
            // now.
            let mut writer = BufWriter::with_capacity(10 * 1024 * 1024, file);

            loop {
                select! {
                    recv(events) -> bytes => {
                        // TODO handle disconnected channel
                        if let Ok(bytes) = bytes {
                            log_event(bytes, &mut writer, &vmobjs);
                        }
                    }
                    recv(signal) -> signal => {
                        // TODO handle disconnected channel
                        if let Ok(signal) = signal {
                            match signal {
                                LoggerSignal::Rotate => (),
                                LoggerSignal::Shutdown => {
                                    let _res = writer.flush();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        })
        .expect("failed to spawn IpfReader thread")
}

/// Return a Logger if we have information for the zone already otherwise return None
pub fn start_logger(zonedid: Zonedid, vmobjs: Vmobjs) -> Option<Logger> {
    let (event_tx, event_rx) = channel::unbounded();
    let (signal_tx, signal_rx) = channel::bounded(1);
    let vms = vmobjs.read().unwrap();
    if let Some(vm) = vms.get(&zonedid) {
        let handle = _start_logger(
            vm.uuid.clone(),
            vm.owner_uuid.clone(),
            vmobjs.clone(),
            event_rx,
            signal_rx,
        );
        return Some(Logger {
            handle,
            sender: event_tx,
            signal: signal_tx,
        });
    }
    None
}
