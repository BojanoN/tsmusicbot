use anyhow::{bail, Result};
use byteorder::{BigEndian, ReadBytesExt};
use futures::prelude::*;
use log::{error, info, trace};
use serde::Deserialize;
use std::collections::VecDeque;
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
    PlayNukedAudio(String),
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
                    ' ', '.', ' ', '=', '\t', ',', '?', '!', ':', '&', '/', '_',
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
        trace!("MSG: {}", split_vec[1]);
        return Action::PlayAudio(split_vec[1].to_string());
    }

    if split_vec[0] == "!brki" {
        trace!("MSG: {}", split_vec[1]);
        return Action::PlayNukedAudio(split_vec[1].to_string());
    }

    Action::None
}

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

    let ytdl_url = match Command::new("youtube-dl")
        .args(&[&link, "--get-url"])
        .stdout(Stdio::piped())
        .output()
    {
        Err(why) => {
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
            }
            panic!("couldn't spawn youtube-dl: {}", why);
        }
        Ok(process) => process,
    };

    let ytdl_stdout = match String::from_utf8(ytdl_url.stdout) {
        Ok(urls) => urls,
        Err(why) => panic!("Empty ytdl command output: {}", why),
    };

    let url = match ytdl_stdout.split('\n').nth(1) {
        Some(s) => s,
        None => {
            error!("Missing audio stream in {}", link);
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
                return;
            }
            return;
        }
    };

    let encoder = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
        .expect("Could not create encoder");

    let ffmpeg = match Command::new("ffmpeg")
        .args(&[
            "-loglevel",
            "quiet",
            "-i",
            url,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    real_main().await
}

async fn real_main() -> Result<()> {
    env_logger::init();

    if let Err(why) = Command::new("ffmpeg")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        error!("Unable to execute ffmpeg: {}", why);
        exit(-1);
    };

    if let Err(why) = Command::new("youtube-dl")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        error!("Unable to execute youtube-dl: {}", why);
        exit(-1);
    };

    let config_file_path = "/opt/tsmusicbot/config.json";
    let config_file = match std::fs::File::open(config_file_path) {
        Ok(id) => id,
        Err(why) => {
            error!("Unable to open configuration file: {}", why);
            exit(-1);
        }
    };

    let config_json: Config = match serde_json::from_reader(config_file) {
        Ok(cfg) => cfg,
        Err(why) => {
            error!("Failed to parse config: {}", why);
            exit(-1);
        }
    };

    let con_config = Connection::build(config_json.host)
        .name(config_json.name)
        .password(config_json.password)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false);

    let id = match Identity::new_from_str(&config_json.id) {
        Ok(id) => id,
        Err(why) => {
            error!("Invalid teamspeak3 identity string: {}", why);
            exit(-1);
        }
    };

    let con_config = con_config.identity(id);

    let mut init_con = match con_config.connect() {
        Ok(con) => con,
        Err(why) => {
            error!("Unable to connect: {}", why);
            exit(-1);
        }
    };
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

    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<String> = VecDeque::new();

    loop {
        let events = init_con.events().try_for_each(|e| async {
            match e {
                StreamItem::BookEvents(msg_vec) => {
                    for msg in msg_vec {
                        match msg {
                            Event::Message {
                                invoker: _,
                                target: _,
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
                            Action::PlayAudio(link) | Action::PlayNukedAudio(link) => {
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
                                if playing { let _ = cmd_send.send(PlayTaskCmd::ChangeVolume {modifier}).await; };
                            },
                            Action::Stop => {
                                if playing{ let _ = cmd_send.send(PlayTaskCmd::Stop).await;};
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
                        init_con.disconnect(DisconnectOptions::new())?;
                        bail!("Disconnected");
                  }
        };
    }

    // Disconnect
    init_con.disconnect(DisconnectOptions::new())?;

    Ok(())
}
