//! The session with the gateway: how `sandboxd` meets it, and how it serves the session once the
//! gateway accepts its hello.
//!
//! A collect and a freeze, which may last, are served on threads of their own where one can
//! start, so that the session reads on meanwhile; any other request is served before the next
//! frame is read.

use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fmt, thread};

use schema::frame;
use schema::ir::GpuModel;
use schema::sandbox::{
    FromSandbox, HELLO_TIMEOUT, Hello, HelloReply, MAX_HELLO_BYTES, RegistrationGrant,
    SANDBOXD_GRANT, ToSandbox, read_hello_reply,
};
use stdx::process::{self, Bound, Echo, Environment, Run, printed};
use stdx::{Context, Deadline, unpoisoned};

use crate::commands::Commands;
use crate::files::{self, Puts};
use crate::model::{Model, Part};
use crate::ops::Ops;
use crate::until::Until;

/// Where `sandboxd` meets its gateway.
pub(crate) enum Gateway {
    /// At a Unix socket of the sandbox's own, which identifies the sandbox.
    Socket(PathBuf),
    /// At a TCP address, where the hello presents the grant that identifies the sandbox.
    Tcp(SocketAddr, RegistrationGrant),
    /// At a TCP address of the sandbox's own, where it listens, and the gateway's answer to the
    /// hello presents the grant that identifies the gateway; until the deadline, past which no
    /// gateway will.
    Listen(SocketAddr, RegistrationGrant, Deadline),
}

// The listener doesn't block, so that the deadline is checked between polls. A twentieth of a
// second is a delay that the gateway's dial, taking a round trip or more, hardly notices.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

const UNCLAIMED: &str = "no gateway claimed the sandbox by its start's deadline, and the slack \
                         after it: expected the gateway to dial while the start lasts, past which \
                         nothing does, so the sandbox ends";

// Any host behind the gateway's address may reach a listening `sandboxd`, as often as it
// likes. Ten is enough to show why the gateway's own dials failed, which come first unless
// another peer races them, and as many as the kernel's rate limit lets a burst of its
// messages print (`printk_ratelimit_burst`).
const TOLD_PEERS_COUNT: usize = 10;

impl fmt::Display for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(socket) => socket.display().fmt(f),
            Self::Tcp(address, _) | Self::Listen(address, _, _) => address.fmt(f),
        }
    }
}

type Reader = Box<dyn Read>;

type Writer = Box<dyn Write + Send>;

// A first query wakes the GPUs' driver, which takes seconds where nothing keeps it awake; past
// that, the start goes on without a fact that it can do without.
const GPUS_TIMEOUT: Duration = Duration::from_secs(10);

/// Meets the gateway and serves its session until the gateway ends it, with a hello that names
/// the sandbox's GPUs. With `serve_ops` it also serves ops, and with `serve_model` the model
/// endpoint, both from before the first request. It fails, with the gateway's reason, if the
/// gateway refuses the session, having started nothing.
pub(crate) fn serve(gateway: &Gateway, serve_ops: bool, serve_model: bool) -> Result<(), String> {
    let (reader, writer) = meet(gateway, gpus())?;
    let sender = Sender(Arc::new(Mutex::new(writer)));
    let ops = Ops::default();
    if serve_ops {
        ops.serve(sender.clone()).context("serving ops failed")?;
    }
    let model = Model::default();
    if serve_model {
        let served = model.serve(sender.clone());
        served.context("serving the model endpoint failed")?;
    }
    let commands =
        Commands::start_reaping(sender.clone(), serve_ops).context("starting the reaper failed")?;
    let mut session = Session {
        sender,
        commands,
        ops,
        model,
        puts: Puts::default(),
    };
    let served = session.serve(&mut BufReader::new(reader));
    // The op in flight, the callers waiting their turns and the model requests in flight end as
    // `sandboxd` exits, which closes their connections.
    session.commands.kill_all();
    served.context("the session with the gateway failed")
}

/// The sandbox's GPUs, each by its model, as `nvidia-smi` names them within `GPUS_TIMEOUT`;
/// none where it isn't on `PATH`, fails, or names none.
fn gpus() -> Option<Vec<GpuModel>> {
    let mut nvidia_smi = process::command("nvidia-smi", Environment::Inherited);
    nvidia_smi
        .args(["--query-gpu=name", "--format=csv,noheader"])
        // It runs as a command would, which has no grant.
        .env_remove(SANDBOXD_GRANT);
    let how = Run::new(Bound::Within(GPUS_TIMEOUT), Echo::Silent);
    let names = printed(process::run(&mut nvidia_smi, how).ok()?).ok()?;
    let gpus = names
        .lines()
        .map(|name| GpuModel::try_from(name.trim().to_owned()).ok());
    gpus.collect::<Option<Vec<_>>>()
        .filter(|gpus| !gpus.is_empty())
}

