mod agent;
mod bench;
mod capture;
mod clock;
mod encode;
mod frame;
mod input;
mod nvenc;
#[rustfmt::skip]
mod nvenc_sys;
mod pacer;
mod probe;
mod protocol;
mod server;
mod session;
mod ticket;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "skylight",
    version,
    about = "Low-latency browser remote desktop for Linux"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum EncoderArg {
    Auto,
    Nvenc,
    Vaapi,
    Software,
}

impl From<EncoderArg> for encode::Backend {
    fn from(e: EncoderArg) -> Self {
        match e {
            EncoderArg::Auto => encode::Backend::Auto,
            EncoderArg::Nvenc => encode::Backend::Nvenc,
            EncoderArg::Vaapi => encode::Backend::Vaapi,
            EncoderArg::Software => encode::Backend::Software,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the desktop on a loopback address.
    Serve {
        /// Loopback address to listen on. Anything else is refused.
        #[arg(long, default_value = "127.0.0.1:5490", env = "SKYLIGHT_LISTEN")]
        listen: SocketAddr,
        /// This server's name; tickets must carry it as `aud`.
        #[arg(long, env = "SKYLIGHT_AUDIENCE")]
        audience: Option<String>,
        /// Ticket public keys, comma-separated: files (raw base64url or SPKI PEM) or base64url keys.
        #[arg(long, env = "SKYLIGHT_TICKET_KEY")]
        ticket_key: String,
        /// Browser origins allowed to open the WebSocket (besides the built-in viewer).
        #[arg(
            long = "allow-origin",
            env = "SKYLIGHT_ALLOW_ORIGINS",
            value_delimiter = ','
        )]
        allow_origins: Vec<String>,
        #[arg(long, default_value = "auto", env = "SKYLIGHT_BACKEND")]
        backend: String,
        #[arg(long, value_enum, default_value = "auto")]
        encoder: EncoderArg,
        /// Frame-rate cap; clients may ask for less.
        #[arg(long, default_value_t = 144)]
        max_fps: u32,
        /// Bitrate cap in Mbit/s.
        #[arg(long, default_value_t = 50.0)]
        max_mbps: f64,
        /// Starting bitrate in Mbit/s; the controller adapts from here.
        #[arg(long, default_value_t = 16.0)]
        start_mbps: f64,
        /// NVENC preset 1 (fastest) to 7. p1 and p2 encode equally fast on Ada; p2 looks better.
        #[arg(long, default_value_t = 2)]
        nvenc_preset: u8,
        /// Encoder VBV in milliseconds: the largest single frame, in time at the target bitrate.
        #[arg(long, default_value_t = 34)]
        vbv_ms: u32,
        /// Quality mode under the bitrate cap: frames cost only what changed (x264 CRF value;
        /// NVENC uses VBR). 0 switches to CBR, sharper on a LAN but every frame is full size.
        #[arg(long, default_value_t = 23)]
        cq: u8,
        /// Turn off the high-bitrate refresh after motion stops.
        #[arg(long)]
        no_refine: bool,
        #[arg(long, default_value_t = 4)]
        max_sessions: usize,
        /// X display the latency probe opens on (Xwayland's, on KDE). Defaults to $DISPLAY.
        #[arg(long, env = "SKYLIGHT_PROBE_DISPLAY")]
        probe_display: Option<String>,
        /// Serve the web client from this directory instead of the built-in copy.
        #[arg(long)]
        web_dir: Option<PathBuf>,
    },
    /// Create a ticket signing key pair.
    Keygen {
        /// Directory for `ticket-private.pem` (give it to the host app) and `ticket-public.txt`.
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Mint a ticket with a private key (for testing; the host app does this in production).
    Ticket {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        audience: String,
        #[arg(long, default_value = "view,input,clipboard")]
        scope: String,
        #[arg(long, default_value = "local")]
        sub: String,
        #[arg(long, default_value_t = 60)]
        ttl: u64,
        /// Print a viewer URL with the ticket in its fragment instead of the bare ticket.
        #[arg(long)]
        url: Option<String>,
    },
    /// Let KWin trust this binary for screencast and input (writes a .desktop file).
    Install {
        /// Also write and enable a systemd user service with these serve arguments.
        #[arg(long)]
        service: bool,
    },
    /// Check the capture backend, monitors and encoders on this machine.
    Doctor {
        #[arg(long, default_value = "auto")]
        backend: String,
    },
    /// Measure capture and encode speed on this machine.
    Bench {
        #[arg(long, default_value = "synthetic")]
        backend: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long, value_enum, default_value = "auto")]
        encoder: EncoderArg,
        #[arg(long, default_value = "h264")]
        codec: String,
        #[arg(long, default_value_t = 20.0)]
        mbps: f64,
        #[arg(long, default_value_t = 2)]
        nvenc_preset: u8,
        /// Constant-quality target; 0 for CBR.
        #[arg(long, default_value_t = 23)]
        cq: u8,
    },
    /// The latency probe window (started by the server during a benchmark).
    #[command(hide = true)]
    ProbeWindow {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value_t = 160)]
        size: u16,
        #[arg(long, default_value = "move")]
        mode: String,
    },
}

