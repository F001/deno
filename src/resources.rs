// Copyright 2018 the Deno authors. All rights reserved. MIT license.

// Think of Resources as File Descriptors. They are integers that are allocated
// by the privileged side of Deno to refer to various resources.  The simplest
// example are standard file system files and stdio - but there will be other
// resources added in the future that might not correspond to operating system
// level File Descriptors. To avoid confusion we call them "resources" not "file
// descriptors". This module implements a global resource table. Ops (AKA
// handlers) look up resources by their integer id here.

#[cfg(unix)]
use eager_unix as eager;
use errors::bad_resource;
use errors::DenoError;
use errors::DenoResult;
use http_body::HttpBody;
use repl::Repl;
use tokio_util;
use tokio_write;

use futures;
use futures::future::{Either, FutureResult};
use futures::sync::oneshot;
use futures::Future;
use futures::Poll;
use hyper;
use std;
use std::collections::HashMap;
use std::io::{Error, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use tokio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_io;
use tokio_process;

pub type ResourceId = u32; // Sometimes referred to RID.

// These store Deno's file descriptors. These are not necessarily the operating
// system ones.
struct ResourceTable(Mutex<HashMap<ResourceId, Repr>>);

impl ResourceTable {
  fn run_with<F, R>(&self, rid: ResourceId, f: F) -> R
  where
    F: FnOnce(&mut Repr) -> R,
  {
    let mut table = self.0.lock().expect("ResourceTable is poisoned");
    let mut maybe_repr = table.get_mut(&rid);
    match maybe_repr {
      None => panic!("bad rid {}", rid),
      Some(ref mut repr) => f(repr),
    }
  }

  fn insert(&self, repr: Repr) -> Resource {
    let rid = new_rid();
    debug!("Create new resource {}", rid);
    let mut table = self.0.lock().expect("ResourceTable is poisoned");

    match table.insert(rid, repr) {
      Some(_) => panic!("There is already a file with that rid"),
      None => Resource { rid },
    }
  }
}

struct ResourceTable2 {
  rid_gen: u32,
  table: HashMap<ResourceId, Repr>,
}

impl ResourceTable2 {

  fn insert(&mut self, repr: Repr) -> Resource {
    self.rid_gen = self
      .rid_gen
      .checked_add(1)
      .expect("resource id is exhausted");
    match self.table.insert(self.rid_gen, repr) {
      Some(_) => panic!("There is already a file with that rid"),
      None => Resource { rid: self.rid_gen },
    }
  }
}

enum WorkItem {
  Insert {
    repr: Repr,
    tx: oneshot::Sender<Resource>,
  },
  InsertChild {
    child: tokio_process::Child,
    tx: oneshot::Sender<ChildResources>,
  },
  CollectEntries {
    tx: oneshot::Sender<Vec<(u32, String)>>,
  },
}

pub enum ResourceItem {
  Insert(Resource),
  InsertChild(ChildResources),
  CollectEntries(Vec<(u32, String)>),
}

pub enum ResourceFuture {
  Insert {
    rx: oneshot::Receiver<Resource>,
  },
  InsertChild {
    rx: oneshot::Receiver<ChildResources>,
  },
  CollectEntries {
    rx: oneshot::Receiver<Vec<(u32, String)>>,
  },
}

impl Future for ResourceFuture {
  type Item = ResourceItem;
  type Error = futures::Canceled;

  fn poll(&mut self) -> Poll<ResourceItem, futures::Canceled> {
    match self {
      ResourceFuture::Insert { rx } => {
        rx.poll().map(|r| r.map(|x| ResourceItem::Insert(x)))
      }
      ResourceFuture::InsertChild { rx } => {
        rx.poll().map(|r| r.map(|x| ResourceItem::InsertChild(x)))
      }
      ResourceFuture::CollectEntries { rx } => rx
        .poll()
        .map(|r| r.map(|x| ResourceItem::CollectEntries(x))),
    }
  }
}

static RESOURCE_THREAD_RUNNING: AtomicBool = AtomicBool::new(true);
static mut THE_INSTANCE: Option<&'static Arc<ResourceManager>> = None;
static THE_INIT: std::sync::Once = std::sync::ONCE_INIT;

pub fn instance() -> &'static Arc<ResourceManager> {
  THE_INIT.call_once(|| unsafe {
    let r = Arc::new(ResourceManager::new());
    let p = util::leak(r);
    THE_INSTANCE = Some(p);
  });
  unsafe {
    THE_INSTANCE.expect("The global resource manager has not been initialized.")
  }
}

