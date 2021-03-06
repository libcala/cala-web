use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::future::Future;
use std::sync::mpsc::Receiver;
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
use std::collections::HashMap;
use std::cell::Cell;
use std::io::{Write, Read, Error, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;

use pasts::{prelude::*};

use smelling_salts::{Device, Watcher};

// Asynchronous message for passing between tasks on this thread.
enum AsyncMsg {
    // Quit the application.
    Quit,
    // Spawn a new task.
    NewTask(Receiver<Message>, WebserverTask),
    // Reduce task count.
    OldTask,
}

type WebserverTask = Box<dyn Future<Output = AsyncMsg> + Send>;

// Wait until one future is completed in a Vec, remove, then return it's result.
async fn slice_select<T>(
    tasks: &mut Vec<Box<dyn Future<Output = T> + Send>>,
) -> T
{
    struct SliceSelect<'a, T> {
        // FIXME: Shouldn't have to be a `Box`?  Probably does.
        tasks: &'a mut Vec<Box<dyn Future<Output = T> + Send>>,
    }

    impl<'a, T> Future for SliceSelect<'a, T> {
        type Output = T;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
            for future_id in 0..self.tasks.len() {
                let mut future = unsafe {
                    Pin::new_unchecked(self.tasks[future_id].as_mut())
                };

                match future.as_mut().poll(cx) {
                    Poll::Ready(ret) => {
                        let _ = self.tasks.remove(future_id);
                        return Poll::Ready(ret);
                    },
                    Poll::Pending => {}
                }
            }

            Poll::Pending
        }
    }

    SliceSelect { tasks }.await
}

// Blocking call for another thread, to be used as a Future
fn async_thread_main_future(recv: Receiver<Message>) -> AsyncMsg {
    match recv.recv().unwrap() {
        Message::NewJob(task) => AsyncMsg::NewTask(recv, task),
        Message::Terminate => AsyncMsg::Quit,
    }
}

// Asynchronous loop for a thread.
async fn async_thread_main(recv: Receiver<Message>, num_tasks: Arc<AtomicUsize>) {
    let mut tasks: Vec<WebserverTask> = vec![];

    tasks.push(Box::new(pasts::spawn(move || async {
        async_thread_main_future(recv)
    })));

    loop {
        match slice_select(&mut tasks).await {
            // Spawn a new task.
            AsyncMsg::NewTask(recv, task) => {
                tasks.push(Box::new(pasts::spawn(move || async {
                    async_thread_main_future(recv)
                })));
                tasks.push(task)
            }
            // Reduce task count.
            AsyncMsg::OldTask => {
                num_tasks.fetch_sub(1, Ordering::Relaxed);
            }
            // Quit the application.
            AsyncMsg::Quit => {
                break
            }
        }
    }
}

// A function that represents one of the 4 threads that can run tasks.
fn thread_main(recv: Receiver<Message>, num_tasks: Arc<AtomicUsize>) {
    pasts::spawn(|| async {
        async_thread_main(recv, num_tasks)
    });
}

/// Handle to one of the threads.
struct Thread {
    // Number of asynchronous tasks on each thread.
    num_tasks: Arc<AtomicUsize>,
    // Join Handle for this thread.
    join: Option<std::thread::JoinHandle<()>>,
    // Message sender to the thread.
    sender: std::sync::mpsc::Sender<Message>,
}

impl Thread {
    /// Create a new thread.
    pub fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let num_tasks = Arc::new(AtomicUsize::new(0));
        let thread_num_tasks = Arc::clone(&num_tasks);
        let join = Some(std::thread::spawn(move || 
            thread_main(receiver, thread_num_tasks)
        ));

        Thread {
            num_tasks, join, sender,
        }
    }

    /// Get the number of tasks on this thread.
    pub fn tasks(&self) -> usize {
        self.num_tasks.load(Ordering::Relaxed)
    }

    /// Send a Future to this thread.
    pub fn send<T>(&self, future: T)
        where T: Future<Output = AsyncMsg> + Send + 'static
    {
        self.num_tasks.fetch_add(1, Ordering::Relaxed);
        self.sender.send(Message::NewJob(Box::new(future))).unwrap();
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        self.sender.send(Message::Terminate).unwrap();
        if let Some(thread) = self.join.take() {
            thread.join().unwrap();
        }
    }
}

type ResourceGenerator = Box<dyn Fn(Stream) -> Box<dyn Future<Output = Result<(), Error>> + Send> + Send + Sync>;

/// A webserver.
pub struct WebServer {
    web: Arc<Web>,
    threads: Vec<Thread>,
    listener: TcpListener,
    device: Device,
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.device.old();
    }
}

