use rustix::fd::AsFd;
use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags
};

#[cfg(target_vendor = "apple")]
use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};

use std::fs::File;
use std::io::IoSlice;
use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;

pub fn write_fd<Fd: AsFd>(socket: Fd, file: impl AsFd) -> io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut buf = SendAncillaryBuffer::new(&mut space);
    let fd_arr = [file.as_fd()];
    buf.push(SendAncillaryMessage::ScmRights(&fd_arr));
    sendmsg(socket, &[IoSlice::new(&[0])], &mut buf, SendFlags::empty())?;
    Ok(())
}

pub fn read_fd<Fd: AsFd>(socket: Fd) -> io::Result<File> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut buf = RecvAncillaryBuffer::new(&mut space);
    let mut recv_buf = [0];
    #[cfg(not(target_vendor = "apple"))]
    recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut recv_buf)],
        &mut buf,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    #[cfg(target_vendor = "apple")]
    recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut recv_buf)],
        &mut buf,
        RecvFlags::empty()
    )?;
    if let Some(RecvAncillaryMessage::ScmRights(mut fd)) = buf.drain().next() {
        if let Some(fd) = fd.next() {
#[cfg(target_vendor = "apple")]
                let current_flags = fcntl_getfd(&fd)?;
#[cfg(target_vendor = "apple")]
                fcntl_setfd(&fd, current_flags | FdFlags::CLOEXEC)?;

            return Ok(fd.into());
        }
    }
    Err(io::Error::new(io::ErrorKind::Other, "did not receive fd"))
}