pub struct ResourceManager {
  tx: std::sync::mpsc::Sender<WorkItem>,
}

impl ResourceManager {
  fn new() -> Self {
    let (tx, rx) = std::sync::mpsc::channel::<WorkItem>();
    thread::spawn(|| {
      let mut res_table: ResourceTable2 = ResourceTable2 {
        rid_gen: 3,
        table: {
          let mut m = HashMap::new();
          m.insert(0, Repr::Stdin(tokio::io::stdin()));
          m.insert(1, Repr::Stdout(tokio::io::stdout()));
          m.insert(2, Repr::Stderr(tokio::io::stderr()));
          m
        },
      };
      let rx = rx;
      while RESOURCE_THREAD_RUNNING.load(Ordering::SeqCst) {
        if let Ok(item) = rx.recv() {
          match item {
            WorkItem::Insert { repr, tx } => {
              tx.send(res_table.insert(repr)).unwrap();
            }
            WorkItem::InsertChild { mut child, tx } => {
              let stdin_rid = child
                .stdin()
                .take()
                .map(|fd| res_table.insert(Repr::ChildStdin(fd)).rid);

              let stdout_rid = child
                .stdout()
                .take()
                .map(|fd| res_table.insert(Repr::ChildStdout(fd)).rid);

              let stderr_rid = child
                .stderr()
                .take()
                .map(|fd| res_table.insert(Repr::ChildStderr(fd)).rid);

              let child_rid = res_table.insert(Repr::Child(child)).rid;

              tx.send(ChildResources {
                child_rid,
                stdin_rid,
                stdout_rid,
                stderr_rid,
              }).unwrap();
            }
            WorkItem::CollectEntries { tx } => {
              let all = res_table
                .table
                .iter()
                .map(|(key, value)| (*key, inspect_repr(&value)))
                .collect();
              tx.send(all).unwrap();
            }
          }
        } else {
          return;
        }
      }
    });
    ResourceManager { tx }
  }

  fn spawn_insert(&self, repr: Repr) -> ResourceFuture {
    let (tx, rx) = oneshot::channel();
    let w = WorkItem::Insert { repr, tx };
    self.tx.send(w).expect("resource manager thread is dead");
    ResourceFuture::Insert { rx }
  }

  pub fn add_fs_file(&self, fs_file: tokio::fs::File) -> ResourceFuture {
    let repr = Repr::FsFile(fs_file);
    self.spawn_insert(repr)
  }

  pub fn add_tcp_listener(
    &self,
    listener: tokio::net::TcpListener,
  ) -> ResourceFuture {
    let repr = Repr::TcpListener(listener);
    self.spawn_insert(repr)
  }

  pub fn add_tcp_stream(
    &self,
    stream: tokio::net::TcpStream,
  ) -> ResourceFuture {
    let repr = Repr::TcpStream(stream);
    self.spawn_insert(repr)
  }

  pub fn add_hyper_body(&self, body: hyper::Body) -> ResourceFuture {
    let body = HttpBody::from(body);
    let repr = Repr::HttpBody(body);
    self.spawn_insert(repr)
  }

  pub fn add_repl(&self, repl: Repl) -> ResourceFuture {
    let repr = Repr::Repl(repl);
    self.spawn_insert(repr)
  }

  pub fn add_child(&self, child: tokio_process::Child) -> ResourceFuture {
    let (tx, rx) = oneshot::channel();
    let w = WorkItem::InsertChild { child, tx };
    self.tx.send(w).expect("resource manager thread is dead");
    ResourceFuture::InsertChild { rx }
  }

