// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.

//! A minimal HTTP/1.1 server on 127.0.0.1, the counterpart of the
//! `QHttpServer` + `QTcpServer` pairs of the `HAVE_QHTTPSERVER` upstream
//! tests: routes by path, one request per connection (`Connection: close`),
//! and every request is recorded so that the test can check its headers
//! (where upstream's route handlers call `QVERIFY`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A received request: path and headers (lower-case names).
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
}

impl HttpRequest {
    pub fn has_header(&self, name: &str) -> bool {
        self.headers.contains_key(&name.to_ascii_lowercase())
    }
}

/// A response: status, headers, body (`Content-Length` is added).
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn ok(headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> Self {
        Self::with_status(200, headers, body)
    }

    pub fn with_status(status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.into(),
        }
    }
}

type Route = Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>;

/// The server; stops when dropped.
pub struct HttpServer {
    port: u16,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        404 => "Not Found",
        _ => "Status",
    }
}

fn handle(
    mut stream: TcpStream,
    routes: &HashMap<String, Route>,
    requests: &Mutex<Vec<HttpRequest>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
    }
    let request = HttpRequest {
        method,
        path,
        headers,
    };
    requests.lock().unwrap().push(request.clone());
    let response = match routes.get(&request.path) {
        Some(route) => route(&request),
        None => HttpResponse::with_status(404, &[], "not found"),
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    for (k, v) in &response.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

impl HttpServer {
    /// Listens on a free port of 127.0.0.1 with `routes` (path → handler).
    pub fn start(routes: Vec<(&str, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes: HashMap<String, Route> =
            routes.into_iter().map(|(p, r)| (p.to_owned(), r)).collect();
        let thread = {
            let stop = stop.clone();
            let requests = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let _ = handle(stream, &routes, &requests);
                    }
                }
            })
        };
        Self {
            port,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The requests received so far.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop up.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A route handler.
pub fn route(f: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Route {
    Box::new(f)
}
