#[cfg(all(not(feature = "tokio-runtime"), feature = "async-std-runtime"))]
pub use async_std_connection::FuseConnection;
#[cfg(all(not(feature = "async-std-runtime"), feature = "tokio-runtime"))]
pub use tokio_connection::FuseConnection;

#[cfg(feature = "tokio-runtime")]
mod tokio_connection {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::IntoRawFd;
    use std::os::unix::io::RawFd;
    use std::path::Path;
    use std::process::Command;

    use futures_util::lock::Mutex;
    use nix::fcntl::{FcntlArg, OFlag};
    use nix::sys::socket;
    use nix::sys::socket::{AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
    use nix::sys::uio::IoVec;
    use nix::unistd;
    use tokio::io::unix::AsyncFd;
    use tokio::task;
    use tracing::debug;

    use crate::helper::io_error_from_nix_error;
    use crate::MountOptions;

    #[derive(Debug)]
    pub struct FuseConnection {
        fd: AsyncFd<RawFd>,
        read: Mutex<()>,
        write: Mutex<()>,
    }

    impl FuseConnection {
        pub async fn new() -> io::Result<Self> {
            const DEV_FUSE: &str = "/dev/fuse";

            let fd = tokio::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .open(DEV_FUSE)
                .await?
                .into_std()
                .await
                .into_raw_fd();

            Self::set_fd_non_blocking(fd)?;

            Ok(Self {
                fd: AsyncFd::new(fd)?,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }

        #[cfg(feature = "unprivileged")]
        pub async fn new_with_unprivileged(
            mount_options: MountOptions,
            mount_path: impl AsRef<Path>,
        ) -> io::Result<Self> {
            let (fd0, fd1) = match socket::socketpair(
                AddressFamily::Unix,
                SockType::SeqPacket,
                None,
                SockFlag::empty(),
            ) {
                Err(err) => return Err(io_error_from_nix_error(err)),

                Ok((fd0, fd1)) => (fd0, fd1),
            };

            let binary_path = match which::which("fusermount3") {
                Err(err) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("find fusermount binary failed {:?}", err),
                    ));
                }
                Ok(path) => path,
            };

            const ENV: &str = "_FUSE_COMMFD";

            let options = mount_options.build_with_unprivileged();

            debug!("mount options {:?}", options);

            let mount_path = mount_path.as_ref().as_os_str().to_os_string();

            let mut child = task::spawn_blocking(move || {
                Command::new(binary_path)
                    .env(ENV, fd0.to_string())
                    .args(vec![OsString::from("-o"), options, mount_path])
                    .spawn()
            })
            .await
            .unwrap()?;

            if !child.wait()?.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "fusermount run failed",
                ));
            }

            let fd = task::spawn_blocking(move || {
                // let mut buf = vec![0; 10000]; // buf should large enough
                let mut buf = vec![]; // it seems 0 len still works well

                let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

                let bufs = [IoVec::from_mut_slice(&mut buf)];

                let msg = match socket::recvmsg(fd1, &bufs, Some(&mut cmsg_buf), MsgFlags::empty())
                {
                    Err(err) => return Err(io_error_from_nix_error(err)),

                    Ok(msg) => msg,
                };

                let fd = if let Some(ControlMessageOwned::ScmRights(fds)) = msg.cmsgs().next() {
                    if fds.is_empty() {
                        return Err(io::Error::new(io::ErrorKind::Other, "no fuse fd"));
                    }

                    fds[0]
                } else {
                    return Err(io::Error::new(io::ErrorKind::Other, "get fuse fd failed"));
                };

                Ok(fd)
            })
            .await
            .unwrap()?;

            if let Err(err) = unistd::close(fd0) {
                return Err(io_error_from_nix_error(err));
            }

            if let Err(err) = unistd::close(fd1) {
                return Err(io_error_from_nix_error(err));
            }

            Self::set_fd_non_blocking(fd)?;

            Ok(Self {
                fd: AsyncFd::new(fd)?,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }

        pub fn set_fd_non_blocking(fd: RawFd) -> io::Result<()> {
            let flags =
                nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(io_error_from_nix_error)?;

            let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;

            nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io_error_from_nix_error)?;

            Ok(())
        }

        pub async fn read(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
            let _guard = self.read.lock().await;

            loop {
                let mut read_guard = self.fd.readable().await?;
                if let Ok(result) = read_guard
                    .try_io(|fd| unistd::read(fd.as_raw_fd(), buf).map_err(io_error_from_nix_error))
                {
                    return result;
                } else {
                    continue;
                }
            }
        }

        pub async fn write(&self, buf: &[u8]) -> Result<usize, io::Error> {
            let _guard = self.write.lock().await;

            loop {
                let mut write_guard = self.fd.writable().await?;
                if let Ok(result) = write_guard.try_io(|fd| {
                    unistd::write(fd.as_raw_fd(), buf).map_err(io_error_from_nix_error)
                }) {
                    return result;
                } else {
                    continue;
                }
            }
        }
    }

    impl AsRawFd for FuseConnection {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    impl Drop for FuseConnection {
        fn drop(&mut self) {
            let _ = unistd::close(self.as_raw_fd());
        }
    }
}

