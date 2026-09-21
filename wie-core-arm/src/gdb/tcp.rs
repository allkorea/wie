extern crate std;

use alloc::{format, sync::Arc};
use std::{
    io,
    net::{Shutdown, TcpListener, TcpStream},
    println,
    thread::{self, JoinHandle},
    time::Duration,
};

use gdbstub::{
    stub::{DisconnectReason, GdbStub},
    target::ext::base::multithread::MultiThreadResume,
};
use spin::Mutex;

use crate::{ArmCore, engine::DebugInner};

use super::{GdbBlockingEventLoop, GdbTarget};

pub(crate) struct GdbServer {
    debug: Arc<DebugInner>,
    connection: Arc<Mutex<Option<TcpStream>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

pub(crate) fn start(core: ArmCore) -> wie_util::Result<GdbServer> {
    TcpListener::bind("127.0.0.1:2159")
        .and_then(|listener| GdbServer::new(core, listener))
        .map_err(|err| wie_util::WieError::FatalError(format!("Failed to start GDB server: {err}")))
}

impl GdbServer {
    fn new(core: ArmCore, listener: TcpListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let target = GdbTarget::new(core);
        let debug = target.debug.clone();
        let connection = Arc::new(Mutex::new(None));
        let server_connection = connection.clone();
        let thread = thread::Builder::new().spawn(move || {
            if let Err(err) = target.run_gdb_server(listener, server_connection) {
                tracing::error!("GDB server error: {err}");
            }
        })?;
        Ok(Self {
            debug,
            connection,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(crate) fn shutdown(&self) {
        // The target has no server handle, so joining cannot reenter this lock.
        // Serialize callers until the server has actually released its target.
        let mut handle = self.thread.lock();
        self.debug.shutdown();
        let connection = self.connection.lock().take();
        if let Some(connection) = connection
            && let Err(error) = connection.shutdown(Shutdown::Both)
            && error.kind() != io::ErrorKind::NotConnected
        {
            tracing::warn!("Failed to close GDB connection: {error}");
        }
        if let Some(thread) = handle.take() {
            thread.thread().unpark();
            if thread.join().is_err() {
                tracing::error!("GDB server thread panicked during shutdown");
            }
        }
    }
}

impl Drop for GdbServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl GdbTarget {
    fn run_gdb_server(mut self, sock: TcpListener, connection: Arc<Mutex<Option<TcpStream>>>) -> io::Result<()> {
        println!("GDB server listening on {}", sock.local_addr()?);

        while !self.debug.is_stopped() {
            let (stream, addr) = match sock.accept() {
                Ok(client) => client,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::park_timeout(Duration::from_millis(50));
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            {
                let mut active = connection.lock();
                if self.debug.is_stopped() {
                    break;
                }
                *active = Some(stream.try_clone()?);
            }

            println!("GDB client attached from {addr}");

            let result = self.run_session(stream);
            let closed = connection.lock().take();
            drop(closed);
            match result {
                Ok(DisconnectReason::Disconnect) => {
                    println!("GDB client requested detach");
                    println!("GDB client detached");
                }
                Ok(DisconnectReason::TargetExited(code)) => {
                    println!("GDB session ended: target exited with code {code}");
                    return Ok(());
                }
                Ok(DisconnectReason::TargetTerminated(sig)) => {
                    println!("GDB session ended: target terminated with signal {sig:?}");
                    return Ok(());
                }
                Ok(DisconnectReason::Kill) => {
                    println!("GDB session ended: kill requested");
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!("GDB session ended: {err}");
                }
            }
            println!("GDB server waiting for next client");
        }
        Ok(())
    }

    fn run_session(&mut self, stream: TcpStream) -> io::Result<DisconnectReason> {
        self.debug.pause();
        if self.debug.is_stopped() {
            return Ok(DisconnectReason::TargetExited(0));
        }
        self.clear_resume_actions().map_err(io::Error::other)?;
        let result = GdbStub::new(stream).run_blocking::<GdbBlockingEventLoop<TcpStream>>(self);
        if self.debug.is_stopped() {
            return Ok(DisconnectReason::TargetExited(0));
        }
        self.debug
            .detach()
            .map_err(|err| io::Error::other(format!("Failed to detach GDB: {err}")))?;
        result.map_err(|err| io::Error::other(format!("{err}")))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, string::String, vec::Vec};
    use core::{
        pin::Pin,
        task::{Context, Poll, Waker},
        time::Duration,
    };
    use std::io::{Read, Write};

    use crossbeam::channel;
    use gdbstub::{common::Tid, target::ext::base::multithread::MultiThreadSingleStep};

    use crate::{Allocator, engine::DebuggedArm32CpuEngine};

    use super::*;

    fn send_packet(stream: &mut TcpStream, payload: &str) {
        let checksum = payload.bytes().fold(0u8, u8::wrapping_add);
        write!(stream, "${payload}#{checksum:02x}").unwrap();
    }

    fn read_packet(stream: &mut TcpStream) -> String {
        let mut byte = [0];
        loop {
            Read::read_exact(stream, &mut byte).unwrap();
            if byte[0] == b'$' {
                break;
            }
        }
        let mut payload = Vec::new();
        loop {
            Read::read_exact(stream, &mut byte).unwrap();
            if byte[0] == b'#' {
                break;
            }
            payload.push(byte[0]);
        }
        let mut checksum = [0; 2];
        Read::read_exact(stream, &mut checksum).unwrap();
        assert_eq!(
            u8::from_str_radix(core::str::from_utf8(&checksum).unwrap(), 16).unwrap(),
            payload.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        );
        Write::write_all(stream, b"+").unwrap();
        let mut decoded = Vec::new();
        let mut bytes = payload.into_iter();
        while let Some(byte) = bytes.next() {
            if byte == b'*' {
                let count = bytes.next().unwrap() - 29;
                decoded.extend(core::iter::repeat_n(*decoded.last().unwrap(), count as usize));
            } else {
                decoded.push(byte);
            }
        }
        String::from_utf8(decoded).unwrap()
    }

    #[test]
    fn remote_sessions_read_threads_step_interrupt_and_reattach() {
        let mut core = ArmCore::new(false, None).unwrap();
        let engine = DebuggedArm32CpuEngine::new();
        let debug = engine.debug_inner();
        core.inner.lock().engine = Box::new(engine);
        Allocator::init(&mut core).unwrap();
        core.load(&[0x01, 0x30, 0xfd, 0xe7], 0x1000, 4).unwrap(); // add r0, #1; b 0x1000
        let _parked = core.run_in_thread(|| async { Ok(()) }).unwrap();
        let mut context = core.read_thread_context(1).unwrap();
        context.r0 = 42;
        core.write_thread_context(1, &context);

        let mut running_core = core.clone();
        let task = core
            .run_in_thread(move || async move {
                running_core.run_function::<()>(0x1001, &[0]).await?;
                Ok(())
            })
            .unwrap();
        let runner = thread::spawn(move || futures::executor::block_on(task));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut target = GdbTarget {
            core,
            debug: debug.clone(),
            step_threads: Vec::new(),
            resumed_threads: Vec::new(),
            scheduler_locked: false,
        };
        target.set_resume_action_step(Tid::new(2).unwrap(), None).unwrap();
        target.set_resume_action_continue(Tid::new(1).unwrap(), None).unwrap();
        assert_eq!(target.step_threads, [2]);
        let server = thread::spawn(move || {
            for session in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let result = target.run_session(stream);
                if session == 1 {
                    assert!(result.is_err());
                } else {
                    assert!(matches!(result.unwrap(), DisconnectReason::Disconnect));
                }
            }
        });

        for session in 0..3 {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            send_packet(&mut stream, "qSupported:multiprocess+;swbreak+");
            assert!(read_packet(&mut stream).contains("multiprocess+"));
            send_packet(&mut stream, "?");
            assert!(read_packet(&mut stream).contains("thread:p01.02;"));
            send_packet(&mut stream, "qfThreadInfo");
            assert_eq!(read_packet(&mut stream), "mp01.02,p01.01");

            if session == 0 {
                send_packet(&mut stream, "Hgp1.1");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "g");
                let regs = read_packet(&mut stream);
                assert_eq!(&regs[..8], "2a000000");
                send_packet(&mut stream, "Hgp1.99");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "g");
                assert!(read_packet(&mut stream).starts_with('E'));
                send_packet(&mut stream, "Hgp1.2");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "Z0,1002,2");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "vCont;c");
                assert!(read_packet(&mut stream).contains("swbreak:;"));
                send_packet(&mut stream, "vCont;s:p1.2");
                let stopped = read_packet(&mut stream);
                assert!(stopped.starts_with("T05thread:p01.02;"));
                assert!(!stopped.contains("swbreak"));
                send_packet(&mut stream, "g");
                let regs = read_packet(&mut stream);
                assert_eq!(&regs[..8], "01000000");
                assert_eq!(&regs[15 * 8..16 * 8], "00100000");
            } else if session == 1 {
                send_packet(&mut stream, "Z0,1002,2");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "vCont;c");
                drop(stream);
                thread::sleep(Duration::from_millis(10));
                continue;
            } else {
                assert!(!debug.has_breakpoints());
                assert!(debug.read_registers().r0 > 1);
                send_packet(&mut stream, "!");
                assert_eq!(read_packet(&mut stream), "OK");
                send_packet(&mut stream, "vAttach;1");
                assert!(read_packet(&mut stream).contains("thread:p01.02;"));
                send_packet(&mut stream, "vCont;c");
                Write::write_all(&mut stream, &[3]).unwrap();
                assert!(read_packet(&mut stream).starts_with("T02thread:p01.02;"));
                send_packet(&mut stream, "M1000,4:70477047"); // bx lr at either PC in the loop
                assert_eq!(read_packet(&mut stream), "OK");
            }
            send_packet(&mut stream, "D;1");
            assert_eq!(read_packet(&mut stream), "OK");
            drop(stream);
            if session == 0 {
                thread::sleep(Duration::from_millis(10));
            }
        }
        server.join().unwrap();
        runner.join().unwrap().unwrap();
    }

    #[test]
    fn remote_step_actions_remain_bound_to_their_threads() {
        let mut core = ArmCore::new(false, None).unwrap();
        let engine = DebuggedArm32CpuEngine::new();
        let debug = engine.debug_inner();
        core.inner.lock().engine = Box::new(engine);
        Allocator::init(&mut core).unwrap();
        let mut tasks = Vec::new();
        for address in [0x1000, 0x2000] {
            core.load(&[0x01, 0x30, 0xfd, 0xe7], address, 4).unwrap(); // add r0, #1; loop
            let mut running_core = core.clone();
            tasks.push(
                core.run_in_thread(move || async move { running_core.run_function::<()>(address | 1, &[0]).await })
                    .unwrap(),
            );
        }
        let runner = thread::spawn(move || {
            let mut cx = Context::from_waker(Waker::noop());
            while !tasks.is_empty() {
                tasks.retain_mut(|task| match Pin::new(task).poll(&mut cx) {
                    Poll::Pending => true,
                    Poll::Ready(result) => {
                        result.unwrap();
                        false
                    }
                });
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut target = GdbTarget {
                core,
                debug,
                step_threads: Vec::new(),
                resumed_threads: Vec::new(),
                scheduler_locked: false,
            };
            let (stream, _) = listener.accept().unwrap();
            assert!(matches!(target.run_session(stream).unwrap(), DisconnectReason::Disconnect));
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        send_packet(&mut stream, "qSupported:multiprocess+;swbreak+");
        read_packet(&mut stream);
        send_packet(&mut stream, "Z0,1000,2");
        assert_eq!(read_packet(&mut stream), "OK");
        send_packet(&mut stream, "vCont;c");
        assert!(read_packet(&mut stream).contains("swbreak:;"));
        send_packet(&mut stream, "vCont;s:p1.2");
        assert_eq!(read_packet(&mut stream), "T05thread:p01.02;");
        send_packet(&mut stream, "vCont;s:p1.1");
        let stepped_breakpoint = read_packet(&mut stream);

        send_packet(&mut stream, "z0,1000,2");
        assert_eq!(read_packet(&mut stream), "OK");
        send_packet(&mut stream, "vCont;s:p1.1");
        read_packet(&mut stream);
        send_packet(&mut stream, "vCont;s:p1.1;s:p1.2");
        let stepped_threads = read_packet(&mut stream);
        send_packet(&mut stream, "Hgp1.1");
        assert_eq!(read_packet(&mut stream), "OK");
        send_packet(&mut stream, "g");
        let registers = read_packet(&mut stream);

        for command in ["M1000,4:70477047", "M2000,4:70477047", "D;1"] {
            send_packet(&mut stream, command);
            assert_eq!(read_packet(&mut stream), "OK");
        }
        server.join().unwrap();
        runner.join().unwrap();
        assert_eq!(stepped_breakpoint, "T05thread:p01.01;");
        assert_eq!(stepped_threads, "T05thread:p01.01;");
        assert_eq!(&registers[..8], "02000000");
    }

    #[test]
    fn shutdown_releases_idle_initial_paused_and_running_sessions() {
        for mode in 0..4 {
            let mut core = ArmCore::new(false, None).unwrap();
            let engine = DebuggedArm32CpuEngine::new();
            let debug = engine.debug_inner();
            core.inner.lock().engine = Box::new(engine);
            Allocator::init(&mut core).unwrap();
            let weak_core = Arc::downgrade(&core.inner);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = GdbServer::new(core.clone(), listener).unwrap();
            let (task_tx, task_rx) = channel::bounded(1);
            let runner = if mode >= 2 {
                core.load(&[0x01, 0x30, 0xfd, 0xe7], 0x1000, 4).unwrap();
                let mut running = core.clone();
                let task = core.run_in_thread(move || async move { running.run_function::<()>(0x1001, &[0]).await }).unwrap();
                Some(thread::spawn(move || task_tx.send(futures::executor::block_on(task)).unwrap()))
            } else {
                None
            };
            let mut client = if mode > 0 { Some(TcpStream::connect(address).unwrap()) } else { None };
            if let Some(stream) = client.as_mut() {
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while server.connection.lock().is_none() {
                    assert!(std::time::Instant::now() < deadline, "GDB did not accept the client");
                    thread::yield_now();
                }
                if mode >= 2 {
                    send_packet(stream, "qSupported:multiprocess+;swbreak+");
                    read_packet(stream);
                    if mode == 2 {
                        // Leave the packet reader waiting for the remaining bytes.
                        Write::write_all(stream, b"$g").unwrap();
                    } else {
                        send_packet(stream, "vCont;c");
                    }
                }
            }
            let (done_tx, done_rx) = channel::bounded(1);
            let stopper = thread::spawn(move || {
                server.shutdown();
                server.shutdown();
                done_tx.send(()).unwrap();
                server
            });
            done_rx.recv_timeout(Duration::from_secs(2)).expect("GDB shutdown blocked");
            let server = stopper.join().unwrap();
            if let Some(runner) = runner {
                assert!(task_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
                runner.join().unwrap();
            }
            debug.resume(Vec::new(), None);
            debug.interrupt();
            assert!(debug.is_stopped());
            drop(client);
            core.shutdown();
            drop(server);
            drop(core);
            assert!(weak_core.upgrade().is_none());
            if mode == 0 {
                TcpListener::bind(address).unwrap();
            }
        }
    }
}