impl WebServer {
    /// Create a new Webserver with a path to the static resources.
    pub fn with_resources(path: &'static str) -> Self {
        let urls = HashMap::new();

        let listener = TcpListener::bind("127.0.0.1:8080")
            .unwrap();
        listener.set_nonblocking(true).expect("Failed to set non-blocking");
        let mut threads = vec![];

        for _ in 0..4 {
            threads.push(Thread::new());
        }

        let device = Device::new(listener.as_raw_fd(), Watcher::new().input());

        WebServer { web: Arc::new(Web { path, urls }), threads, listener, device }
    }

    /// Add an async function for a URL.
    pub fn url<F: 'static, G: 'static>(mut self, url: &'static str, func: G)
        -> Self
        where F: Future<Output = Result<(), std::io::Error>> + Send, G: Fn(Stream) -> F + Sync + Send
    {
        Arc::get_mut(&mut self.web).unwrap().urls.insert(url, ("text/html; charset=utf-8", Box::new(
            move |stream| Box::new(func(stream))
        )));
        self
    }

    /// Add an async function for a URL.
    pub fn url_with_type<F: 'static, G: 'static>(
        mut self,
        url: &'static str,
        func: G,
        content_type: &'static str)
        -> Self
        where F: Future<Output = Result<(), std::io::Error>> + Send, G: Fn(Stream) -> F + Sync + Send
    {
        Arc::get_mut(&mut self.web).unwrap().urls.insert(url, (content_type, Box::new(
            move |stream| Box::new(func(stream))
        )));
        self
    }
}

impl Future for WebServer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.listener.accept() {
            Ok(stream) => {
                // Select the thread that is the least busy.
                let mut thread_id = 0;
                let mut thread_tasks = self.threads[0].tasks();
                for id in 1..self.threads.len() {
                    let n_tasks = self.threads[id].tasks();
                    if n_tasks < thread_tasks {
                        thread_id = id;
                        thread_tasks = n_tasks;
                    }
                }

                // Send task to selected thread.
                let stream = stream.0;
                stream.set_nonblocking(true).expect("Couldn't set unblocking!");
                let read_device = Device::new(stream.as_raw_fd(), Watcher::new().input());
                let stream = Arc::new(stream);
                let future = handle_connection(stream, Arc::clone(&self.web), read_device);

                self.threads[thread_id].send(future);

                self.poll(cx)
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                self.device.register_waker(cx.waker());
                Poll::Pending
            }
            Err(e) => {
                panic!("I/O ERROR {}!", e)
            }
        }
    }
}

struct Web {
    path: &'static str,
    urls: HashMap<&'static str, (&'static str, ResourceGenerator)>,
}

struct StreamRead<'a>(&'a mut TcpStream, &'a Device, &'a mut Vec<u8>);

impl Future for StreamRead<'_> {
    type Output = ();
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut buffer = [0; 512];
        loop {
            match this.0.read(&mut buffer) {
                Ok(bytes) if bytes != 0 => {
                    this.2.extend(&buffer[..bytes]);
                    if bytes != 512 {
                        return Poll::Ready(())
                    }
                }
                Err(ref e) if e.kind() != ErrorKind::WouldBlock => {
                    panic!("Stream Read IO Error {}!", e)
                }
                _ => {
                    this.1.register_waker(cx.waker());
                    return Poll::Pending
                }
            }
        }
    }
}

struct StreamWrite<'a>(&'a TcpStream, &'a Device, &'a [u8]);

impl Future for StreamWrite<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        match this.0.write(&mut this.2) {
            Ok(_) => Poll::Ready(()),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                this.1.register_waker(cx.waker());
                Poll::Pending
            }
            Err(e) => panic!("Stream Write IO Error {}!", e),
        }
    }
}

struct StreamFlush<'a>(&'a TcpStream, &'a Device);

impl Future for StreamFlush<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.0.flush() {
            Ok(_) => Poll::Ready(()),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                self.1.register_waker(cx.waker());
                Poll::Pending
            }
            Err(e) => panic!("Stream Write IO Error {}!", e),
        }
    }
}

unsafe impl Sync for Stream {}

/// An HTTP Stream.
pub struct Stream {
    internal: Cell<Option<InternalStream>>
}

impl Stream {
    /// Try to send all data in the stream as HTTP.  May fail if disconnected to
    /// client.
    pub async fn send(&self) -> Result<(), std::io::Error> {
        let mut this = self.internal.take().unwrap();

        let ret = this.send().await;

        self.internal.set(Some(this));

        ret
    }

    /// Push UTF-8 text into the stream.
    pub fn push_str(&self, text: &str) {
        let mut this = self.internal.take().unwrap();

        this.push_str(text);

        self.internal.set(Some(this));
    }