#[cfg(feature = "async-std-runtime")]
mod async_std_connection {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::IntoRawFd;
    use std::os::unix::io::RawFd;
    use std::path::Path;
    use std::process::Command;

    use async_io::Async;
    use async_std::{fs, task};
    use futures_util::lock::Mutex;
    use nix::sys::socket;
    use nix::sys::socket::{AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
    use nix::sys::uio::IoVec;
    use nix::unistd;
    use tracing::debug;

    use crate::helper::io_error_from_nix_error;
    use crate::MountOptions;

    #[derive(Debug)]
    pub struct FuseConnection {
        fd: Async<RawFd>,
        read: Mutex<()>,
        write: Mutex<()>,
    }

    impl FuseConnection {
        pub async fn new() -> io::Result<Self> {
            const DEV_FUSE: &str = "/dev/fuse";

            let fd = fs::OpenOptions::new()
                .write(true)
                .read(true)
                .open(DEV_FUSE)
                .await?
                .into_raw_fd();

            Ok(Self {
                fd: Async::new(fd)?,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }

        #[cfg(feature = "unprivileged")]
        pub async fn new_with_unprivileged(
            mount_options: MountOptions,
            mount_path: impl AsRef<Path>,
        ) -> io::Result<Self> {
            let (fd0, fd1) = match socket::socketpair(
                AddressFamily::Unix,
                SockType::SeqPacket,
                None,
                SockFlag::empty(),
            ) {
                Err(err) => return Err(io_error_from_nix_error(err)),

                Ok((fd0, fd1)) => (fd0, fd1),
            };

            let binary_path = match which::which("fusermount3") {
                Err(err) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("find fusermount binary failed {:?}", err),
                    ));
                }
                Ok(path) => path,
            };

            const ENV: &str = "_FUSE_COMMFD";

            let options = mount_options.build_with_unprivileged();

            debug!("mount options {:?}", options);

            let mount_path = mount_path.as_ref().as_os_str().to_os_string();

            let mut child = task::spawn_blocking(move || {
                Command::new(binary_path)
                    .env(ENV, fd0.to_string())
                    .args(vec![OsString::from("-o"), options, mount_path])
                    .spawn()
            })
            .await?;

            if !child.wait()?.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "fusermount run failed",
                ));
            }

            let fd = task::spawn_blocking(move || {
                // let mut buf = vec![0; 10000]; // buf should large enough
                let mut buf = vec![]; // it seems 0 len still works well

                let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

                let bufs = [IoVec::from_mut_slice(&mut buf)];

                let msg = match socket::recvmsg(fd1, &bufs, Some(&mut cmsg_buf), MsgFlags::empty())
                {
                    Err(err) => return Err(io_error_from_nix_error(err)),

                    Ok(msg) => msg,
                };

                let fd = if let Some(ControlMessageOwned::ScmRights(fds)) = msg.cmsgs().next() {
                    if fds.len() < 1 {
                        return Err(io::Error::new(io::ErrorKind::Other, "no fuse fd"));
                    }

                    fds[0]
                } else {
                    return Err(io::Error::new(io::ErrorKind::Other, "get fuse fd failed"));
                };

                Ok(fd)
            })
            .await?;

            if let Err(err) = unistd::close(fd0) {
                return Err(io_error_from_nix_error(err));
            }

            if let Err(err) = unistd::close(fd1) {
                return Err(io_error_from_nix_error(err));
            }

            Ok(Self {
                fd: Async::new(fd)?,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }

        pub async fn read(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
            let _guard = self.read.lock().await;

            self.fd
                .read_with(|fd| unistd::read(*fd, buf).map_err(io_error_from_nix_error))
                .await
        }

        pub async fn write(&self, buf: &[u8]) -> Result<usize, io::Error> {
            let _guard = self.write.lock().await;

            self.fd
                .write_with(|fd| unistd::write(*fd, buf).map_err(io_error_from_nix_error))
                .await
        }
    }

    impl AsRawFd for FuseConnection {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    impl Drop for FuseConnection {
        fn drop(&mut self) {
            let _ = unistd::close(self.fd.as_raw_fd());
        }
    }
}
