use std::cmp::min;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, format_err, Result};
use clap::{App, AppSettings, Arg};
use mio::net::{TcpListener, TcpStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};

use proxmox_io::ByteBuffer;
use proxmox_lang::error::io_err_other;
use proxmox_sys::{
    linux::pty::{make_controlling_terminal, PTY},
};

const MSG_TYPE_DATA: u8 = 0;
const MSG_TYPE_RESIZE: u8 = 1;
//const MSG_TYPE_PING: u8 = 2;

fn remove_number(buf: &mut ByteBuffer) -> Option<usize> {
    loop {
        if let Some(pos) = &buf.iter().position(|&x| x == b':') {
            let data = buf.remove_data(*pos);
            buf.consume(1); // the ':'
            let len = match std::str::from_utf8(&data) {
                Ok(lenstring) => match lenstring.parse() {
                    Ok(len) => len,
                    Err(err) => {
                        eprintln!("error parsing number: '{}'", err);
                        break;
                    }
                },
                Err(err) => {
                    eprintln!("error parsing number: '{}'", err);
                    break;
                }
            };
            return Some(len);
        } else if buf.len() > 20 {
            buf.consume(20);
        } else {
            break;
        }
    }
    None
}

fn process_queue(buf: &mut ByteBuffer, pty: &mut PTY) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }

    loop {
        if buf.len() < 2 {
            break;
        }

        let msgtype = buf[0] - b'0';

        if msgtype == MSG_TYPE_DATA {
            buf.consume(2);
            if let Some(len) = remove_number(buf) {
                return Some(len);
            }
        } else if msgtype == MSG_TYPE_RESIZE {
            buf.consume(2);
            if let Some(cols) = remove_number(buf) {
                if let Some(rows) = remove_number(buf) {
                    pty.set_size(cols as u16, rows as u16).ok()?;
                }
            }
        // ignore incomplete messages
        } else {
            buf.consume(1);
            // ignore invalid or ping (msgtype 2)
        }
    }

    None
}

type TicketResult = Result<(Box<[u8]>, Box<[u8]>)>;

