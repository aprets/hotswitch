//! The HDMI switch (an ESPHome device) that sends the PS5 to one desk at a time.
//!
//! Entering PS5 mode on a Mac first selects that Mac's output on the switch. The switch's
//! event stream is then used only to leave: when the switch moves to another output (the other
//! Mac took the PS5, or someone pressed its button), PS5 mode ends here. Nothing here ever
//! enters PS5 mode on its own. A Mac that leaves PS5 mode by itself sends the switch back to
//! the TV, but only while the switch still shows that Mac's output.

use crate::settings::HdmiSwitch;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

/// The TV's output: where the switch goes when nobody is on the PS5.
pub const IDLE_OUTPUT: &str = "A";

pub struct Switch {
    config: HdmiSwitch,
    client: reqwest::blocking::Client,
}

impl Switch {
    pub fn new(config: HdmiSwitch) -> Result<Switch, String> {
        // Per read: the switch pings every 10 s, so 35 s of silence means the stream is dead.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Switch { config, client })
    }

    /// This Mac's output letter.
    pub fn output(&self) -> &str {
        &self.config.output
    }

    /// Send the PS5 to `output`, trying each address in turn. A dry run only logs it: the
    /// switch is hardware. (Its event stream is still followed; that only reads.)
    pub fn select(&self, output: &str) -> Result<(), String> {
        if crate::settings::dry_run() {
            eprintln!("hdmi switch dry run: would select {output}");
            return Ok(());
        }
        let mut errors = Vec::new();
        for url in &self.config.urls {
            let response = self
                .client
                .post(format!("{url}/select/HDMI%20input/set?option={output}"))
                .basic_auth(&self.config.user, Some(&self.config.password))
                // ESPHome answers a POST without a length with 411 Length Required, and
                // reqwest sends none for an empty body.
                .header(reqwest::header::CONTENT_LENGTH, "0")
                .timeout(Duration::from_secs(3))
                .send();
            match response {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => errors.push(format!("{url}: HTTP {}", r.status())),
                Err(e) => errors.push(format!("{url}: {}", causes(&e))),
            }
        }
        if errors.is_empty() {
            errors.push("no address configured".into());
        }
        Err(errors.join("; "))
    }

    /// Follow the event stream forever, calling `on_output` with the live output whenever it
    /// changes, and again after every reconnect. Backs off up to 30 s while unreachable.
    pub fn watch(&self, on_output: impl Fn(String)) -> ! {
        let mut failures = 0;
        loop {
            if self
                .config
                .urls
                .iter()
                .any(|url| self.stream(url, &on_output))
            {
                failures = 0;
            } else {
                failures += 1;
            }
            thread::sleep(backoff(failures));
        }
    }

    /// One connection to `url/events`. True if it connected, once the stream has ended.
    fn stream(&self, url: &str, on_output: &impl Fn(String)) -> bool {
        let response = self
            .client
            .get(format!("{url}/events"))
            .basic_auth(&self.config.user, Some(&self.config.password))
            .send();
        let response = match response {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                eprintln!("hdmi switch: {url}: HTTP {}", r.status());
                return false;
            }
            Err(e) => {
                eprintln!("hdmi switch: {url}: {}", causes(&e));
                return false;
            }
        };
        eprintln!("hdmi switch: following {url}");
        let mut tracker = Tracker::default();
        for line in BufReader::new(response).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(output) = tracker.line(&line) {
                on_output(output);
            }
        }
        eprintln!("hdmi switch: {url} stream ended");
        true
    }
}

/// An error with its sources: reqwest's own text ("error sending request") hides the cause.
fn causes(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

/// Wait before reconnecting: 1 s after a stream that worked, doubling to 30 s while failing.
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(30))
}

/// Turns the switch's server-sent events into the live output, reported when it changes.
#[derive(Default)]
pub struct Tracker {
    event: String,
    active: Option<String>,
}

impl Tracker {
    pub fn line(&mut self, line: &str) -> Option<String> {
        if let Some(name) = line.strip_prefix("event:") {
            self.event = name.trim().to_string();
            return None;
        }
        let Some(data) = line.strip_prefix("data:") else {
            if line.is_empty() {
                self.event.clear();
            }
            return None;
        };
        if self.event != "state" {
            return None;
        }
        let output = live_output(data.trim())?;
        if self.active.as_ref() == Some(&output) {
            return None;
        }
        self.active = Some(output.clone());
        Some(output)
    }
}

/// The output a `state` event names: the select's value, or an output LED turning on. An LED
/// turning off (mid-switch, nothing lit) says nothing yet.
fn live_output(data: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(data).ok()?;
    let id = event.get("id")?.as_str()?;
    let state = event.get("state")?.as_str()?;
    if id == "select/HDMI input" {
        return Some(state.to_string());
    }
    let letter = id
        .strip_prefix("binary_sensor/Input ")?
        .strip_suffix(" LED")?;
    (state == "ON").then(|| letter.to_string())
}

/// An event says `active` is live. True when this Mac, in PS5 mode, should leave because the
/// PS5 went to another output. Events never enter PS5 mode.
pub fn should_leave(active: &str, mine: &str, in_ps5: bool) -> bool {
    in_ps5 && active != mine
}

