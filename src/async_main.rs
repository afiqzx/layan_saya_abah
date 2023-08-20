use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{RawFd, AsRawFd};
use std::sync::{Arc, Mutex};

use epoll::{ControlOptions::*, Event, Events};

thread_local! {
    pub static REACTOR: Reactor = Reactor::new();
}

pub static SCHEDULER: Scheduler = Scheduler {
    runnable: Mutex::new(VecDeque::new()),
};

pub struct Reactor {
    pub epoll: RawFd,
    pub tasks: RefCell<HashMap<RawFd, Waker>>,
}
impl Reactor {
    /// Creates a new reactor
    pub fn new() -> Reactor {
        Reactor {
            epoll: epoll::create(false).unwrap(),
            tasks: RefCell::new(HashMap::new()),
        }
    }

    /// Add a file descriptor with read and write interest
    /// `waker` will be called when the descriptor becomes ready
    pub fn add(&self, fd: RawFd, waker: Waker) {
        let event = epoll::Event::new(Events::EPOLLIN | Events::EPOLLOUT, fd as u64);
        epoll::ctl(self.epoll, EPOLL_CTL_ADD, fd, event).unwrap();
        self.tasks.borrow_mut().insert(fd, waker);
    }

    /// Remove the file descriptor from epoll
    /// It will no longer receive any notifiations
    pub fn remove(&self, fd: RawFd) {
        self.tasks.borrow_mut().remove(&fd);
    }

    /// Drive tasks (state machine?) forward, or blocking until an event arrive
    pub fn wait(&self) {
        let mut events = [Event::new(Events::empty(), 0); 1024];
        let timeout = -1; // block forever
        let event_count = epoll::wait(self.epoll, timeout, &mut events).unwrap();

        for event in &events[..event_count] {
            let fd = event.data as i32;

            // wake the task
            if let Some(waker) = self.tasks.borrow().get(&fd) {
                waker.wake();
            }
        }
    }
}

type SharedTask = Arc<Mutex<dyn Future<Output = ()> + Send>>;

#[derive(Default)]
pub struct Scheduler {
    pub runnable: Mutex<VecDeque<SharedTask>>,
}
impl Scheduler {
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.runnable
            .lock()
            .unwrap()
            .push_back(Arc::new(Mutex::new(task)));
    }

    pub fn run(&self) {
        loop {
            loop {
                // pop a runnable task
                let Some(task) = self.runnable.lock().unwrap().pop_front() else { break; };

                let t2 = task.clone();

                let wake = Arc::new(move || {
                    SCHEDULER.runnable.lock().unwrap().push_back(t2.clone());
                });

                // poll it
                task.try_lock().unwrap().poll(Waker(wake));
            }

            // if there are no runnable tasks, block on epoll
            REACTOR.with(|reactor| reactor.wait());
        }
    }
}

pub struct Waker(Arc<dyn Fn() + Send + Sync>);
impl Waker {
    pub fn wake(&self) {
        (self.0)()
    }
}

pub trait Future {
    type Output;

    fn poll(&mut self, waker: Waker) -> Option<Self::Output>;
}

fn main() {
    SCHEDULER.spawn(Main::Start);
    SCHEDULER.run();
}

enum Main {
    Start,
    Accept { listener: TcpListener },
}

impl Future for Main {
    type Output = ();

    fn poll(&mut self, waker: Waker) -> Option<()> {
        if let Main::Start = self {
            let listener = TcpListener::bind("localhost:3000").unwrap();
            listener.set_nonblocking(true).unwrap();

            REACTOR.with(|reactor| {
                reactor.add(listener.as_raw_fd(), waker);
            });

            *self = Main::Accept { listener };
        }

        if let Main::Accept { listener } = self {
            match listener.accept() {
                Ok((connection, _)) => {
                    connection.set_nonblocking(true).unwrap();
                    
                    SCHEDULER.spawn(Handler {
                        connection,
                        state: HandlerState::Start,
                    });

                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return None;
                },
                Err(e) => panic!("{e}"),
            }
        }

        None
    }
}

struct Handler {
    connection: TcpStream,
    state: HandlerState,
}

impl Future for Handler {
    type Output = ();

    fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
        if let HandlerState::Start = self.state {
            // register connection
            REACTOR.with(|reactor| {
                reactor.add(self.connection.as_raw_fd(), waker);
            });
            
            self.state = HandlerState::Read {
                request: [0u8; 1024],
                read: 0,
            };
            
        }

        if let HandlerState::Read { request, read  } = &mut self.state {
            loop {
                // try reading
                match self.connection.read(&mut request[*read..]) {
                    Ok(0) => {
                        println!("client dissconnected unexpectedly becuase 0 bytes is read");
                        return Some(())
                    }
                    Ok(n) => *read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
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

            self.state = HandlerState::Write {
                response,
                written: 0,
            };
        }

        if let HandlerState::Write { response, written } = &mut self.state {
            loop {
                // write response bytes
                match self.connection.write(&response[*written..]) {
                    Ok(0) => {
                        println!("client disself.connection.cted unexpectedly");
                        return Some(());
                    }
                    Ok(n) => *written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                if *written == response.len() {
                    break;
                }
            }

            self.state = HandlerState::Flush;
        }

        if let HandlerState::Flush = self.state {
            match self.connection.flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e) => panic!("{e}"),
            }
        }

        REACTOR.with(|reactor| {
            reactor.remove(self.connection.as_raw_fd());
        });

        Some(())
    }
}

enum HandlerState {
    Start,
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

