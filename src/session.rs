//! Filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use std::io;
use std::ffi::OsStr;
use std::fmt;
use std::path::{PathBuf, Path};
use thread_scoped::{scoped, JoinGuard};
use libc::{EAGAIN, EINTR, ENODEV, ENOENT};
use log::{error, info};

use crate::channel::{self, Channel};
use crate::request::Request;
use crate::Filesystem;

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. The FUSE default is 16M on macOS
/// and 128k on other systems.
pub const MAX_WRITE_SIZE: usize = 16 * 1024 * 1024;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to MAX_WRITE_SIZE bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: usize = MAX_WRITE_SIZE + 4096;

/// The session data structure
#[derive(Debug)]
pub struct Session<FS: Filesystem> {
    /// Filesystem operation implementations
    pub filesystem: FS,
    /// Communication channel to the kernel driver
    ch: Channel,
    /// FUSE protocol major version
    pub proto_major: u32,
    /// FUSE protocol minor version
    pub proto_minor: u32,
    /// True if the filesystem is initialized (init operation done)
    pub initialized: bool,
    /// True if the filesystem was destroyed (destroy operation done)
    pub destroyed: bool,
}

enum RecvResult<'a> {
    // A request has been readed
    Some(Request<'a>),
    // No request available but safe to retry
    Retry,
    // Filesystem has been unmounted or there is an error, next call to receive should return an error
    Drop(Option<io::Error>),
}

impl<FS: Filesystem> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    pub fn new(filesystem: FS, mountpoint: &Path, options: &[&OsStr]) -> io::Result<Session<FS>> {
        info!("Mounting {}", mountpoint.display());
        Channel::new(mountpoint, options).map(|ch| {
            Session {
                filesystem: filesystem,
                ch: ch,
                proto_major: 0,
                proto_minor: 0,
                initialized: false,
                destroyed: false,
            }
        })
    }

    /// Return path of the mounted filesystem
    pub fn mountpoint(&self) -> &Path {
        &self.ch.mountpoint()
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. This read-dispatch-loop is non-concurrent to prevent
    /// having multiple buffers (which take up much memory), but the filesystem methods
    /// may run concurrent by spawning threads.
    pub fn run(&mut self) -> io::Result<()> {
        // Buffer for receiving requests from the kernel. Only one is allocated and
        // it is reused immediately after dispatching to conserve memory and allocations.
        let mut buffer: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
        loop {
            // Read the next request from the given channel to kernel driver
            // The kernel driver makes sure that we get exactly one request per read
            match self.receive(&mut buffer) {
                RecvResult::Some(request) => request.dispatch(self),
                RecvResult::Retry => continue,
                RecvResult::Drop(None) => return Ok(()),
                RecvResult::Drop(Some(err)) => return Err(err),
            }
        }
        Ok(())
    }

    ///
    /// Read a single request from the fuse channel
    /// this can be non blocking if `ll::channel::set_nonblocking` is set on the fuse channel
    /// 
    #[inline]
    fn receive<'a>(&mut self, buffer: &'a mut Vec<u8>) -> RecvResult<'a> {
        match self.ch.receive(buffer) {
            Ok(_) => match Request::new(self.ch.sender(), buffer) {
                // Return request
                Some(request) => RecvResult::Some(request),
                // Should drop on illegal request
                None => RecvResult::Drop(None),
            },
            Err(err) => match err.raw_os_error() {
                // The operation was interupted by the kernel, the user or fuse explicitly request a retry
                Some(ENOENT) | Some(EINTR) | Some(EAGAIN) => RecvResult::Retry,
                // Filesystem was unmounted without error
                Some(ENODEV) => RecvResult::Drop(None),
                // Return last os error
                _ => RecvResult::Drop(Some(err)),
            }
        }
    }
}

impl<'a, FS: Filesystem + Send + 'a> Session<FS> {
    /// Run the session loop in a background thread
    pub unsafe fn spawn(self) -> io::Result<BackgroundSession<'a>> {
        BackgroundSession::new(self)
    }
}

impl<FS: Filesystem> Drop for Session<FS> {
    fn drop(&mut self) {
        info!("Unmounted {}", self.mountpoint().display());
    }
}

/// The background session data structure
pub struct BackgroundSession<'a> {
    /// Path of the mounted filesystem
    pub mountpoint: PathBuf,
    /// Thread guard of the background session
    pub guard: JoinGuard<'a, io::Result<()>>,
}

impl<'a> BackgroundSession<'a> {
    /// Create a new background session for the given session by running its
    /// session loop in a background thread. If the returned handle is dropped,
    /// the filesystem is unmounted and the given session ends.
    pub unsafe fn new<FS: Filesystem + Send + 'a>(se: Session<FS>) -> io::Result<BackgroundSession<'a>> {
        let mountpoint = se.mountpoint().to_path_buf();
        let guard = scoped(move || {
            let mut se = se;
            se.run()
        });
        Ok(BackgroundSession { mountpoint: mountpoint, guard: guard })
    }
}

impl<'a> Drop for BackgroundSession<'a> {
    fn drop(&mut self) {
        info!("Unmounting {}", self.mountpoint.display());
        // Unmounting the filesystem will eventually end the session loop,
        // drop the session and hence end the background thread.
        match channel::unmount(&self.mountpoint) {
            Ok(()) => (),
            Err(err) => error!("Failed to unmount {}: {}", self.mountpoint.display(), err),
        }
    }
}

// replace with #[derive(Debug)] if Debug ever gets implemented for
// thread_scoped::JoinGuard
impl<'a> fmt::Debug for BackgroundSession<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "BackgroundSession {{ mountpoint: {:?}, guard: JoinGuard<()> }}", self.mountpoint)
    }
}

use mio::{Evented, Poll, Token, Ready, PollOpt};
use mio::unix::EventedFd;
///
/// A FuseEvented provides a way to use the FUSE filesystem in a custom event
/// loop. It implements the mio Evented trait, so it can be polled for
/// readiness.
///
// TODO: Drop
#[derive(Debug)]
pub struct EventedSession<FS: Filesystem>(Session<FS>);

impl<FS: Filesystem>  Evented for EventedSession<FS> {
    fn register(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        let raw_fd = unsafe {self.0.ch.raw_fd() };
        EventedFd(&raw_fd).register(poll, token, interest, opts)
    }
    fn reregister(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        let raw_fd = unsafe {self.0.ch.raw_fd() };
        EventedFd(&raw_fd).reregister(poll, token, interest, opts)
    }
    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        let raw_fd = unsafe {self.0.ch.raw_fd() };
        EventedFd(&raw_fd).deregister(poll)
    }
}

impl<FS: Filesystem> EventedSession<FS> {
    pub fn handle_one_req(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
       unimplemented!()
       // self.0.handle_one_req(buf)
    }
}
 