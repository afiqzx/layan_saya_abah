use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

enum ConnectionState {
    Read {
        request: Vec<u8>,
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

    let mut conn_vec = Vec::new();

    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(true).unwrap();

                // kep track of connection state
                let state = ConnectionState::Read {
                    request: Vec::new(),
                    read: 0,
                };

                conn_vec.push((conn, state));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        };

        let mut completed = Vec::new();

        'next: for (i, (conn, state)) in conn_vec.iter_mut().enumerate() {
            if let ConnectionState::Read { request, read } = state {
                loop {
                    // try reading
                    match conn.read(&mut request[*read..]) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly");
                            completed.push(i);
                        }
                        Ok(n) => *read += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue 'next,
                        Err(e) => panic!("{e}"),
                    }
                }
            }

            if let ConnectionState::Write { response, written } = state {
                // Hello world
                *response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length:12\n",
                    "Connection: close\r\n\r\n",
                    "Hello World!"
                )
                .as_bytes();

                loop {
                    // write response bytes
                    match conn.write(&response[*written..]) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly");
                            completed.push(i);
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
                        completed.push(i);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue 'next,
                    Err(e) => panic!("{e}"),

                }
                //conn.flush().unwrap();

                //completed.push(i);
            }

        }

        // remove completed connection
        for i in completed.iter().rev() {
            conn_vec.remove(*i);
        }

        //std::thread::spawn(|| {
        //    if let Err(e) = handle_connection(connection) {
        //        println!("failed to handle connection: {e:?}");
        //    }
        //});
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
