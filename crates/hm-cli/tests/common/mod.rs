//! A fake KISS TNC for tests: it plays the radio channel, relaying every data
//! frame from one client to all the others and optionally dropping every n-th.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use hm_bearer::kiss;

pub struct FakeTnc {
    pub addr: SocketAddr,
    pub dropped: Arc<AtomicUsize>,
    /// While set, every frame is lost: the band is dead.
    pub blocked: Arc<AtomicBool>,
}

pub fn fake_tnc(drop_every: usize) -> FakeTnc {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let clients: Arc<Mutex<Vec<(usize, TcpStream)>>> = Arc::default();

    let dropped = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicUsize::new(0));
    let d = dropped.clone();
    let blocked = Arc::new(AtomicBool::new(false));
    let b = blocked.clone();
    thread::spawn(move || {
        for (id, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { return };
            clients.lock().unwrap().push((id, stream.try_clone().unwrap()));
            let (clients, dropped, seen, blocked) = (clients.clone(), d.clone(), seen.clone(), b.clone());
            thread::spawn(move || {
                let mut dec = kiss::Decoder::new(4096);
                let mut buf = [0u8; 4096];
                let mut frames = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    dec.push(&buf[..n], &mut frames);
                    for f in frames.drain(..) {
                        let k = seen.fetch_add(1, Ordering::SeqCst) + 1;
                        if blocked.load(Ordering::SeqCst) || (drop_every > 0 && k % drop_every == 0) {
                            dropped.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                        let wire = kiss::data_frame(f.port, &f.data);
                        for (other, s) in clients.lock().unwrap().iter_mut() {
                            if *other != id {
                                let _ = s.write_all(&wire);
                            }
                        }
                    }
                }
            });
        }
    });
    FakeTnc {
        addr,
        dropped,
        blocked,
    }
}