fn read_key_arg(arg: &str) -> Result<String> {
    let p = std::path::Path::new(arg);
    if p.exists() {
        return std::fs::read_to_string(p).with_context(|| format!("reading {arg}"));
    }
    Ok(arg.to_string())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "skylight".into())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "skylight=info".into()),
        )
        .init();
    clock::init();
    encode::init_logging(std::env::var_os("SKYLIGHT_FFMPEG_LOG").is_some());
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            listen,
            audience,
            ticket_key,
            allow_origins,
            backend,
            encoder,
            max_fps,
            max_mbps,
            start_mbps,
            nvenc_preset,
            vbv_ms,
            cq,
            no_refine,
            max_sessions,
            probe_display,
            web_dir,
        } => {
            if !listen.ip().is_loopback() {
                bail!("--listen must be a loopback address (got {listen})");
            }
            let keys = ticket_key
                .split(',')
                .map(|k| ticket::parse_public(&read_key_arg(k.trim())?))
                .collect::<Result<Vec<_>>>()?;
            let audience = audience.unwrap_or_else(hostname);
            let backend = capture::open_backend(&backend, max_fps)?;
            let encoders =
                encode::probe(&[encode::Codec::H264, encode::Codec::Hevc, encode::Codec::Av1]);
            for (c, name) in &encoders {
                tracing::info!("encoder for {c:?}: {name}");
            }
            if encoders.is_empty() {
                bail!("no video encoder available");
            }
            let mbps = |m: f64| (m * 1_000_000.0) as u64;
            let app = Arc::new(session::App {
                hub: capture::Hub::new(backend),
                verifier: ticket::Verifier::new(keys, audience.clone()),
                config: session::Config {
                    max_fps,
                    max_bitrate: mbps(max_mbps),
                    start_bitrate: mbps(start_mbps.min(max_mbps)),
                    min_bitrate: mbps(0.5),
                    encoder: encoder.into(),
                    nvenc_preset,
                    vbv_ms,
                    cq: (cq > 0).then_some(cq),
                    refine: !no_refine,
                    allowed_origins: allow_origins,
                    max_sessions,
                    probe_display: probe_display.or_else(|| std::env::var("DISPLAY").ok()),
                },
                sessions: AtomicUsize::new(0),
                unauthenticated: AtomicUsize::new(0),
                input_tokens: std::sync::Mutex::new(std::collections::HashMap::new()),
                encoders,
            });
            tracing::info!(audience = %audience, backend = app.hub.backend.name(), "ready");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(server::serve(app, listen, web_dir))?;
        }
        Cmd::Keygen { out_dir } => {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::create_dir_all(&out_dir)?;
            let k = ticket::generate();
            let priv_path = out_dir.join("ticket-private.pem");
            if priv_path.exists() {
                bail!("{} already exists; move it away first", priv_path.display());
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&priv_path)?;
            std::io::Write::write_all(&mut f, ticket::private_pem(&k).as_bytes())?;
            let pub_path = out_dir.join("ticket-public.txt");
            std::fs::write(&pub_path, ticket::public_b64(&k.verifying_key()) + "\n")?;
            std::fs::write(
                out_dir.join("ticket-public.pem"),
                ticket::public_pem(&k.verifying_key()),
            )?;
            println!(
                "private key (host app only, chmod 600): {}",
                priv_path.display()
            );
            println!(
                "public key (skylight serve --ticket-key): {}",
                pub_path.display()
            );
        }
        Cmd::Ticket {
            key,
            audience,
            scope,
            sub,
            ttl,
            url,
        } => {
            let k = ticket::parse_private(&std::fs::read_to_string(&key)?)?;
            let now = clock::unix_secs();
            let claims = ticket::Claims {
                v: 1,
                aud: audience,
                sub,
                iat: now,
                exp: now + ttl.min(ticket::MAX_TTL),
                jti: ticket::random_id(),
                scope: scope
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                mon: None,
                ses: None,
            };
            let t = ticket::mint(&k, &claims);
            match url {
                Some(base) => println!("{}#ticket={t}", base.trim_end_matches('#')),
                None => println!("{t}"),
            }
        }
        Cmd::Install { service } => install(service)?,
        Cmd::Doctor { backend } => doctor(&backend)?,
        Cmd::Bench {
            backend,
            seconds,
            encoder,
            codec,
            mbps,
            nvenc_preset,
            cq,
        } => {
            let codec = encode::Codec::parse(&codec).context("codec must be h264, hevc or av1")?;
            bench::run(
                &backend,
                seconds,
                encoder.into(),
                codec,
                (mbps * 1e6) as u64,
                nvenc_preset,
                (cq > 0).then_some(cq),
            )?;
        }
        Cmd::ProbeWindow { x, y, size, mode } => probe::run(
            x,
            y,
            size,
            probe::Mode::parse(&mode).context("mode is move, click or key")?,
        )?,
    }
    Ok(())
}