  pub fn table_entries(&self) -> ResourceFuture {
    let (tx, rx) = oneshot::channel();
    let w = WorkItem::CollectEntries { tx };
    self.tx.send(w).expect("resource manager thread is dead");
    ResourceFuture::CollectEntries { rx }
  }

  pub fn close(&self) {
    RESOURCE_THREAD_RUNNING.store(false, Ordering::SeqCst);
    unsafe {
      THE_INSTANCE = None;
    }
  }
}

lazy_static! {
  // Starts at 3 because stdio is [0-2].
  static ref NEXT_RID: AtomicUsize = AtomicUsize::new(3);
  static ref RESOURCE_TABLE: ResourceTable = ResourceTable(Mutex::new({
    let mut m = HashMap::new();
    // TODO Load these lazily during lookup?
    m.insert(0, Repr::Stdin(tokio::io::stdin()));
    m.insert(1, Repr::Stdout(tokio::io::stdout()));
    m.insert(2, Repr::Stderr(tokio::io::stderr()));
    m
  }));
}

// Internal representation of Resource.
enum Repr {
  Stdin(tokio::io::Stdin),
  Stdout(tokio::io::Stdout),
  Stderr(tokio::io::Stderr),
  FsFile(tokio::fs::File),
  TcpListener(tokio::net::TcpListener),
  TcpStream(tokio::net::TcpStream),
  HttpBody(HttpBody),
  Repl(Repl),
  Child(tokio_process::Child),
  ChildStdin(tokio_process::ChildStdin),
  ChildStdout(tokio_process::ChildStdout),
  ChildStderr(tokio_process::ChildStderr),
}

pub fn table_entries() -> Vec<(u32, String)> {
  let table = RESOURCE_TABLE.0.lock().unwrap();

  table
    .iter()
    .map(|(key, value)| (*key, inspect_repr(&value)))
    .collect()
}

#[test]
fn test_table_entries() {
  let mut entries = table_entries();
  entries.sort();
  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0], (0, String::from("stdin")));
  assert_eq!(entries[1], (1, String::from("stdout")));
  assert_eq!(entries[2], (2, String::from("stderr")));
}

fn inspect_repr(repr: &Repr) -> String {
  let h_repr = match repr {
    Repr::Stdin(_) => "stdin",
    Repr::Stdout(_) => "stdout",
    Repr::Stderr(_) => "stderr",
    Repr::FsFile(_) => "fsFile",
    Repr::TcpListener(_) => "tcpListener",
    Repr::TcpStream(_) => "tcpStream",
    Repr::HttpBody(_) => "httpBody",
    Repr::Repl(_) => "repl",
    Repr::Child(_) => "child",
    Repr::ChildStdin(_) => "childStdin",
    Repr::ChildStdout(_) => "childStdout",
    Repr::ChildStderr(_) => "childStderr",
  };

  String::from(h_repr)
}

// Abstract async file interface.
// Ideally in unix, if Resource represents an OS rid, it will be the same.
#[derive(Debug)]
pub struct Resource {
  pub rid: ResourceId,
}

impl Resource {
  // TODO Should it return a Resource instead of net::TcpStream?
  pub fn poll_accept(&mut self) -> Poll<(TcpStream, SocketAddr), Error> {
    RESOURCE_TABLE.run_with(self.rid, |repr| match repr {
      Repr::TcpListener(ref mut s) => s.poll_accept(),
      _ => panic!("Cannot accept"),
    })
  }

  // close(2) is done by dropping the value. Therefore we just need to remove
  // the resource from the RESOURCE_TABLE.
  pub fn close(self) {
    debug!("Remove resource {}", self.rid);
    let mut table = RESOURCE_TABLE.0.lock().unwrap();
    let r = table.remove(&self.rid);
    assert!(r.is_some());
  }

  pub fn shutdown(&mut self, how: Shutdown) -> Result<(), DenoError> {
    RESOURCE_TABLE.run_with(self.rid, |repr| match repr {
      Repr::TcpStream(ref mut f) => {
        TcpStream::shutdown(f, how).map_err(DenoError::from)
      }
      _ => panic!("Cannot shutdown"),
    })
  }
}