/// Reads from the stream and returns the first line and the rest
fn read_ticket_line(
    stream: &mut TcpStream,
    buf: &mut ByteBuffer,
    timeout: Duration,
) -> TicketResult {
    let mut poll = Poll::new()?;
    poll.registry()
        .register(stream, Token(0), Interest::READABLE)?;
    let mut events = Events::with_capacity(1);

    let now = Instant::now();
    let mut elapsed = Duration::new(0, 0);

    loop {
        poll.poll(&mut events, Some(timeout - elapsed))?;
        if !events.is_empty() {
            match buf.read_from(stream) {
                Ok(n) => {
                    if n == 0 {
                        bail!("connection closed before authentication");
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => return Err(err.into()),
            }

            if buf[..].contains(&b'\n') {
                break;
            }

            if buf.is_full() {
                bail!("authentication data is incomplete: {:?}", &buf[..]);
            }
        }

        elapsed = now.elapsed();
        if elapsed > timeout {
            bail!("timed out");
        }
    }

    let newline_idx = &buf[..].iter().position(|&x| x == b'\n').unwrap();

    let line = buf.remove_data(*newline_idx);
    buf.consume(1); // discard newline

    match line.iter().position(|&b| b == b':') {
        Some(pos) => {
            let (username, ticket) = line.split_at(pos);
            Ok((username.into(), ticket[1..].into()))
        }
        None => bail!("authentication data is invalid"),
    }
}

fn authenticate(
    username: &[u8],
    ticket: &[u8],
    path: &str,
    perm: Option<&str>,
    authport: u16,
    port: Option<u16>,
) -> Result<()> {
    let mut post_fields: Vec<(&str, &str)> = Vec::with_capacity(5);
    post_fields.push(("username", std::str::from_utf8(username)?));
    post_fields.push(("password", std::str::from_utf8(ticket)?));
    post_fields.push(("path", path));
    if let Some(perm) = perm {
        post_fields.push(("privs", perm));
    }
    let port_str;
    if let Some(port) = port {
        port_str = port.to_string();
        post_fields.push(("port", &port_str));
    }

    let url = format!("http://localhost:{}/api2/json/access/ticket", authport);

    match ureq::post(&url).send_form(&post_fields[..]) {
        Ok(res) if res.status() == 200 => Ok(()),
        Ok(res) | Err(ureq::Error::Status(_, res)) => {
            let code = res.status();
            bail!("invalid authentication - {} {}", code, res.status_text())
        }
        Err(err) => bail!("authentication request failed - {}", err),
    }
}

fn listen_and_accept(
    hostname: &str,
    port: u64,
    port_as_fd: bool,
    timeout: Duration,
) -> Result<(TcpStream, u16)> {
    let listener = if port_as_fd {
        unsafe { std::net::TcpListener::from_raw_fd(port as i32) }
    } else {
        std::net::TcpListener::bind((hostname, port as u16))?
    };
    let port = listener.local_addr()?.port();
    let mut listener = TcpListener::from_std(listener);
    let mut poll = Poll::new()?;

    poll.registry()
        .register(&mut listener, Token(0), Interest::READABLE)?;

    let mut events = Events::with_capacity(1);

    let now = Instant::now();
    let mut elapsed = Duration::new(0, 0);

    loop {
        poll.poll(&mut events, Some(timeout - elapsed))?;
        if !events.is_empty() {
            let (stream, client) = listener.accept()?;
            println!("client connection: {:?}", client);
            return Ok((stream, port));
        }

        elapsed = now.elapsed();
        if elapsed > timeout {
            bail!("timed out");
        }
    }
}

fn run_pty(cmd: &OsStr, params: clap::OsValues) -> Result<PTY> {
    let (mut pty, secondary_name) = PTY::new().map_err(io_err_other)?;

    let mut filtered_env: HashMap<OsString, OsString> = std::env::vars_os()
        .filter(|&(ref k, _)| {
            k == "PATH"
                || k == "USER"
                || k == "HOME"
                || k == "LANG"
                || k == "LANGUAGE"
                || k.to_string_lossy().starts_with("LC_")
        })
        .collect();
    filtered_env.insert("TERM".into(), "xterm-256color".into());

    let mut command = Command::new(cmd);

    command.args(params).env_clear().envs(&filtered_env);

    unsafe {
        command.pre_exec(move || {
            make_controlling_terminal(&secondary_name).map_err(io_err_other)?;
            Ok(())
        });
    }

    command.spawn()?;

    pty.set_size(80, 20)?;
    Ok(pty)
}

const TCP: Token = Token(0);
const PTY: Token = Token(1);

fn do_main() -> Result<()> {
    let matches = App::new("termproxy")
        .setting(AppSettings::TrailingVarArg)
        .arg(Arg::with_name("port").takes_value(true).required(true))
        .arg(
            Arg::with_name("authport")
                .takes_value(true)
                .long("authport"),
        )
        .arg(Arg::with_name("use-port-as-fd").long("port-as-fd"))
        .arg(
            Arg::with_name("path")
                .takes_value(true)
                .long("path")
                .required(true),
        )
        .arg(Arg::with_name("perm").takes_value(true).long("perm"))
        .arg(Arg::with_name("cmd").multiple(true).required(true))
        .get_matches();

    let port: u64 = matches
        .value_of("port")
        .unwrap()
        .parse()
        .map_err(io_err_other)?;
    let path = matches.value_of("path").unwrap();
    let perm: Option<&str> = matches.value_of("perm");
    let mut cmdparams = matches.values_of_os("cmd").unwrap();
    let cmd = cmdparams.next().unwrap();
    let authport: u16 = matches
        .value_of("authport")
        .unwrap_or("85")
        .parse()
        .map_err(io_err_other)?;
    let mut pty_buf = ByteBuffer::new();
    let mut tcp_buf = ByteBuffer::new();

    let use_port_as_fd = matches.is_present("use-port-as-fd");

    if use_port_as_fd && port > u16::MAX as u64 {
        return Err(format_err!("port too big"));
    } else if port > i32::MAX as u64 {
        return Err(format_err!("Invalid FD number"));
    }

    let (mut tcp_handle, port) =
        listen_and_accept("localhost", port, use_port_as_fd, Duration::new(10, 0))
            .map_err(|err| format_err!("failed waiting for client: {}", err))?;

    let (username, ticket) = read_ticket_line(&mut tcp_handle, &mut pty_buf, Duration::new(10, 0))
        .map_err(|err| format_err!("failed reading ticket: {}", err))?;
    let port = if use_port_as_fd { Some(port) } else { None };
    authenticate(&username, &ticket, path, perm, authport, port)?;
    tcp_handle.write_all(b"OK").expect("error writing response");

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);

    let mut pty = run_pty(cmd, cmdparams)?;

    poll.registry().register(
        &mut tcp_handle,
        TCP,
        Interest::READABLE | Interest::WRITABLE,
    )?;
    poll.registry().register(
        &mut SourceFd(&pty.as_raw_fd()),
        PTY,
        Interest::READABLE | Interest::WRITABLE,
    )?;

    let mut tcp_writable = true;
    let mut pty_writable = true;
    let mut tcp_readable = true;
    let mut pty_readable = true;
    let mut remaining = 0;
    let mut finished = false;

    while !finished {
        if tcp_readable && !pty_buf.is_full() || pty_readable && !tcp_buf.is_full() {
            poll.poll(&mut events, Some(Duration::new(0, 0)))?;
        } else {
            poll.poll(&mut events, None)?;
        }

        for event in &events {
            let writable = event.is_writable();
            let readable = event.is_readable();
            if event.is_read_closed() {
                finished = true;
            }
            match event.token() {
                TCP => {
                    if readable {
                        tcp_readable = true;
                    }
                    if writable {
                        tcp_writable = true;
                    }
                }
                PTY => {
                    if readable {
                        pty_readable = true;
                    }
                    if writable {
                        pty_writable = true;
                    }
                }
                _ => unreachable!(),
            }
        }

        while tcp_readable && !pty_buf.is_full() {
            let bytes = match pty_buf.read_from(&mut tcp_handle) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    tcp_readable = false;
                    break;
                }
                Err(err) => {
                    if !finished {
                        return Err(format_err!("error reading from tcp: {}", err));
                    }
                    break;
                }
            };
            if bytes == 0 {
                finished = true;
                break;
            }
        }

        while pty_readable && !tcp_buf.is_full() {
            let bytes = match tcp_buf.read_from(&mut pty) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    pty_readable = false;
                    break;
                }
                Err(err) => {
                    if !finished {
                        return Err(format_err!("error reading from pty: {}", err));
                    }
                    break;
                }
            };
            if bytes == 0 {
                finished = true;
                break;
            }
        }

        while !tcp_buf.is_empty() && tcp_writable {
            let bytes = match tcp_handle.write(&tcp_buf[..]) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    tcp_writable = false;
                    break;
                }
                Err(err) => {
                    if !finished {
                        return Err(format_err!("error writing to tcp : {}", err));
                    }
                    break;
                }
            };
            tcp_buf.consume(bytes);
        }

        while !pty_buf.is_empty() && pty_writable {
            if remaining == 0 {
                remaining = match process_queue(&mut pty_buf, &mut pty) {
                    Some(val) => val,
                    None => break,
                };
            }
            let len = min(remaining, pty_buf.len());
            let bytes = match pty.write(&pty_buf[..len]) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    pty_writable = false;
                    break;
                }
                Err(err) => {
                    if !finished {
                        return Err(format_err!("error writing to pty : {}", err));
                    }
                    break;
                }
            };
            remaining -= bytes;
            pty_buf.consume(bytes);
        }
    }

    Ok(())
}

fn main() {
    std::process::exit(match do_main() {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("{}", err);
            1
        }
    });
}
