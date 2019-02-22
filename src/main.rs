extern crate alsa;
extern crate daemonize;
extern crate futures;
extern crate getopts;
extern crate hostname;
extern crate ini;
extern crate librespot;
#[macro_use]
extern crate log;
extern crate rpassword;
extern crate simplelog;
extern crate syslog;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_signal;
extern crate xdg;

use std::process::exit;
use std::panic;
use std::convert::From;
use std::error::Error;
use std::path::PathBuf;
use std::io;

use librespot::connect::spirc::{Spirc, SpircTask};
use librespot::core::session::Session;
use librespot::core::config::SessionConfig;
use librespot::playback::player::Player;
use librespot::playback::audio_backend::{Sink, BACKENDS};
use librespot::core::authentication::get_credentials;
use librespot::connect::discovery::{discovery, DiscoveryStream};
use librespot::playback::mixer;
use librespot::playback::config::PlayerConfig;
use librespot::playback::mixer::Mixer;
use librespot::core::cache::Cache;
use librespot::core::config::{ConnectConfig, DeviceType};

use daemonize::Daemonize;
use futures::{Async, Future, Poll, Stream};
use tokio_core::reactor::{Core, Handle};
use tokio_io::IoStream;
use tokio_signal::ctrl_c;

mod config;
mod cli;
mod alsa_mixer;

struct MainLoopState {
    connection: Box<Future<Item = Session, Error = io::Error>>,
    mixer: Box<FnMut() -> Box<mixer::Mixer>>,
    backend: fn(Option<String>) -> Box<Sink>,
    audio_device: Option<String>,
    spirc_task: Option<SpircTask>,
    spirc: Option<Spirc>,
    ctrl_c_stream: IoStream<()>,
    shutting_down: bool,
    cache: Option<Cache>,
    player_config: PlayerConfig,
    session_config: SessionConfig,
    device_name: String,
    handle: Handle,
    discovery_stream: DiscoveryStream,
}

impl MainLoopState {
    fn new(
        connection: Box<Future<Item = Session, Error = io::Error>>,
        mixer: Box<FnMut() -> Box<mixer::Mixer>>,
        backend: fn(Option<String>) -> Box<Sink>,
        audio_device: Option<String>,
        ctrl_c_stream: IoStream<()>,
        discovery_stream: DiscoveryStream,
        cache: Option<Cache>,
        player_config: PlayerConfig,
        session_config: SessionConfig,
        device_name: String,
        handle: Handle,
    ) -> MainLoopState {
        MainLoopState {
            connection: connection,
            mixer: mixer,
            backend: backend,
            audio_device: audio_device,
            spirc_task: None,
            spirc: None,
            ctrl_c_stream: ctrl_c_stream,
            shutting_down: false,
            cache: cache,
            player_config: player_config,
            session_config: session_config,
            device_name: device_name,
            handle: handle,
            discovery_stream: discovery_stream,
        }
    }
}

impl Future for MainLoopState {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            if let Async::Ready(Some(creds)) = self.discovery_stream.poll().unwrap() {
                if let Some(ref mut spirc) = self.spirc {
                    spirc.shutdown();
                }
                let session_config = self.session_config.clone();
                let cache = self.cache.clone();
                let handle = self.handle.clone();
                self.connection = Session::connect(session_config, creds, cache, handle);
            }

            if let Async::Ready(session) = self.connection.poll().unwrap() {
                let mixer = (self.mixer)();
                let audio_filter = mixer.get_audio_filter();
                self.connection = Box::new(futures::future::empty());
                let backend = self.backend;
                let audio_device = self.audio_device.clone();
                let player = Player::new(
                    self.player_config.clone(),
                    session.clone(),
                    audio_filter,
                    move || (backend)(audio_device),
                );

                let (spirc, spirc_task) = Spirc::new(
                    ConnectConfig {
                        name: self.device_name.clone(),
                        device_type: DeviceType::default(),
                        volume: mixer.volume() as i32,
                    },
                    session,
                    player,
                    mixer,
                );
                self.spirc_task = Some(spirc_task);
                self.spirc = Some(spirc);
            } else if let Async::Ready(_) = self.ctrl_c_stream.poll().unwrap() {
                if !self.shutting_down {
                    if let Some(ref spirc) = self.spirc {
                        spirc.shutdown();
                        self.shutting_down = true;
                    } else {
                        return Ok(Async::Ready(()));
                    }
                }
            } else if let Some(Async::Ready(_)) = self.spirc_task
                .as_mut()
                .map(|ref mut st| st.poll().unwrap())
            {
                return Ok(Async::Ready(()));
            } else {
                return Ok(Async::NotReady);
            }
        }
    }
}

