//! A scripted HTTP/1.1 server on 127.0.0.1 for the provider tests: records every
//! request and answers each connection with the same [`Reply`].

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

#[derive(Debug, Clone)]
pub enum Reply {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// Reads the request, never answers; [`FakeServer::closed`] fires when the
    /// client closes the socket.
    Hang,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Self {
        Reply::Respond {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }
}

pub struct FakeServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    pub closed: Arc<Notify>,
}

impl FakeServer {
    pub async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Notify::new());
        let (reqs, done) = (requests.clone(), closed.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reqs, done, reply) = (reqs.clone(), done.clone(), reply.clone());
                tokio::spawn(serve(stream, reply, reqs, done));
            }
        });
        FakeServer {
            addr,
            requests,
            closed,
        }
    }

    /// `http://127.0.0.1:<port>/v1`
    pub fn base(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(
    mut stream: TcpStream,
    reply: Reply,
    requests: Arc<Mutex<Vec<Recorded>>>,
    closed: Arc<Notify>,
) {
    let Some(req) = read_request(&mut stream).await else {
        return;
    };
    requests.lock().unwrap().push(req);
    match reply {
        Reply::Respond {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (n, v) in headers {
                head.push_str(&format!("{n}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        Reply::Hang => {
            let mut buf = [0u8; 256];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            closed.notify_one();
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while data.len() < head_end + len {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&data[head_end..]).into_owned();
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}
