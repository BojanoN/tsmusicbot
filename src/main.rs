use anyhow::{bail, Result};
use byteorder::{BigEndian, ReadBytesExt};
use futures::prelude::*;
use log::{error, info, trace};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::ErrorKind::NotFound;
use std::io::Read;
use std::path::Path;
use std::process::{exit, Command, Stdio};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};

use tsclientlib::events::Event;
use tsclientlib::{Connection, DisconnectOptions, Identity, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

extern crate audiopus;
extern crate byteorder;
extern crate serde;
extern crate serde_json;

#[derive(Debug, Deserialize)]
struct Config {
    host: String,
    password: String,
    name: String,
    id: String,
}

#[derive(Debug)]
enum Action {
    PlayAudio(String),
    Stop,
    ChangeVolume { modifier: f32 },
    None,
}

#[derive(Debug)]
enum PlayTaskCmd {
    Stop,
    ChangeVolume { modifier: f32 },
}

#[derive(Debug)]
enum AudioPacket {
    Payload(OutPacket),
    None,
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            c.is_alphanumeric()
                || [
                    ' ', '.', ' ', '=', '\t', ',', '?', '!', ':', '&', '/', '-', '_',
                ]
                .contains(c)
        })
        .collect()
}

fn parse_command(msg: &str) -> Action {
    let stripped = msg.replace("[URL]", "").replace("[/URL]", "");
    let sanitized = sanitize(&stripped).trim().to_string();

    if &sanitized[..=0] != "!" {
        return Action::None;
    }

    let split_vec: Vec<&str> = sanitized.split(' ').collect();

    if split_vec[0] == "!stop" {
        println!("STOP MSG");

        return Action::Stop;
    }

    if split_vec.len() < 2 {
        return Action::None;
    }

    if split_vec[0] == "!volume" {
        let amount = split_vec[1].parse::<u32>();
        match amount {
            Err(_) => {
                return Action::None;
            }
            Ok(num) => {
                let modifier: f32 = num.max(0).min(100) as f32 / 100_f32;
                return Action::ChangeVolume { modifier };
            }
        };
    }

    if split_vec[0] == "!yt" {
        println!("MSG: {}", split_vec[1]);

        return Action::PlayAudio(split_vec[1].to_string());
    }

    Action::None
}

const CACHE_FOLDER: &str = "./cache";
const DEFAULT_VOLUME: f32 = 0.2;

async fn play_file(
    link: String,
    pkt_send: mpsc::Sender<AudioPacket>,
    mut cmd_recv: mpsc::Receiver<PlayTaskCmd>,
    volume: f32,
) {
    const FRAME_SIZE: usize = 960;
    const MAX_PACKET_SIZE: usize = 3 * 1276;

    let codec = CodecType::OpusMusic;
    let mut current_volume = volume;

    let mut ytdl_fname = match Command::new("youtube-dl")
        .args(&[&link, "--get-filename"])
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(why) => panic!("couldn't spawn ffmpeg: {}", why),
        Ok(process) => process,
    };

    match ytdl_fname.wait() {
        Err(e) => {
            error!("Error: {}", e);
            return;
        }
        Ok(status) => {
            if let false = status.success() {
                error!("youtube-dl error");
                if let Err(e) = pkt_send.send(AudioPacket::None).await {
                    error!("Status packet sending error: {}", e);
                    return;
                }
                return;
            }
        }
    };

    info!("CALLED: {}", &link);

    let mut fname: String = String::new();

    if let Err(e) = ytdl_fname.stdout.unwrap().read_to_string(&mut fname) {
        error!("Error: {}", e);
        if let Err(e) = pkt_send.send(AudioPacket::None).await {
            error!("Status packet sending error: {}", e);
            return;
        }
        return;
    }

    fname = Path::new(".")
        .join(CACHE_FOLDER)
        .join(
            [&fname[0..fname.rfind('.').unwrap()], ".wav"]
                .join("")
                .replace('\n', ""),
        )
        .to_str()
        .unwrap()
        .to_string();

    let mut ytdl = match Command::new("youtube-dl")
        .args(&[
            "-x",
            "--output",
            &[CACHE_FOLDER, "/%(title)s-%(id)s.%(ext)s"].join(""),
            "--audio-format",
            "wav",
            &link,
        ])
        .spawn()
    {
        Err(why) => panic!("couldn't spawn youtube-dl: {}", why),
        Ok(process) => process,
    };

    if let Err(e) = ytdl.wait() {
        error!("Error: {}", e);
        if let Err(e) = pkt_send.send(AudioPacket::None).await {
            error!("Status packet sending error: {}", e);
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
                return;
            }
            return;
        }
        return;
    };

    let encoder = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .expect("Could not create encoder");

    info!("FNAME: {}", &fname);
    let ffmpeg = match Command::new("ffmpeg")
        .args(&[
            "-loglevel",
            "quiet",
            "-i",
            &fname,
            "-af",
            "aresample=48000",
            "-f",
            "s16be",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(why) => panic!("couldn't spawn ffmpeg: {}", why),
        Ok(process) => process,
    };

    let mut pcm_in_be: [i16; FRAME_SIZE * 2] = [0; FRAME_SIZE * 2];
    let mut opus_pkt: [u8; MAX_PACKET_SIZE] = [0; MAX_PACKET_SIZE];

    let mut ffmpeg_stdout = ffmpeg.stdout.unwrap();

    //let mut start;

    loop {
        // start = Instant::now();

        let cmd: Option<PlayTaskCmd> =
            match timeout(Duration::from_micros(1), cmd_recv.recv()).await {
                Err(_) => None,
                Ok(msg) => msg,
            };

        match cmd {
            None => {}
            Some(PlayTaskCmd::ChangeVolume { modifier }) => {
                current_volume = modifier;
            }
            Some(PlayTaskCmd::Stop) => {
                break;
            }
        };

        if ffmpeg_stdout
            .read_i16_into::<BigEndian>(&mut pcm_in_be)
            .is_err()
        {
            break;
        };

        for i in 0..FRAME_SIZE * 2 {
            pcm_in_be[i] = (pcm_in_be[i] as f32 * current_volume) as i16;
        }

        let len = encoder.encode(&pcm_in_be, &mut opus_pkt[..]).unwrap();

        let packet = OutAudio::new(&AudioData::C2S {
            id: 0,
            codec,
            data: &opus_pkt[..len],
        });

        if let Err(e) = pkt_send.send(AudioPacket::Payload(packet)).await {
            error!("Audio packet sending error: {}", e);
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
                return;
            }
            break;
        }

        let usec_sleep = Duration::from_micros(17000);

        sleep(usec_sleep).await;
    }

    info!("Cleanup...");
    if let Err(e) = pkt_send.send(AudioPacket::None).await {
        error!("Status packet sending error: {}", e);
        return;
    }
    cmd_recv.close();

    if let Err(e) = std::fs::remove_file(fname) {
        error!("Error removing file: {}", e);
        return;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    real_main().await
}

