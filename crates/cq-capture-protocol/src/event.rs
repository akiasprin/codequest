use crate::NativeValue;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentEntry {
    pub key: NativeValue,
    pub value: NativeValue,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitObservation {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub spawn_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Event {
    InvocationStarted {
        recorder_pid: u32,
        command: NativeValue,
        args: Vec<NativeValue>,
        cwd: NativeValue,
        environment: Vec<EnvironmentEntry>,
        removed_environment: Vec<NativeValue>,
    },
    ArgumentFileObserved {
        path: NativeValue,
        contents: Option<NativeValue>,
        error: Option<String>,
    },
    ProcessStarted {
        pid: u32,
    },
    InvocationFinished {
        exit: ExitObservation,
        reporting_errors: Vec<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u32,
    pub run_id: String,
    pub invocation_id: String,
    pub seq: u64,
    pub time_ms: u64,
    pub event: Event,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub run_id: String,
    pub invocation_id: String,
    pub seq: u64,
    pub accepted: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Packet {
    Event(Envelope),
    Ack(Ack),
}
