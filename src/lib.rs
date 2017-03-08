extern crate kernel32;
extern crate winapi;

use kernel32::*;
use winapi::*;
use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ffi::OsString;

pub struct PipeStream {
    server_half: bool,
    handle: Handle,
}

impl PipeStream {
    fn create_pipe(path: &Path) -> io::Result<HANDLE> {
        let mut os_str: OsString = path.as_os_str().into();
        os_str.push("\x00");
        let u16_slice = os_str.encode_wide().collect::<Vec<u16>>();

        let _ = unsafe { WaitNamedPipeW(u16_slice.as_ptr(), 0) };
        let handle = unsafe {
            CreateFileW(u16_slice.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        std::ptr::null_mut(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        std::ptr::null_mut())
        };

        if handle != INVALID_HANDLE_VALUE {
            Ok(handle)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<PipeStream> {
        let handle = PipeStream::create_pipe(path.as_ref())?;

        Ok(PipeStream {
            handle: Handle { inner: handle },
            server_half: false,
        })
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        let _ = unsafe { FlushFileBuffers(self.handle.inner) };
        if self.server_half {
            let _ = unsafe { DisconnectNamedPipe(self.handle.inner) };
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut bytes_read = 0;
        let ok = unsafe {
            ReadFile(self.handle.inner,
                     buf.as_mut_ptr() as LPVOID,
                     buf.len() as DWORD,
                     &mut bytes_read,
                     std::ptr::null_mut())
        };

        if ok != 0 {
            Ok(bytes_read as usize)
        } else {
            match io::Error::last_os_error().raw_os_error().map(|x| x as u32) {
                Some(ERROR_PIPE_NOT_CONNECTED) => Ok(0),
                Some(err) => Err(io::Error::from_raw_os_error(err as i32)),
                _ => panic!(""),
            }
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut bytes_written = 0;
        let ok = unsafe {
            WriteFile(self.handle.inner,
                      buf.as_ptr() as LPCVOID,
                      buf.len() as DWORD,
                      &mut bytes_written,
                      std::ptr::null_mut())
        };

        if ok != 0 {
            Ok(bytes_written as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let ok = unsafe { FlushFileBuffers(self.handle.inner) };

        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub struct PipeListener<'a> {
    path: Cow<'a, Path>,
    next_pipe: Handle,
}

impl<'a> PipeListener<'a> {
    fn create_pipe(path: &Path, first: bool) -> io::Result<Handle> {
        let mut os_str: OsString = path.as_os_str().into();
        os_str.push("\x00");
        let u16_slice = os_str.encode_wide().collect::<Vec<u16>>();

        let mut access_flags = PIPE_ACCESS_DUPLEX;
        if first {
            access_flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let handle = unsafe {
            CreateNamedPipeW(u16_slice.as_ptr(),
                             access_flags,
                             PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                             PIPE_UNLIMITED_INSTANCES,
                             65536,
                             65536,
                             50,
                             std::ptr::null_mut())
        };

        if handle != INVALID_HANDLE_VALUE {
            Ok(Handle { inner: handle })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn connect_pipe(handle: &Handle) -> io::Result<()> {
        let result = unsafe { ConnectNamedPipe(handle.inner, std::ptr::null_mut()) };

        if result != 0 {
            Ok(())
        } else {
            match io::Error::last_os_error().raw_os_error().map(|x| x as u32) {
                Some(ERROR_PIPE_CONNECTED) => Ok(()),
                Some(err) => Err(io::Error::from_raw_os_error(err as i32)),
                _ => panic!(""),
            }
        }
    }

    pub fn bind<P: Into<Cow<'a, Path>>>(path: P) -> io::Result<Self> {
        let path = path.into();
        let handle = PipeListener::create_pipe(&path, true)?;
        Ok(PipeListener {
            path: path,
            next_pipe: handle,
        })
    }

    pub fn accept(&mut self) -> io::Result<PipeStream> {
        let handle = std::mem::replace(&mut self.next_pipe,
                                       PipeListener::create_pipe(&self.path, false)?);

        PipeListener::connect_pipe(&handle)?;

        Ok(PipeStream {
            handle: handle,
            server_half: true,
        })
    }

    pub fn incoming<'b>(&'b mut self) -> Incoming<'b, 'a> {
        Incoming { listener: self }
    }
}

pub struct Incoming<'a, 'b>
    where 'b: 'a
{
    listener: &'a mut PipeListener<'b>,
}

impl<'a, 'b> IntoIterator for &'a mut PipeListener<'b> {
    type Item = io::Result<PipeStream>;
    type IntoIter = Incoming<'a, 'b>;

    fn into_iter(self) -> Incoming<'a, 'b> {
        self.incoming()
    }
}

impl<'a, 'b> Iterator for Incoming<'a, 'b> {
    type Item = io::Result<PipeStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

#[cfg(test)]
mod test {
    use std::thread;
    use super::*;

    macro_rules! or_panic {
        ($e:expr) => {
            match $e {
                Ok(e) => e,
                Err(e) => {
                    panic!("{}", e);
                },
            }
        }
    }

    #[test]
    fn basic() {
        let socket_path = Path::new("//./pipe/basicsock");
        println!("{:?}", socket_path);
        let msg1 = b"hello";
        let msg2 = b"world!";

        let mut listener = or_panic!(PipeListener::bind(socket_path));
        let thread = thread::spawn(move || {
            let mut stream = or_panic!(listener.accept());
            let mut buf = [0; 5];
            or_panic!(stream.read(&mut buf));
            assert_eq!(&msg1[..], &buf[..]);
            or_panic!(stream.write_all(msg2));
        });

        let mut stream = or_panic!(PipeStream::connect(socket_path));

        or_panic!(stream.write_all(msg1));
        let mut buf = vec![];
        or_panic!(stream.read_to_end(&mut buf));
        assert_eq!(&msg2[..], &buf[..]);
        drop(stream);

        thread.join().unwrap();
    }

    #[test]
    fn iter() {
        let socket_path = Path::new("//./pipe/itersock");

        let mut listener = or_panic!(PipeListener::bind(socket_path));
        let thread = thread::spawn(move || for stream in listener.incoming().take(2) {
            let mut stream = or_panic!(stream);
            let mut buf = [0];
            or_panic!(stream.read(&mut buf));
        });

        for _ in 0..2 {
            let mut stream = or_panic!(PipeStream::connect(socket_path));
            or_panic!(stream.write_all(&[0]));
        }

        thread.join().unwrap();
    }
}

struct Handle {
    inner: HANDLE,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.inner) };
    }
}

unsafe impl Sync for Handle {}
unsafe impl Send for Handle {}