impl Read for Resource {
  fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
    unimplemented!();
  }
}

impl AsyncRead for Resource {
  fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, Error> {
    RESOURCE_TABLE.run_with(self.rid, |repr| match repr {
      Repr::FsFile(ref mut f) => f.poll_read(buf),
      Repr::Stdin(ref mut f) => f.poll_read(buf),
      Repr::TcpStream(ref mut f) => f.poll_read(buf),
      Repr::HttpBody(ref mut f) => f.poll_read(buf),
      Repr::ChildStdout(ref mut f) => f.poll_read(buf),
      Repr::ChildStderr(ref mut f) => f.poll_read(buf),
      _ => panic!("Cannot read"),
    })
  }
}

impl Write for Resource {
  fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
    unimplemented!()
  }

  fn flush(&mut self) -> std::io::Result<()> {
    unimplemented!()
  }
}

impl AsyncWrite for Resource {
  fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, Error> {
    RESOURCE_TABLE.run_with(self.rid, |repr| match repr {
      Repr::FsFile(ref mut f) => f.poll_write(buf),
      Repr::Stdout(ref mut f) => f.poll_write(buf),
      Repr::Stderr(ref mut f) => f.poll_write(buf),
      Repr::TcpStream(ref mut f) => f.poll_write(buf),
      Repr::ChildStdin(ref mut f) => f.poll_write(buf),
      _ => panic!("Cannot write"),
    })
  }

  fn shutdown(&mut self) -> futures::Poll<(), std::io::Error> {
    unimplemented!()
  }
}

fn new_rid() -> ResourceId {
  let next_rid = NEXT_RID.fetch_add(1, Ordering::SeqCst);
  next_rid as ResourceId
}

pub fn add_fs_file(fs_file: tokio::fs::File) -> Resource {
  let repr = Repr::FsFile(fs_file);
  RESOURCE_TABLE.insert(repr)
}

pub fn add_tcp_listener(listener: tokio::net::TcpListener) -> Resource {
  let repr = Repr::TcpListener(listener);
  RESOURCE_TABLE.insert(repr)
}

pub fn add_tcp_stream(stream: tokio::net::TcpStream) -> Resource {
  let repr = Repr::TcpStream(stream);
  RESOURCE_TABLE.insert(repr)
}

pub fn add_hyper_body(body: hyper::Body) -> Resource {
  let body = HttpBody::from(body);
  let repr = Repr::HttpBody(body);
  RESOURCE_TABLE.insert(repr)
}

pub fn add_repl(repl: Repl) -> Resource {
  let repr = Repr::Repl(repl);
  RESOURCE_TABLE.insert(repr)
}

#[derive(Debug)]
pub struct ChildResources {
  pub child_rid: ResourceId,
  pub stdin_rid: Option<ResourceId>,
  pub stdout_rid: Option<ResourceId>,
  pub stderr_rid: Option<ResourceId>,
}

pub fn add_child(mut c: tokio_process::Child) -> ChildResources {
  let stdin_rid = c
    .stdin()
    .take()
    .map(|fd| RESOURCE_TABLE.insert(Repr::ChildStdin(fd)).rid);

  let stdout_rid = c
    .stdout()
    .take()
    .map(|fd| RESOURCE_TABLE.insert(Repr::ChildStdout(fd)).rid);

  let stderr_rid = c
    .stderr()
    .take()
    .map(|fd| RESOURCE_TABLE.insert(Repr::ChildStderr(fd)).rid);

  let child_rid = RESOURCE_TABLE.insert(Repr::Child(c)).rid;

  return ChildResources {
    child_rid,
    stdin_rid,
    stdout_rid,
    stderr_rid,
  };
}

pub struct ChildStatus {
  rid: ResourceId,
}

// Invert the dumbness that tokio_process causes by making Child itself a future.
impl Future for ChildStatus {
  type Item = ExitStatus;
  type Error = DenoError;

