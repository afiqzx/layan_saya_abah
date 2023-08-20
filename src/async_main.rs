use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
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

#[derive(Clone)]
pub struct Waker(Arc<dyn Fn() + Send + Sync>);
impl Waker {
    pub fn wake(&self) {
        (self.0)()
    }
}

pub trait Future {
    type Output;

    fn poll(&mut self, waker: Waker) -> Option<Self::Output>;

    fn chain<F, T>(self, transition: F) -> Chain<Self, F, T>
    where
        F: FnOnce(Self::Output) -> T,
        T: Future,
        Self: Sized,
    {
        Chain::First {
            future1: self,
            transition: Some(transition),
        }
    }
}

pub enum Chain<T1, F, T2> {
    // transtition need to be Option because FnOnce will move
    First { future1: T1, transition: Option<F> },
    Second { future2: T2 },
}
impl<T1, F, T2> Future for Chain<T1, F, T2>
where
    T1: Future,
    F: FnOnce(T1::Output) -> T2,
    T2: Future,
{
    type Output = T2::Output;

    fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
        if let Chain::First {
            future1,
            transition,
        } = self
        {
            // poll the first future
            match future1.poll(waker.clone()) {
                Some(value) => {
                    // firest future done
                    let future2 = (transition.take().unwrap())(value);
                    *self = Chain::Second { future2 };
                }
                None => return None,
            }
        }

        if let Chain::Second { future2 } = self {
            return future2.poll(waker);
        }

        None
    }
}

struct WithData<'data, D, F> {
    data: D,
    future: F,
    _data: PhantomData<&'data D>,
}
impl<'data, D, F> WithData<'data, D, F> 
where
    F: Future + 'data,
{
    pub fn new(data: D, construct: impl Fn(&D) -> F) -> Self {
        let future = construct(&data);
        WithData { data, future, _data: PhantomData }
    }
}

impl<'data, D, F> Future for WithData<'data, D, F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
        self.future.poll(waker)
    }
}

fn main() {
    /// comment out the future you want
    //SCHEDULER.spawn(Main::Start);
    SCHEDULER.spawn(listen());
    SCHEDULER.run();
}

// struct state machine -----------
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
                }
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

        if let HandlerState::Read { request, read } = &mut self.state {
            loop {
                // try reading
                match self.connection.read(&mut request[*read..]) {
                    Ok(0) => {
                        println!("client dissconnected unexpectedly becuase 0 bytes is read");
                        return Some(());
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

// function future --------

pub fn poll_fn<F, T>(f: F) -> impl Future<Output = T>
where
    F: FnMut(Waker) -> Option<T>,
{
    struct FromFn<F>(F);
    impl<F, T> Future for FromFn<F>
    where
        F: FnMut(Waker) -> Option<T>,
    {
        type Output = T;

        fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
            (self.0)(waker)
        }
    }

    FromFn(f)
}

fn listen() -> impl Future<Output = ()> {
    poll_fn(|waker| {
        let listener = TcpListener::bind("localhost:3000").unwrap();
        listener.set_nonblocking(true).unwrap();

        REACTOR.with(|reactor| {
            reactor.add(listener.as_raw_fd(), waker);
        });

        Some(listener)
    })
    .chain(|listener| {
        poll_fn(move |_| match listener.accept() {
            Ok((connection, _)) => {
                connection.set_nonblocking(true).unwrap();

                SCHEDULER.spawn(handle(connection));

                None
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return None;
            }
            Err(e) => panic!("{e}"),
        })
    })
}

fn handle(connection: TcpStream) -> impl Future<Output = ()> {
    let connection = Arc::new(Mutex::new(connection));
    let start_connection = connection.clone();
    let read_connection = connection.clone();
    let write_connection = connection.clone();
    let flush_connection = connection.clone();

    poll_fn(move |waker| {
        REACTOR.with(|reactor| {
            reactor.add(start_connection.lock().unwrap().as_raw_fd(), waker);
        });

        Some(())
    })
    .chain(move |_| {
        let mut read = 0;
        let mut request = [0u8; 1024];

        poll_fn(move |_| {
            loop {
                // try read from stream
                match read_connection.lock().unwrap().read(&mut request[read..]) {
                    Ok(0) => {
                        println!("client disconnected unexpected");
                        return Some(());
                    }
                    Ok(n) => read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                let read = read;
                if read >= 4 && &request[read - 4..read] == b"\r\n\r\n" {
                    break;
                }
            }

            // done, print request
            let request = String::from_utf8_lossy(&request[..read]);
            println!("{request}");

            Some(())
        })
    })
    .chain(move |_| {
        // Hello world
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Length:12\n",
            "Connection: close\r\n\r\n",
            "Hello World!"
        )
        .as_bytes();

        let mut written = 0;

        poll_fn(move |_| {
            loop {
                // write response bytes
                match write_connection.lock().unwrap().write(&response[written..]) {
                    Ok(0) => {
                        println!("client disself.connection.cted unexpectedly");
                        return Some(());
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                if written == response.len() {
                    break;
                }
            }

            Some(())
        })
    })
    .chain(move |_| {
        poll_fn(move |_| {
            match flush_connection.lock().unwrap().flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e) => panic!("{e}"),
            }

            REACTOR.with(|reactor| {
                reactor.remove(flush_connection.lock().unwrap().as_raw_fd());
            });

            Some(())
        })
    })
}
