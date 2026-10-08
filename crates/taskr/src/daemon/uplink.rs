use super::*;

// host.go/liveness.go expire host heartbeats after 30s. Ten seconds leaves
// two missed intervals of headroom without re-running the observation pass.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(10);
pub(super) const LEGACY: Duration = Duration::from_secs(5);
#[derive(Default)]
pub(super) struct Uplink {
    pub epoch: Option<String>,
    pub generation: i64,
    pub agents: BTreeMap<String, Value>,
    pub resync: bool,
    full: u64,
    delta: u64,
    heartbeat: u64,
    skipped: u64,
    last_sent: Option<Instant>,
}
impl Uplink {
    pub fn sent(&self) -> u64 {
        self.full + self.delta + self.heartbeat
    }
    pub fn counters(&self) -> Value {
        json!({"full":self.full,"delta":self.delta,"heartbeat":self.heartbeat,"skipped_unchanged":self.skipped})
    }
    pub fn interval(&self) -> Duration {
        if self.epoch.is_some() && !self.resync {
            HEARTBEAT
        } else {
            LEGACY
        }
    }
    pub fn observe(
        &mut self,
        raw: &str,
        dir: &Path,
        agents: BTreeMap<String, Value>,
    ) -> Result<Option<Value>> {
        let full = self.epoch.is_none() || self.resync;
        let (kind, changed, removed) = if full {
            (
                "observe",
                agents.values().cloned().collect::<Vec<_>>(),
                Vec::new(),
            )
        } else if agents != self.agents {
            (
                "delta",
                agents
                    .iter()
                    .filter(|(pane, a)| self.agents.get(*pane) != Some(*a))
                    .map(|(_, a)| a.clone())
                    .collect(),
                self.agents
                    .keys()
                    .filter(|p| !agents.contains_key(*p))
                    .cloned()
                    .collect(),
            )
        } else {
            if self.last_sent.is_some_and(|at| at.elapsed() >= HEARTBEAT) {
                return self.heartbeat(raw, dir).map(Some);
            }
            self.skipped += 1;
            // Herdr notifications often describe our own token writes. They need
            // no uplink; only the scheduled liveness pass sends a heartbeat.
            return Ok(None);
        };
        let reply = self.call(raw, dir, kind, &json!(changed), &json!(removed))?;
        self.agents = agents;
        self.accept(&reply);
        Ok(Some(reply))
    }
    pub fn heartbeat(&mut self, raw: &str, dir: &Path) -> Result<Value> {
        self.skipped += 1;
        let reply = self.call(raw, dir, "heartbeat", &Value::Null, &Value::Null)?;
        self.accept(&reply);
        Ok(reply)
    }
    fn call(
        &mut self,
        raw: &str,
        dir: &Path,
        kind: &str,
        agents: &Value,
        removed: &Value,
    ) -> Result<Value> {
        self.last_sent = Some(Instant::now());
        match kind {
            "observe" => self.full += 1,
            "delta" => self.delta += 1,
            _ => self.heartbeat += 1,
        }
        crate::net::daemon_host(
            raw,
            kind,
            agents,
            removed,
            self.epoch.as_deref(),
            self.generation,
            dir,
        )
        .map_err(transport)
    }
    fn accept(&mut self, reply: &Value) {
        self.epoch = reply["hostd"]["epoch"]
            .as_str()
            .filter(|_| reply["hostd"]["version"] == 1)
            .map(String::from);
        self.generation = reply["hostd"]["generation"].as_i64().unwrap_or(0);
        self.resync = false;
    }
}
