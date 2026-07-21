use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    os::unix::{fs::OpenOptionsExt, net::UnixListener},
    path::Path,
    process::ExitCode,
    thread,
    time::Duration,
};

const DEFAULT_PORT: &str = "/dev/cu.usbmodem5A7C1191771";
const DEFAULT_SOCKET: &str = "/tmp/exokernel-scservo-bridge.sock";
const BAUDRATE: libc::speed_t = 1_000_000;
const IOSSIOSPEED: libc::c_ulong = 0x8004_5402;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scservo-serial-bridge: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let mut port = DEFAULT_PORT.to_owned();
    let mut socket = DEFAULT_SOCKET.to_owned();
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--port" => {
                port = args.next().ok_or_else(|| invalid("--port requires a path"))?;
            }
            "--socket" => {
                socket = args
                    .next()
                    .ok_or_else(|| invalid("--socket requires a path"))?;
            }
            _ => return Err(invalid("usage: scservo-serial-bridge [--port PATH] [--socket PATH]")),
        }
    }

    let serial = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(&port)?;
    configure_serial(&serial)?;

    let socket_path = Path::new(&socket);
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    println!("Serial bridge ready: {socket} <-> {port} at {BAUDRATE} 8N1");
    let (stream, _) = listener.accept()?;
    println!("QEMU connected to serial bridge");

    let mut qemu_input = stream.try_clone()?;
    let mut serial_output = serial.try_clone()?;
    let writer = thread::spawn(move || {
        let result = io::copy(&mut qemu_input, &mut serial_output);
        let _ = serial_output.flush();
        result
    });

    let mut serial_input = serial;
    let mut qemu_output = stream;
    let read_result = io::copy(&mut serial_input, &mut qemu_output);
    let write_result = writer
        .join()
        .map_err(|_| io::Error::other("serial bridge writer thread panicked"))?;
    let _ = std::fs::remove_file(socket_path);
    read_result?;
    write_result?;
    Ok(())
}

fn configure_serial(serial: &File) -> io::Result<()> {
    let fd = serial.as_raw_fd();
    let mut termios = unsafe {
        let mut value = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut value) != 0 {
            return Err(io::Error::last_os_error());
        }
        value
    };

    unsafe { libc::cfmakeraw(&mut termios) };
    termios.c_cflag |= libc::CLOCAL | libc::CREAD;
    termios.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE);
    termios.c_cflag |= libc::CS8;
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;

    unsafe {
        libc::cfsetispeed(&mut termios, libc::B38400);
        libc::cfsetospeed(&mut termios, libc::B38400);
        if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
            return Err(io::Error::last_os_error());
        }
        let speed = BAUDRATE;
        if libc::ioctl(fd, IOSSIOSPEED, &speed) != 0 {
            return Err(io::Error::last_os_error());
        }
        let modem_bits = libc::TIOCM_DTR | libc::TIOCM_RTS;
        if libc::ioctl(fd, libc::TIOCMBIS, &modem_bits) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