fn doctor(backend: &str) -> Result<()> {
    let b = capture::open_backend(backend, 60)?;
    println!("backend:   {}", b.name());
    for m in b.monitors() {
        println!(
            "monitor:   {} ({}) {}x{} px at {},{} logical {}x{}, {:.0} Hz",
            m.id,
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            m.logical_width,
            m.logical_height,
            m.refresh_mhz as f64 / 1000.0
        );
    }
    println!(
        "input:     {}",
        if b.input().is_some() { "yes" } else { "no" }
    );
    println!(
        "clipboard: {}",
        if b.clipboard().is_some() { "yes" } else { "no" }
    );
    for (c, name) in encode::probe(&[encode::Codec::H264, encode::Codec::Hevc, encode::Codec::Av1])
    {
        println!("encoder:   {c:?} via {name}");
    }
    let hub = capture::Hub::new(b);
    let mon = hub.resolve(None)?;
    let cap = hub.capture(&mon)?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let st = cap.store.lock();
    match &st.error {
        Some(e) => println!("capture:   failed: {e}"),
        None => println!(
            "capture:   {}x{} {:?}, {} frame(s) in 1 s (an idle screen sends few)",
            st.width, st.height, st.format, st.frames_in
        ),
    }
    Ok(())
}

fn install(service: bool) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let home = std::env::var("HOME").context("HOME is not set")?;
    let apps = PathBuf::from(&home).join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    let desktop = apps.join("dev.skylight.server.desktop");
    std::fs::write(
        &desktop,
        format!(
            "[Desktop Entry]\nType=Application\nName=Skylight\nComment=Remote desktop server\nExec={}\nNoDisplay=true\n\
             X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input\n",
            exe.display()
        ),
    )?;
    println!("wrote {}", desktop.display());
    let status = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        println!("note: kbuildsycoca6 did not run; KWin picks the file up at the next login");
    }
    if service {
        let unit_dir = PathBuf::from(&home).join(".config/systemd/user");
        std::fs::create_dir_all(&unit_dir)?;
        let unit = unit_dir.join("skylight.service");
        std::fs::write(
            &unit,
            include_str!("../deploy/skylight.service").replace("@BIN@", &exe.display().to_string()),
        )?;
        println!(
            "wrote {} (edit ~/.config/skylight/env, then: systemctl --user enable --now skylight)",
            unit.display()
        );
    }
    Ok(())
}