/// Where to send the switch when this Mac leaves PS5 mode by itself: back to the TV, but only
/// while the switch still shows this Mac's output. Unknown means leave it alone.
pub fn release_target(active: Option<&str>, mine: &str) -> Option<&'static str> {
    (active == Some(mine) && mine != IDLE_OUTPUT).then_some(IDLE_OUTPUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A localhost HTTP server that answers each connection with the next canned response and
    /// records each request's head.
    fn serve(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        thread::spawn(move || {
            for (stream, response) in listener.incoming().zip(responses) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                log.lock().unwrap().push(head);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (url, seen)
    }

    fn switch(urls: Vec<String>) -> Switch {
        Switch::new(HdmiSwitch {
            urls,
            user: "hotswitch".into(),
            password: "pw".into(),
            output: "C".into(),
        })
        .unwrap()
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";

    fn events(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        )
    }

    fn state(id: &str, state: &str) -> String {
        format!("event: state\ndata: {{\"id\":\"{id}\",\"value\":\"{state}\",\"state\":\"{state}\"}}\n\n")
    }

    #[test]
    fn select_posts_the_output_with_basic_auth() {
        let (url, seen) = serve(vec![OK.into()]);
        assert_eq!(switch(vec![url]).select("C"), Ok(()));
        let head = seen.lock().unwrap()[0].to_lowercase();
        assert!(
            head.starts_with("post /select/hdmi%20input/set?option=c http/1.1"),
            "{head}"
        );
        // base64("hotswitch:pw")
        assert!(
            head.contains("authorization: basic ag90c3dpdgnoonb3"),
            "{head}"
        );
        assert!(head.contains("content-length: 0"), "{head}");
    }

    #[test]
    fn select_falls_back_to_the_next_address() {
        let (url, seen) = serve(vec![OK.into()]);
        let dead = "http://127.0.0.1:1".to_string();
        assert_eq!(switch(vec![dead.clone(), url]).select("B"), Ok(()));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(switch(vec![dead]).select("B").is_err());
    }

    #[test]
    fn select_reports_why_each_address_failed() {
        let (url, _) = serve(vec![
            "HTTP/1.1 411 Length Required\r\nContent-Length: 0\r\n\r\n".into(),
        ]);
        let dead = "http://127.0.0.1:1".to_string();
        let error = switch(vec![url.clone(), dead.clone()])
            .select("B")
            .unwrap_err();
        assert!(error.contains(&format!("{url}: HTTP 411")), "{error}");
        // The cause, not just reqwest's "error sending request".
        assert!(
            error.contains(&format!("{dead}: error sending request")),
            "{error}"
        );
        assert!(error.to_lowercase().contains("refused"), "{error}");
    }

    #[test]
    fn the_event_stream_reports_each_new_live_output() {
        let body = [
            "retry: 30000\nevent: ping\n\n".to_string(),
            state("binary_sensor/Input A LED", "OFF"),
            state("binary_sensor/Input C LED", "ON"),
            state("select/HDMI input", "C"),
            state("sensor/Uptime", "2403 s"),
            // Mid-switch: nothing lit says nothing; then B lights and the select follows.
            state("binary_sensor/Input C LED", "OFF"),
            state("binary_sensor/Input B LED", "ON"),
            state("select/HDMI input", "B"),
            "event: log\ndata: [D][select:015]: 'HDMI input' >> C\n\n".to_string(),
        ]
        .concat();
        let (url, seen) = serve(vec![events(&body)]);
        let outputs = Mutex::new(Vec::new());
        let connected =
            switch(vec![url.clone()]).stream(&url, &|o| outputs.lock().unwrap().push(o));
        assert!(connected);
        assert_eq!(*outputs.lock().unwrap(), ["C", "B"]);
        assert!(seen.lock().unwrap()[0]
            .to_lowercase()
            .starts_with("get /events"));
    }

    #[test]
    fn a_refused_stream_counts_as_a_failure() {
        let (url, _) = serve(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".into(),
        ]);
        assert!(!switch(vec![url.clone()]).stream(&url, &|_| {}));
    }

    #[test]
    fn events_only_ever_make_a_mac_leave() {
        assert!(should_leave("B", "C", true), "the other Mac took the PS5");
        assert!(should_leave("A", "C", true), "someone pressed the button");
        assert!(!should_leave("C", "C", true));
        assert!(!should_leave("C", "C", false), "never enters");
        assert!(!should_leave("B", "C", false));
    }

    #[test]
    fn leaving_sends_the_switch_to_the_tv_only_if_it_is_still_ours() {
        assert_eq!(release_target(Some("C"), "C"), Some("A"));
        assert_eq!(release_target(Some("B"), "C"), None, "the other Mac has it");
        assert_eq!(release_target(Some("A"), "C"), None, "already on the TV");
        assert_eq!(release_target(None, "C"), None, "unknown: leave it alone");
    }

    #[test]
    fn backoff_doubles_to_thirty_seconds() {
        let secs: Vec<u64> = (0..7).map(|f| backoff(f).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
    }
}
