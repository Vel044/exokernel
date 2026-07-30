use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
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
const DEFAULT_RESPONSE_LATENCY_MS: u64 = 16;

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
    // 正常运行默认不逐帧打印，避免终端I/O改变半双工请求/应答时序。
    // 排查桥接问题时显式设置SCSERVO_BRIDGE_TRACE=1。
    let trace = env::var_os("SCSERVO_BRIDGE_TRACE").is_some();
    // QEMU的usb-serial字符后端没有向本桥暴露Guest提交Bulk IN的时刻。
    // 完整系统测试中USB线程还会被推理和控制线程抢占，因此给Guest留出
    // 足够时间完成Bulk OUT、重新获得Quantum并提交下一笔Bulk IN。
    let response_latency_ms = env::var("SCSERVO_BRIDGE_LATENCY_MS")
        .ok()
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                invalid("SCSERVO_BRIDGE_LATENCY_MS must be an unsigned integer")
            })
        })
        .transpose()?
        .unwrap_or(DEFAULT_RESPONSE_LATENCY_MS);

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
    println!(
        "Serial bridge ready: {socket} <-> {port} at {BAUDRATE} 8N1, response latency {response_latency_ms} ms"
    );
    let (stream, _) = listener.accept()?;
    println!("QEMU connected to serial bridge");

    let mut qemu_input = stream.try_clone()?;
    let mut serial_output = serial.try_clone()?;
    let writer = thread::spawn(move || {
        let mut request = [0u8; 512];
        loop {
            let length = qemu_input.read(&mut request)?;
            if length == 0 {
                return Ok::<(), io::Error>(());
            }
            if trace {
                print_frame("QEMU -> SCServo", &request[..length]);
            }
            serial_output.write_all(&request[..length])?;
            serial_output.flush()?;
        }
    });

    let mut serial_input = serial;
    let mut qemu_output = stream;
    let mut response = [0u8; 512];
    let read_result = loop {
        match std::io::Read::read(&mut serial_input, &mut response) {
            Ok(0) => break Ok(0),
            Ok(length) => {
                if trace {
                    print_frame("SCServo -> QEMU", &response[..length]);
                }
                // SCServo通常在Guest的Bulk OUT完成后立即返回状态包。此时
                // Guest还没来得及向QEMU FTDI提交Bulk IN，字符后端若立刻
                // 推送响应，可能在没有挂起IN请求时丢掉这批字节。真实FTDI
                // 也会通过latency timer聚合短包。这里的默认值比真实FTDI更保守，
                // 用于覆盖Guest同时运行推理、控制和USB线程的最坏调度延迟。
                thread::sleep(Duration::from_millis(response_latency_ms));
                if let Err(error) = qemu_output.write_all(&response[..length]) {
                    break Err(error);
                }
                if let Err(error) = qemu_output.flush() {
                    break Err(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(error),
        }
    };
    let write_result = writer
        .join()
        .map_err(|_| io::Error::other("serial bridge writer thread panicked"))?;
    let _ = std::fs::remove_file(socket_path);
    read_result?;
    write_result?;
    Ok(())
}

/// 输出一帧的方向、长度和十六进制内容。桥只观察字节，不解析或修改协议；
/// 因此该日志可以确认故障发生在物理串口前还是QEMU/xHCI返回路径中。
fn print_frame(direction: &str, frame: &[u8]) {
    print!("[bridge] {direction} ({} bytes):", frame.len());
    for byte in frame {
        print!(" {byte:02x}");
    }
    println!();
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
