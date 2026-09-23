use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct HttpProxy {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    maximum: Arc<AtomicUsize>,
    gate: Option<GateControl>,
    accept_thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct GateControl {
    suffix: String,
    state: Arc<(Mutex<GateState>, Condvar)>,
}

#[derive(Default)]
struct GateState {
    arrivals: usize,
    released: usize,
}

#[allow(dead_code)]
pub struct RequestGate {
    control: GateControl,
}

impl HttpProxy {
    #[allow(dead_code)]
    pub fn start(upstream: SocketAddr, chunked_responses: BTreeMap<String, Vec<u8>>) -> Self {
        Self::start_with_responses(upstream, None, chunked_responses, BTreeMap::new())
    }

    #[allow(dead_code)]
    pub fn start_gated(upstream: SocketAddr, suffix: &str) -> (Self, RequestGate) {
        let control = GateControl {
            suffix: suffix.to_owned(),
            state: Arc::new((Mutex::new(GateState::default()), Condvar::new())),
        };
        let gate = RequestGate {
            control: control.clone(),
        };
        (
            Self::start_with_responses(upstream, Some(control), BTreeMap::new(), BTreeMap::new()),
            gate,
        )
    }

    #[allow(dead_code)]
    pub fn start_gated_truncated(
        upstream: SocketAddr,
        path: &str,
        body: Vec<u8>,
    ) -> (Self, RequestGate) {
        let control = GateControl {
            suffix: path.to_owned(),
            state: Arc::new((Mutex::new(GateState::default()), Condvar::new())),
        };
        let gate = RequestGate {
            control: control.clone(),
        };
        (
            Self::start_with_responses(
                upstream,
                Some(control),
                BTreeMap::new(),
                BTreeMap::from([(path.to_owned(), body)]),
            ),
            gate,
        )
    }

    #[allow(dead_code)]
    pub fn start_truncated(upstream: SocketAddr, path: &str, body: Vec<u8>) -> Self {
        Self::start_with_responses(
            upstream,
            None,
            BTreeMap::new(),
            BTreeMap::from([(path.to_owned(), body)]),
        )
    }

    fn start_with_responses(
        upstream: SocketAddr,
        gate: Option<GateControl>,
        chunked_responses: BTreeMap<String, Vec<u8>>,
        truncated_responses: BTreeMap<String, Vec<u8>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let chunked_responses = Arc::new(chunked_responses);
        let truncated_responses = Arc::new(truncated_responses);
        let thread_stop = stop.clone();
        let thread_active = active.clone();
        let thread_maximum = maximum.clone();
        let thread_gate = gate.clone();
        let accept_thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let active = thread_active.clone();
                        let maximum = thread_maximum.clone();
                        let gate = thread_gate.clone();
                        let chunked_responses = chunked_responses.clone();
                        let truncated_responses = truncated_responses.clone();
                        std::thread::spawn(move || {
                            let _ = proxy_request(
                                client,
                                upstream,
                                gate.as_ref(),
                                &chunked_responses,
                                &truncated_responses,
                                &active,
                                &maximum,
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            maximum,
            gate,
            accept_thread: Some(accept_thread),
        }
    }

    pub fn uri(&self) -> String {
        format!("http://{}", self.address)
    }

    #[allow(dead_code)]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[allow(dead_code)]
    pub fn maximum(&self) -> usize {
        self.maximum.load(Ordering::SeqCst)
    }
}

impl Drop for HttpProxy {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.release_all();
        }
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(dead_code)]
impl RequestGate {
    pub fn wait_for_arrivals(&self, expected: usize, timeout: Duration) -> bool {
        let (lock, changed) = &*self.control.state;
        let state = lock.lock().unwrap();
        let (state, _) = changed
            .wait_timeout_while(state, timeout, |state| state.arrivals < expected)
            .unwrap();
        state.arrivals >= expected
    }

    pub fn release_one(&self) {
        let (lock, changed) = &*self.control.state;
        let mut state = lock.lock().unwrap();
        state.released += 1;
        changed.notify_all();
    }

    pub fn release_all(&self) {
        self.control.release_all();
    }
}

impl Drop for RequestGate {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl GateControl {
    fn arrive_and_wait(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap();
        let ticket = state.arrivals;
        state.arrivals += 1;
        changed.notify_all();
        while state.released <= ticket {
            state = changed.wait(state).unwrap();
        }
    }

    fn release_all(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.released = usize::MAX;
        changed.notify_all();
    }
}

fn proxy_request(
    mut client: TcpStream,
    upstream: SocketAddr,
    gate: Option<&GateControl>,
    chunked_responses: &BTreeMap<String, Vec<u8>>,
    truncated_responses: &BTreeMap<String, Vec<u8>>,
    active: &AtomicUsize,
    maximum: &AtomicUsize,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = client.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let first_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .unwrap_or_default();
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let counted = gate.is_some_and(|gate| path.ends_with(&gate.suffix));
    if counted {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(now, Ordering::SeqCst);
        gate.expect("counted response has a gate").arrive_and_wait();
    }
    if let Some(body) = truncated_responses.get(path) {
        write!(
            client,
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            body.len() + 10
        )?;
        client.write_all(body)?;
    } else if let Some(body) = chunked_responses.get(path) {
        write!(
            client,
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            body.len()
        )?;
        client.write_all(body)?;
        client.write_all(b"\r\n0\r\n\r\n")?;
    } else {
        let mut upstream_stream = TcpStream::connect(upstream)?;
        write!(
            upstream_stream,
            "GET {path} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n"
        )?;
        let mut response = Vec::new();
        upstream_stream.read_to_end(&mut response)?;
        client.write_all(&response)?;
    }
    if counted {
        active.fetch_sub(1, Ordering::SeqCst);
    }
    Ok(())
}
