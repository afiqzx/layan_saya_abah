use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use epoll::{ControlOptions::*, Event, Events};

enum ConnectionState {
    Read {
        request: [u8; 1024],
        read: usize,
    },
    Write {
        response: &'static [u8],
        written: usize,
    },
    Flush,
}

fn main() {
    let listener = TcpListener::bind("localhost:3000").unwrap();
    listener.set_nonblocking(true).unwrap();

    let epoll = epoll::create(false).unwrap();

    let event = Event::new(Events::EPOLLIN, listener.as_raw_fd() as _);
    epoll::ctl(epoll, EPOLL_CTL_ADD, listener.as_raw_fd(), event).unwrap();
    let mut conn_map = HashMap::new();

    loop {
        let mut events = [Event::new(Events::empty(), 0); 1024];
        let timeout = -1; // block until something happens
        let num_events = epoll::wait(epoll, timeout, &mut events).unwrap();
        let mut completed = Vec::new();

        'next: for event in &events[..num_events] {
            let fd = event.data as i32;
            println!("the fd is {}", fd);

            if fd == listener.as_raw_fd() {
                match listener.accept() {
                    Ok((conn, _)) => {
                        conn.set_nonblocking(true).unwrap();
                        let fd = conn.as_raw_fd();

                        // register the connection with epoll
                        let event = Event::new(Events::EPOLLIN | Events::EPOLLOUT, fd as _);
                        epoll::ctl(epoll, EPOLL_CTL_ADD, fd, event).unwrap();

                        // kep track of connection state
                        let state = ConnectionState::Read {
                            request: [0u8; 1024],
                            read: 0,
                        };

                        conn_map.insert(fd, (conn, state));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("{e}"),
                };

                println!("listhener fd is {}", listener.as_raw_fd());
                continue 'next;
            }

            let (conn, state) = conn_map.get_mut(&fd).unwrap();

            if let ConnectionState::Read { request, read } = state {
                loop {
                    // try reading
                    match conn.read(&mut request[*read..]) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly becuase 0 bytes is read");
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => *read += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue 'next,
                        Err(e) => panic!("{e}"),
                    }

                    if request.get(*read - 4..*read) == Some(b"\r\n\r\n") {
                        break;
                    }
                }

                // Hello world
                let response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length:12\n",
                    "Connection: close\r\n\r\n",
                    "Hello World!"
                )
                .as_bytes();
                *state = ConnectionState::Write {
                    response,
                    written: 0,
                };
            }

            if let ConnectionState::Write { response, written } = state {
                loop {
                    // write response bytes
                    match conn.write(&response[*written..]) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly");
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => *written += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue 'next,
                        Err(e) => panic!("{e}"),
                    }

                    if *written == response.len() {
                        break;
                    }
                }

                *state = ConnectionState::Flush;
            }

            if let ConnectionState::Flush = state {
                match conn.flush() {
                    Ok(_) => {
                        completed.push(fd);
                        conn.flush().unwrap();
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue 'next,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        // remove completed connection
        for fd in completed.iter().rev() {
            let (conn, _state) = conn_map.remove(&fd).unwrap();
            drop(conn);
        }
    }
}

fn handle_connection(mut conn: TcpStream) -> io::Result<()> {
    let mut request = [0u8; 1024];
    let mut read = 0;

    loop {
        // try reading
        let num_bytes = conn.read(&mut request[read..])?;

        // if the client dc
        if num_bytes == 0 {
            println!("client disconnected unexpectedly");
            return Ok(());
        }

        // keep track read bytes
        read += num_bytes;

        // break at end of request
        if request.get(read - 4..read) == Some(b"\r\n\r\n") {
            break;
        }
    }

    let req_string = String::from_utf8_lossy(&request[..read]);
    println!("{req_string}");

    // Hello world
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Length:12\n",
        "Connection: close\r\n\r\n",
        "Hello World!"
    );

    let mut written = 0;
    println!("{}", response.len());

    loop {
        // write response bytes
        let num_bytes = conn.write(response[written..].as_bytes())?;

        // if the client dc
        if num_bytes == 0 {
            println!("client disconnected unexpectedly");
            return Ok(());
        }

        written += num_bytes;

        if written == response.len() {
            break;
        }
    }

    conn.flush()
}