fn main() {
    let opts = cli::command_line_argument_options();
    let args: Vec<String> = std::env::args().collect();

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            println!("Error: {}\n{}", f.to_string(), cli::usage(&args[0], &opts));
            exit(1)
        }
    };

    if matches.opt_present("backends") {
        cli::print_backends();
        exit(0);
    }

    if matches.opt_present("help") {
        println!("{}", cli::usage(&args[0], &opts));
        exit(0);
    }

    if matches.opt_present("no-daemon") {
        let filter = if matches.opt_present("verbose") {
            simplelog::LogLevelFilter::Trace
        } else {
            simplelog::LogLevelFilter::Info
        };

        simplelog::TermLogger::init(filter, simplelog::Config::default())
            .map_err(Box::<Error>::from)
            .or_else(|_| {
                simplelog::SimpleLogger::init(filter, simplelog::Config::default())
                    .map_err(Box::<Error>::from)
            })
            .expect("Couldn't initialize logger");
    } else {
        let filter = if matches.opt_present("verbose") {
            log::LogLevelFilter::Trace
        } else {
            log::LogLevelFilter::Info
        };
        syslog::init(syslog::Facility::LOG_DAEMON, filter, Some("Spotifyd"))
            .expect("Couldn't initialize logger");

        let mut daemonize = Daemonize::new();
        if let Some(pid) = matches.opt_str("pid") {
            daemonize = daemonize.pid_file(pid);
        }
        match daemonize.start() {
            Ok(_) => info!("Detached from shell, now running in background."),
            Err(e) => error!("Something went wrong while daemonizing: {}", e),
        };
    }

    panic::set_hook(Box::new(|panic_info| {
        error!(
            "Caught panic with message: {}",
            match (
                panic_info.payload().downcast_ref::<String>(),
                panic_info.payload().downcast_ref::<&str>(),
            ) {
                (Some(s), _) => &**s,
                (_, Some(&s)) => s,
                _ => "Unknown error type, can't produce message.",
            }
        );
    }));

    let config_file = matches
        .opt_str("config")
        .map(|s| PathBuf::from(s))
        .or_else(|| config::get_config_file().ok());
    let config = config::get_config(config_file, &matches);

    let local_audio_device = config.audio_device.clone();
    let local_mixer = config.mixer.clone();
    let mut mixer = match config.volume_controller {
        config::VolumeController::Alsa => {
            info!("Using alsa volume controller.");
            Box::new(move || {
                Box::new(alsa_mixer::AlsaMixer {
                    device: local_audio_device.clone().unwrap_or("default".to_string()),
                    mixer: local_mixer.clone().unwrap_or("Master".to_string()),
                }) as Box<Mixer>
            }) as Box<FnMut() -> Box<Mixer>>
        }
        config::VolumeController::SoftVol => {
            info!("Using software volume controller.");
            Box::new(|| Box::new(mixer::softmixer::SoftMixer::open()) as Box<Mixer>)
                as Box<FnMut() -> Box<Mixer>>
        }
    };

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let cache = config.cache;
    let player_config = config.player_config;
    let session_config = config.session_config;
    let backend = config.backend.clone();
    let device_id = session_config.device_id.clone();
    let discovery_stream = discovery(
        &handle,
        ConnectConfig {
            name: config.device_name.clone(),
            device_type: DeviceType::default(),
            volume: (mixer()).volume() as i32,
        },
        device_id,
        0,
    ).unwrap();
    let connection = if let Some(credentials) = get_credentials(
        config.username.or(matches.opt_str("username")),
        config.password.or(matches.opt_str("password")),
        cache.as_ref().and_then(Cache::credentials),
    ) {
        Session::connect(
            session_config.clone(),
            credentials,
            cache.clone(),
            handle.clone(),
        )
    } else {
        Box::new(futures::future::empty())
            as Box<futures::Future<Item = Session, Error = io::Error>>
    };

    let backend = find_backend(backend.as_ref().map(String::as_ref));
    let initial_state = MainLoopState::new(
        connection,
        mixer,
        backend,
        config.audio_device.clone(),
        Box::new(ctrl_c(&handle).flatten_stream()),
        discovery_stream,
        cache,
        player_config,
        session_config,
        config.device_name.clone(),
        handle,
    );
    core.run(initial_state).unwrap();
}

fn find_backend(name: Option<&str>) -> fn(Option<String>) -> Box<Sink> {
    match name {
        Some(name) => {
            BACKENDS
                .iter()
                .find(|backend| name == backend.0)
                .expect(format!("Unknown backend: {}.", name).as_ref())
                .1
        }
        None => {
            let &(name, back) = BACKENDS
                .first()
                .expect("No backends were enabled at build time");
            info!("No backend specified, defaulting to: {}.", name);
            back
        }
    }
}