  fn poll(&mut self) -> Poll<ExitStatus, DenoError> {
    RESOURCE_TABLE.run_with(self.rid, |repr| match repr {
      Repr::Child(ref mut child) => child.poll().map_err(DenoError::from),
      _ => Err(bad_resource(self.rid)),
    })
  }
}

pub fn child_status(rid: ResourceId) -> DenoResult<ChildStatus> {
  RESOURCE_TABLE.run_with(rid, |repr| match repr {
    Repr::Child(_) => Ok(ChildStatus { rid }),
    _ => Err(bad_resource(rid)),
  })
}

pub fn readline(rid: ResourceId, prompt: &str) -> DenoResult<String> {
  RESOURCE_TABLE.run_with(rid, |repr| match repr {
    Repr::Repl(ref mut r) => {
      let line = r.readline(&prompt)?;
      Ok(line)
    }
    _ => Err(bad_resource(rid)),
  })
}

pub fn lookup(rid: ResourceId) -> Option<Resource> {
  debug!("resource lookup {}", rid);
  let table = RESOURCE_TABLE.0.lock().unwrap();
  table.get(&rid).map(|_| Resource { rid })
}

pub type EagerRead<R, T> =
  Either<tokio_io::io::Read<R, T>, FutureResult<(R, T, usize), std::io::Error>>;

pub type EagerWrite<R, T> =
  Either<tokio_write::Write<R, T>, FutureResult<(R, T, usize), std::io::Error>>;

pub type EagerAccept = Either<
  tokio_util::Accept,
  FutureResult<(tokio::net::TcpStream, std::net::SocketAddr), std::io::Error>,
>;

#[cfg(not(unix))]
#[allow(unused_mut)]
pub fn eager_read<T: AsMut<[u8]>>(
  resource: Resource,
  mut buf: T,
) -> EagerRead<Resource, T> {
  Either::A(tokio_io::io::read(resource, buf)).into()
}

#[cfg(not(unix))]
pub fn eager_write<T: AsRef<[u8]>>(
  resource: Resource,
  buf: T,
) -> EagerWrite<Resource, T> {
  Either::A(tokio_write::write(resource, buf)).into()
}

#[cfg(not(unix))]
pub fn eager_accept(resource: Resource) -> EagerAccept {
  Either::A(tokio_util::accept(resource)).into()
}

// This is an optimization that Tokio should do.
// Attempt to call read() on the main thread.
#[cfg(unix)]
pub fn eager_read<T: AsMut<[u8]>>(
  resource: Resource,
  buf: T,
) -> EagerRead<Resource, T> {
  RESOURCE_TABLE.run_with(resource.rid, |repr| match repr {
    Repr::TcpStream(ref mut tcp_stream) => {
      eager::tcp_read(tcp_stream, resource, buf)
    }
    _ => Either::A(tokio_io::io::read(resource, buf)),
  })
}

// This is an optimization that Tokio should do.
// Attempt to call write() on the main thread.
#[cfg(unix)]
pub fn eager_write<T: AsRef<[u8]>>(
  resource: Resource,
  buf: T,
) -> EagerWrite<Resource, T> {
  RESOURCE_TABLE.run_with(resource.rid, |repr| match repr {
    Repr::TcpStream(ref mut tcp_stream) => {
      eager::tcp_write(tcp_stream, resource, buf)
    }
    _ => Either::A(tokio_write::write(resource, buf)),
  })
}

#[cfg(unix)]
pub fn eager_accept(resource: Resource) -> EagerAccept {
  RESOURCE_TABLE.run_with(resource.rid, |repr| match repr {
    Repr::TcpListener(ref mut tcp_listener) => {
      eager::tcp_accept(tcp_listener, resource)
    }
    _ => Either::A(tokio_util::accept(resource)),
  })
}

mod util {
  use std::mem;
  // copied from rayon-core project
  pub fn leak<T>(v: T) -> &'static T {
    unsafe {
      let b = Box::new(v);
      let p: *const T = &*b;
      mem::forget(b); // leak our reference, so that `b` is never freed
      &*p
    }
  }
}