async fn real_main() -> Result<()> {
    if let Err(e) = Command::new("ffmpeg").spawn() {
        if let NotFound = e.kind() {
            error!("ffmpeg was not found!");
            exit(-1);
        }
    }

    if let Err(e) = Command::new("youtube-dl").spawn() {
        if let NotFound = e.kind() {
            error!("youtube-dl was not found!");
            exit(-1);
        }
    }

    let config_file = std::fs::File::open("config.json").expect("Failed to open config");
    let config_json: Config = serde_json::from_reader(config_file).expect("Failed to parse config");

    let con_config = Connection::build(config_json.host)
        .name(config_json.name)
        .password(config_json.password)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false);

    let id = Identity::new_from_str(&config_json.id).unwrap();

    let con_config = con_config.identity(id);

    let mut init_con = con_config.connect()?;
    let r = init_con
        .events()
        .try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
        .next()
        .await;
    if let Some(r) = r {
        r?;
    }

    let (pkt_send, mut pkt_recv) = mpsc::channel(64);

    let (status_send, mut status_recv) = mpsc::channel(64);

    let mut playing: bool = false;

    // Mount ramfs cache using /dev/shm
    let shm_dir = Path::new("/dev/shm/").join(CACHE_FOLDER);
    let cache_symlink = Path::new(CACHE_FOLDER);

    if !shm_dir.exists() {
        std::fs::create_dir(shm_dir)?;
    }

    if cache_symlink.exists() {
        let md = std::fs::symlink_metadata(cache_symlink).unwrap();

        if md.is_dir() {
            std::fs::remove_dir(cache_symlink)?;
        } else if md.is_file() {
            std::fs::remove_file(cache_symlink)?;
        }
    }

    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<String> = VecDeque::new();

    loop {
        let events = init_con.events().try_for_each(|e| async {
            match e {
                StreamItem::BookEvents(msg_vec) => {
                    for msg in msg_vec {
                        match msg {
                            Event::Message {
                                invoker,
                                target,
                                message,
                            } => {
                                if let Err(e) = status_send.send(parse_command(&message)).await {
                                    error!("Status packet sending error: {}", e);
                                }
                            }

                            _ => {}
                        }
                    }
                }
                _ => {}
            };
            Ok(())
        });

        tokio::select! {

          val =   status_recv.recv() => {
                match val {
                    None => {
                    },
                    Some(action) => {
                        match action {
                            Action::PlayAudio(link) => {
                                trace!("RECV");
                                if !playing{
                                    playing = true;
                                    let audio_task_pkt_send = pkt_send.clone();

                                    let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                    cmd_send = task_cmd_send;

                                    tokio::spawn(async move {
                                        play_file(link, audio_task_pkt_send, task_cmd_recv,  DEFAULT_VOLUME).await;
                                    });
                                } else {
                                    play_queue.push_back(link);
                                }
                            },
                            Action::ChangeVolume {modifier} => {
                                if playing { cmd_send.send(PlayTaskCmd::ChangeVolume {modifier}).await; };
                            },
                            Action::Stop => {
                                if playing{ cmd_send.send(PlayTaskCmd::Stop).await;};
                            }
                            _ => {},
                        }
                    }
                }
            }

        val = pkt_recv.recv() => {
            match val {
                None => {
                },
                Some(msg) => {
                    if playing{

                        match msg {

                           AudioPacket::Payload(pkt) =>{
                    if let Err(e) = init_con.send_audio(pkt) {
                            error!("Audio packet sending error: {}", e);
                            break;
                    }},
                            AudioPacket::None => {
                                if play_queue.is_empty(){
                                    playing = false;
                                } else {
                                    let link = play_queue.pop_front().unwrap();
                                    let audio_task_pkt_send = pkt_send.clone();

                                    let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                    cmd_send = task_cmd_send;

                                    tokio::spawn(async move {
                                        play_file(link, audio_task_pkt_send, task_cmd_recv,  DEFAULT_VOLUME).await;
                                    });
                                }
                            }
                        }
                    }
            }
            }
        }

            _ = tokio::signal::ctrl_c() => { break; }
            r = events => {
                        r?;
                        bail!("Disconnected");
                  }
        };
    }

    // Disconnect
    init_con.disconnect(DisconnectOptions::new())?;

    Ok(())
}