    /// Push bytes into the stream.
    pub fn push_data(&self, bytes: &[u8]) {
        let mut this = self.internal.take().unwrap();

        this.push_data(bytes);

        self.internal.set(Some(this));
    }
}

struct InternalStream {
    stream: Arc<TcpStream>,
    write_device: Device,
    output: Vec<u8>,
}

impl Drop for InternalStream {
    fn drop(&mut self) {
        self.write_device.old();
    }
}

impl InternalStream {
    /// Try to send all data in the stream as HTTP.  May fail if disconnected to
    /// client.
    pub async fn send(&mut self) -> Result<(), std::io::Error> {
        let stream = Arc::get_mut(&mut self.stream).unwrap();

        StreamWrite(stream, &self.write_device, &self.output).await;
        StreamFlush(stream, &self.write_device).await;

        Ok(())
    }

    /// Push UTF-8 text into the stream.
    pub fn push_str(&mut self, text: &str) {
        self.output.extend(text.bytes());
    }

    /// Push bytes into the stream.
    pub fn push_data(&mut self, bytes: &[u8]) {
        self.output.extend(bytes);
    }
}

enum Message {
    NewJob(WebserverTask),
    Terminate,
}

async fn handle_connection(mut streama: Arc<TcpStream>, web: Arc<Web>, mut read_device: Device) -> AsyncMsg {
    // Should be O.k, only one instance of this Arc.
    let stream = Arc::get_mut(&mut streama).unwrap();

    let mut buffer = vec![];

    StreamRead(stream, &read_device, &mut buffer).await;
    read_device.old();

    // Check for GET header.
    if !buffer.starts_with(b"GET ") {
        // Invalid header (Missing GET)
        return AsyncMsg::OldTask;
    }

    // Get the path from the header.
    let mut end = 4;
    let path = loop {
        if end == buffer.len() {
            // Invalid header (Missing HTTP/1.1)
            return AsyncMsg::OldTask;
        }
        if buffer[end] == b' ' {
            break &buffer[4..end];
        }
        end += 1;
    };

    // Check for the end of the header.
    if !buffer[end+1..].starts_with(b"HTTP/1.1\r\n") {
        // Invalid header (Missing HTTP/1.1)
        return AsyncMsg::OldTask;
    }

    let write_device = Device::new(streama.as_raw_fd(), Watcher::new().output());

    let mut streamb = InternalStream { stream: streama, output: vec![], write_device };

    let path = if let Ok(path) = std::str::from_utf8(path) {
        path
    } else {
        // Invalid UTF-8 In path (disconnect).
        return AsyncMsg::OldTask;
    };

    let mut index = web.path.to_string();
    index.push_str("/index.html");

    let mut e404 = web.path.to_string();
    e404.push_str("/404.html");

    // FIXME: Less redundant.
    if "/" == path {
        if let Ok(contents) = std::fs::read_to_string(index) {
            streamb.push_str("HTTP/1.1 200 OK\nContent-Type: ");
            streamb.push_str("text/html; charset=utf-8");
            streamb.push_str("\r\n\r\n");
            streamb.push_str(&contents);
            streamb.send().await.unwrap();
        } else {
            streamb.push_str("HTTP/1.1 404 NOT FOUND\nContent-Type: ");
            streamb.push_str("text/html; charset=utf-8");
            streamb.push_str("\r\n\r\n");
            if let Ok(cs) = std::fs::read_to_string(e404) {
                streamb.push_str(&cs);
            } else {
                streamb.push_str("404 NOT FOUND");
            };
            streamb.send().await.unwrap();
        }
    } else {
        let mut page = web.path.to_string();
        page.push_str(path);

        if let Some(request) = web.urls.get(path) {
            {
                streamb.push_str("HTTP/1.1 200 OK\nContent-Type: ");
                streamb.push_str(request.0);
                streamb.push_str("\r\n\r\n");
            }
            Pin::from(request.1(Stream { internal: Cell::new(Some(streamb)) }))
                .await
                .unwrap();
        } else if let Ok(contents) = std::fs::read_to_string(page) {
            streamb.push_str("HTTP/1.1 200 OK\nContent-Type: ");
            streamb.push_str("text/html; charset=utf-8");
            streamb.push_str("\r\n\r\n");
            streamb.push_str(&contents);
            streamb.send().await.unwrap();
        } else {
            streamb.push_str("HTTP/1.1 404 NOT FOUND\nContent-Type: ");
            streamb.push_str("text/html; charset=utf-8");
            streamb.push_str("\r\n\r\n");
            if let Ok(cs) = std::fs::read_to_string(e404) {
                streamb.push_str(&cs);
            } else {
                streamb.push_str("404 NOT FOUND");
            };
            streamb.send().await.unwrap();
        }
    };

    AsyncMsg::OldTask
}