/// The hello that presents `grant` and names `gpus`, unless their names would make it longer
/// than `MAX_HELLO_BYTES`, which the gateway would refuse.
fn hello(grant: Option<RegistrationGrant>, gpus: Option<Vec<GpuModel>>) -> Hello {
    let named = Hello { grant, gpus };
    let fits = serde_json::to_vec(&named).is_ok_and(|json| json.len() <= MAX_HELLO_BYTES);
    if fits {
        return named;
    }
    Hello {
        gpus: None,
        ..named
    }
}

/// Listens at `address` until a peer's answer to `hello` presents `grant`, and returns that
/// peer's connection's two ends, the session open; or fails once `by` has passed. Each other
/// peer is told nothing more, and its connection closed, with why on standard error.
fn listen(
    address: &SocketAddr,
    grant: &RegistrationGrant,
    by: Deadline,
    hello: &Hello,
) -> Result<(Reader, Writer), String> {
    let listener = TcpListener::bind(address).context(format_args!(
        "listening for the gateway at {address} failed"
    ))?;
    (listener.set_nonblocking(true)).context("making the listener wait in turns failed")?;
    let mut told = 0;
    loop {
        // A peer that the kernel resets before it is taken costs one turn, as any other refusal.
        let (stream, peer) = match listener.accept() {
            Ok(taken) => taken,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if by.remaining().is_none() {
                    return Err(UNCLAIMED.to_owned());
                }
                thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(error) => {
                return Err(error).context(format_args!("taking a connection at {address} failed"));
            }
        };
        stream
            .set_nonblocking(false)
            .context("making the connection block failed")?;
        match greet(&stream, grant, by, hello) {
            // The listener closes as it drops, so no other peer reaches the session's sandbox.
            Ok(()) => return halves(stream, TcpStream::try_clone),
            Err(error) => {
                told += 1;
                if told <= TOLD_PEERS_COUNT {
                    let line = format_args!("sandboxd: turned away the peer at {peer}: {error}");
                    crate::print_line(io::stderr(), line);
                }
                if told == TOLD_PEERS_COUNT {
                    let line = format_args!(
                        "sandboxd: telling of no more peers it turns away, having told of \
                         {TOLD_PEERS_COUNT}"
                    );
                    crate::print_line(io::stderr(), line);
                }
            }
        }
    }
}

/// Opens the session on `stream` with `hello`, as `open` does, all within `HELLO_TIMEOUT` and
/// by `by`, with a peer whose answer presents `grant`; then lifts that bound.
fn greet(
    stream: &TcpStream,
    grant: &RegistrationGrant,
    by: Deadline,
    hello: &Hello,
) -> Result<(), String> {
    stdx::net::set_options(stream).context("setting the connection's options failed")?;
    // No later than `by`, past which no gateway claims the sandbox.
    let left = by.remaining().unwrap_or_default();
    let mut peer = Until::new(stream, Deadline::after(left.min(HELLO_TIMEOUT)));
    open(&mut peer, hello, Some(grant))?;
    let lifted = stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None));
    lifted.context("lifting the bound on the connection's reads and writes failed")
}

fn meet(gateway: &Gateway, gpus: Option<Vec<GpuModel>>) -> Result<(Reader, Writer), String> {
    let connecting = format_args!("connecting to the gateway at {gateway} failed");
    let opening = format_args!("opening the session with the gateway at {gateway} failed");
    match gateway {
        Gateway::Listen(address, grant, by) => listen(address, grant, *by, &hello(None, gpus)),
        Gateway::Socket(socket) => {
            let stream = UnixStream::connect(socket).context(connecting)?;
            open(&mut &stream, &hello(None, gpus), None).context(opening)?;
            halves(stream, UnixStream::try_clone)
        }
        Gateway::Tcp(address, grant) => {
            // One dial, as at a socket: the gateway listens before the sandbox starts, so a retry
            // would meet no gateway that this dial missed, and the kernel's own retries of the
            // handshake ride out lost packets. An exit then ends the sandbox's start before the
            // start's deadline, with the reason, which the sandbox's console keeps.
            let stream = TcpStream::connect(address).context(connecting)?;
            stdx::net::set_options(&stream)
                .context("setting the options of the connection to the gateway failed")?;
            let hello = hello(Some(grant.clone()), gpus);
            open(&mut &stream, &hello, None).context(opening)?;
            halves(stream, TcpStream::try_clone)
        }
    }
}

/// Says `hello` to `peer`, and returns once its answer accepts the session, presenting
/// `expected`; or why not, saying what `it`, the peer, did.
fn open(
    peer: &mut (impl Read + Write),
    hello: &Hello,
    expected: Option<&RegistrationGrant>,
) -> Result<(), String> {
    let late = |error: io::Error| match error.kind() {
        io::ErrorKind::TimedOut => format!(
            "it didn't take the hello and answer it within {} s, or by the sandbox's deadline if \
             sooner: expected the gateway, which answers at once",
            HELLO_TIMEOUT.as_secs()
        ),
        _ => error.to_string(),
    };
    frame::write(peer, hello, &[])
        .map_err(late)
        .context("sending the hello failed")?;
    // Unbuffered, as a frame is read to its end and no further, so that the session's reads,
    // through a buffer of their own, start at the gateway's first request.
    let answer = read_hello_reply(peer)
        .map_err(late)
        .context("reading its answer failed")?;
    match (answer, expected) {
        (Some(HelloReply::Accepted { grant }), expected) if grant.as_ref() == expected => Ok(()),
        (Some(HelloReply::Accepted { grant: Some(_) }), Some(_)) => {
            Err("it presented another registration grant: expected the sandbox's".to_owned())
        }
        (Some(HelloReply::Accepted { grant: Some(_) }), None) => Err(
            "it presented a registration grant, which it never does where sandboxd dials it: \
             expected none"
                .to_owned(),
        ),
        (Some(HelloReply::Accepted { grant: None }), _) => {
            Err("it presented no registration grant: expected the sandbox's".to_owned())
        }
        (Some(HelloReply::Refused { message }), _) => {
            Err(format!("it refused the session: {message}"))
        }
        (None, _) => Err(
            "it closed the connection before it answered the hello: expected it to accept or \
             refuse the session"
                .to_owned(),
        ),
    }
}

fn halves<S: Read + Write + Send + 'static>(
    stream: S,
    clone: impl FnOnce(&S) -> io::Result<S>,
) -> Result<(Reader, Writer), String> {
    let writer = clone(&stream).context("cloning the connection to the gateway failed")?;
    Ok((Box::new(stream), Box::new(writer)))
}

#[derive(Clone)]
/// Sends frames to the gateway, from any of the session's threads, each whole.
pub(crate) struct Sender(Arc<Mutex<Writer>>);

impl Sender {
    pub(crate) fn send(&self, message: &FromSandbox, body: &[u8]) -> io::Result<()> {
        let mut writer = unpoisoned(self.0.lock());
        frame::write(&mut *writer, message, body)
    }

    pub(crate) fn reply(&self, stream: u32, outcome: Result<(), String>) -> io::Result<()> {
        let message = match outcome {
            Ok(()) => FromSandbox::Done { stream },
            Err(message) => FromSandbox::Failed { stream, message },
        };
        self.send(&message, &[])
    }
}

struct Session {
    sender: Sender,
    commands: Commands,
    ops: Ops,
    model: Model,
    puts: Puts,
}

impl Session {
    fn serve(&mut self, reader: &mut impl Read) -> io::Result<()> {
        while let Some((message, body)) = frame::read(reader)? {
            self.handle(message, &body)?;
        }
        Ok(())
    }

    /// Acts on one request. A failed request is reported to the gateway; an error here means the
    /// session itself failed.
    fn handle(&mut self, message: ToSandbox, body: &[u8]) -> io::Result<()> {
        match message {
            ToSandbox::Put { stream, dir, root } => {
                self.puts.begin(&self.sender, stream, &dir, root)
            }
            ToSandbox::Entry { stream, entry } => self.puts.entry(&self.sender, stream, &entry),
            ToSandbox::Data { stream } => self.puts.data(&self.sender, stream, body),
            ToSandbox::End { stream } => self.puts.end(&self.sender, stream),
            ToSandbox::Start { stream, launch } => self.commands.start(stream, &launch),
            ToSandbox::Kill { stream, command } => {
                self.sender.reply(stream, self.commands.kill(command))
            }
            ToSandbox::Freeze { stream } => {
                let commands = self.commands.clone();
                self.aside("freeze", move |sender| {
                    sender.reply(stream, commands.freeze())
                })
            }
            ToSandbox::Collect { stream, path } => self.aside("collect", move |sender| {
                files::collect(sender, stream, Path::new(path.as_str()))
            }),
            ToSandbox::OpReply { op, reply } => self.ops.reply(op, reply),
            ToSandbox::ModelResponse {
                request,
                status,
                headers,
            } => self.model.part(request, Part::Head { status, headers }),
            ToSandbox::ModelData { request } => self.model.part(request, Part::Data(body.to_vec())),
            ToSandbox::ModelEnd { request, cut } => self.model.part(request, Part::End { cut }),
        }
    }

    /// Serves a request with `serve`, which replies to it, on a thread named `name`, or on this
    /// thread if that one can't start.
    fn aside(
        &self,
        name: &str,
        serve: impl FnOnce(&Sender) -> io::Result<()> + Clone + Send + 'static,
    ) -> io::Result<()> {
        let (sender, aside) = (self.sender.clone(), serve.clone());
        // The gateway writes some frames from threads of its own, beside its requests, each by its
        // deadline, and a frame this session doesn't read waits in the connection's buffers, then
        // in the gateway's write: so the session reads on while a tree goes out at the link's pace,
        // or while freezing waits for the killed processes to go, rather than make the gateway's
        // other writers wait that long, past their deadlines.
        let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
            // A write that fails has failed the connection, which the session's reads then see.
            let _ = aside(&sender);
        });
        match spawned {
            Ok(_) => Ok(()),
            // No thread starts while the sandbox's processes fill its limit, as a fork bomb's do,
            // yet freezing is what kills them, and collecting what keeps the run's record: so the
            // request is served here, holding the session's reads up meanwhile.
            Err(_) => serve(&self.sender),
        }
    }
}
