use audiodevice::*;
use config;
use config::{ConfigError, SampleFormat};
use conversions::{buffer_to_chunk_rawbytes, chunk_to_buffer_rawbytes};
use countertimer;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError, TrySendError};
use rubato::VecResampler;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::Duration;
use wasapi;

use crate::{CaptureStatus, PlaybackStatus};
use CommandMessage;
use PrcFmt;
use ProcessingState;
use Res;
use StatusMessage;

enum DeviceState {
    Ok,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct WasapiPlaybackDevice {
    pub devname: String,
    pub exclusive: bool,
    pub samplerate: usize,
    pub chunksize: usize,
    pub channels: usize,
    pub sample_format: SampleFormat,
    pub target_level: usize,
    pub adjust_period: f32,
    pub enable_rate_adjust: bool,
}

#[derive(Clone, Debug)]
pub struct WasapiCaptureDevice {
    pub devname: String,
    pub exclusive: bool,
    pub loopback: bool,
    pub samplerate: usize,
    pub resampler_conf: config::Resampler,
    pub enable_resampling: bool,
    pub capture_samplerate: usize,
    pub chunksize: usize,
    pub channels: usize,
    pub sample_format: SampleFormat,
    pub silence_threshold: PrcFmt,
    pub silence_timeout: PrcFmt,
    pub stop_on_rate_change: bool,
    pub rate_measure_interval: f32,
}

#[derive(Clone, Debug)]
enum DisconnectReason {
    FormatChange,
    Error,
}

fn get_wave_format(
    sample_format: &SampleFormat,
    samplerate: usize,
    channels: usize,
) -> wasapi::WaveFormat {
    match sample_format {
        SampleFormat::S16LE => {
            wasapi::WaveFormat::new(16, 16, &wasapi::SampleType::Int, samplerate, channels)
        }
        SampleFormat::S24LE => {
            wasapi::WaveFormat::new(32, 24, &wasapi::SampleType::Int, samplerate, channels)
        }
        SampleFormat::S24LE3 => {
            wasapi::WaveFormat::new(24, 24, &wasapi::SampleType::Int, samplerate, channels)
        }
        SampleFormat::S32LE => {
            wasapi::WaveFormat::new(32, 32, &wasapi::SampleType::Int, samplerate, channels)
        }
        SampleFormat::FLOAT32LE => {
            wasapi::WaveFormat::new(32, 32, &wasapi::SampleType::Float, samplerate, channels)
        }
        _ => panic!("Unsupported sample format"),
    }
}

fn open_playback(
    devname: &str,
    samplerate: usize,
    channels: usize,
    sample_format: &SampleFormat,
    exclusive: bool,
) -> Res<(
    wasapi::Device,
    wasapi::AudioClient,
    wasapi::AudioRenderClient,
    wasapi::Handle,
    wasapi::WaveFormat,
)> {
    let sharemode = match exclusive {
        true => wasapi::ShareMode::Exclusive,
        false => wasapi::ShareMode::Shared,
    };
    let device = if devname == "default" {
        wasapi::get_default_device(&wasapi::Direction::Render)?
    } else {
        let collection = wasapi::DeviceCollection::new(&wasapi::Direction::Render)?;
        collection.get_device_with_name(devname)?
    };
    trace!("Found playback device {}", devname);
    let mut audio_client = device.get_iaudioclient()?;
    trace!("Got playback iaudioclient");
    let wave_format = get_wave_format(sample_format, samplerate, channels);
    match audio_client.is_supported(&wave_format, &sharemode) {
        Ok(None) => {
            debug!("Playback device supports format {:?}", wave_format)
        }
        Ok(Some(modified)) => {
            let msg = format!(
                "Playback device doesn't support format:\n{:#?}\nClosest match is:\n{:#?}",
                wave_format, modified
            );
            return Err(ConfigError::new(&msg).into());
        }
        Err(err) => {
            let msg = format!(
                "Playback device doesn't support format:\n{:#?}\nError: {}",
                wave_format, err
            );
            return Err(ConfigError::new(&msg).into());
        }
    };
    let (def_time, min_time) = audio_client.get_periods()?;
    debug!(
        "playback default period {}, min period {}",
        def_time, min_time
    );
    audio_client.initialize_client(
        &wave_format,
        def_time as i64,
        &wasapi::Direction::Render,
        &sharemode,
        false,
    )?;
    debug!("initialized capture");
    let handle = audio_client.set_get_eventhandle()?;
    let render_client = audio_client.get_audiorenderclient()?;
    debug!("Opened Wasapi playback device {}", devname);
    Ok((device, audio_client, render_client, handle, wave_format))
}

fn open_capture(
    devname: &str,
    samplerate: usize,
    channels: usize,
    sample_format: &SampleFormat,
    exclusive: bool,
    loopback: bool,
) -> Res<(
    wasapi::Device,
    wasapi::AudioClient,
    wasapi::AudioCaptureClient,
    wasapi::Handle,
    wasapi::WaveFormat,
)> {
    let sharemode = match exclusive {
        true => wasapi::ShareMode::Exclusive,
        false => wasapi::ShareMode::Shared,
    };
    let device = if devname == "default" && !loopback {
        wasapi::get_default_device(&wasapi::Direction::Capture)?
    } else if devname == "default" && loopback {
        wasapi::get_default_device(&wasapi::Direction::Render)?
    } else if !loopback {
        let collection = wasapi::DeviceCollection::new(&wasapi::Direction::Capture)?;
        collection.get_device_with_name(devname)?
    } else {
        let collection = wasapi::DeviceCollection::new(&wasapi::Direction::Render)?;
        collection.get_device_with_name(devname)?
    };
    trace!("Found capture device {}", devname);
    let mut audio_client = device.get_iaudioclient()?;
    trace!("Got capture iaudioclient");
    let wave_format = get_wave_format(sample_format, samplerate, channels);
    match audio_client.is_supported(&wave_format, &sharemode) {
        Ok(None) => {
            debug!("Capture device supports format {:?}", wave_format)
        }
        Ok(Some(modified)) => {
            let msg = format!(
                "Capture device doesn't support format:\n{:#?}\nClosest match is:\n{:#?}",
                wave_format, modified
            );
            return Err(ConfigError::new(&msg).into());
        }
        Err(err) => {
            let msg = format!(
                "Capture device doesn't support format:\n{:#?}\nError: {}",
                wave_format, err
            );
            return Err(ConfigError::new(&msg).into());
        }
    };
    let (def_time, min_time) = audio_client.get_periods()?;
    debug!(
        "capture default period {}, min period {}",
        def_time, min_time
    );
    audio_client.initialize_client(
        &wave_format,
        def_time as i64,
        &wasapi::Direction::Capture,
        &sharemode,
        loopback,
    )?;
    debug!("initialized capture");
    let handle = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    debug!("Opened Wasapi capture device {}", devname);
    Ok((device, audio_client, capture_client, handle, wave_format))
}

struct PlaybackSync {
    rx_play: Receiver<PlaybackDeviceMessage>,
    tx_cb: Sender<DisconnectReason>,
    bufferfill: Arc<AtomicUsize>,
}

enum PlaybackDeviceMessage {
    Data(Vec<u8>),
    Stop,
}

// Playback loop, play samples received from channel
fn playback_loop(
    audio_client: wasapi::AudioClient,
    render_client: wasapi::AudioRenderClient,
    handle: wasapi::Handle,
    blockalign: usize,
    chunksize: usize,
    sync: PlaybackSync,
) -> Res<()> {
    let mut buffer_free_frame_count = audio_client.get_bufferframecount()?;
    let mut sample_queue: VecDeque<u8> = VecDeque::with_capacity(
        4 * blockalign * (chunksize + 2 * buffer_free_frame_count as usize),
    );

    let tx_cb = sync.tx_cb;
    let mut callbacks = wasapi::EventCallbacks::new();
    callbacks.set_disconnected_callback(move |reason| {
        debug!("Disconnected, reason: {:?}", reason);
        let simplereason = match reason {
            wasapi::DisconnectReason::FormatChanged => DisconnectReason::FormatChange,
            _ => DisconnectReason::Error,
        };
        tx_cb.send(simplereason).unwrap_or(());
    });
    let callbacks_rc = Rc::new(callbacks);
    let callbacks_weak = Rc::downgrade(&callbacks_rc);
    let sessioncontrol = audio_client.get_audiosessioncontrol()?;
    sessioncontrol.register_session_notification(callbacks_weak)?;

    let mut waited_millis = 0;
    trace!("Waiting for data to start playback, will time out after one second");
    while sync.rx_play.len() < 2 && waited_millis < 1000 {
        thread::sleep(Duration::from_millis(10));
        waited_millis += 10;
    }
    debug!("Waited for data for {} ms", waited_millis);

    audio_client.start_stream()?;
    let mut running = true;
    loop {
        buffer_free_frame_count = audio_client.get_available_space_in_frames()?;
        trace!("New buffer frame count {}", buffer_free_frame_count);
        while sample_queue.len() < (blockalign as usize * buffer_free_frame_count as usize) {
            trace!("playback loop needs more samples, reading from channel");
            match sync.rx_play.try_recv() {
                Ok(PlaybackDeviceMessage::Data(chunk)) => {
                    trace!("got chunk");
                    for element in chunk.iter() {
                        sample_queue.push_back(*element);
                    }
                    if !running {
                        running = true;
                        info!("Restarting playback after buffer underrun");
                    }
                }
                Ok(PlaybackDeviceMessage::Stop) => {
                    debug!("Stopping inner playback loop");
                    audio_client.stop_stream()?;
                    return Ok(());
                }
                Err(TryRecvError::Empty) => {
                    for _ in 0..((blockalign as usize * buffer_free_frame_count as usize)
                        - sample_queue.len())
                    {
                        sample_queue.push_back(0);
                    }
                    if running {
                        running = false;
                        warn!("Playback interrupted, no data available");
                    }
                }
                Err(_) => {
                    error!("Channel is closed");
                    return Err(DeviceError::new("Data channel closed").into());
                }
            }
        }
        render_client.write_to_device_from_deque(
            buffer_free_frame_count as usize,
            blockalign as usize,
            &mut sample_queue,
        )?;
        let curr_buffer_fill = sample_queue.len() / blockalign + sync.rx_play.len() * chunksize;
        sync.bufferfill.store(curr_buffer_fill, Ordering::Relaxed);
        trace!("write ok");
        if handle.wait_for_event(1000).is_err() {
            error!("Error on playback, stopping stream");
            audio_client.stop_stream()?;
            return Err(DeviceError::new("Error on playback").into());
        }
    }
}

// Capture loop, capture samples and send in chunks of "chunksize" frames to channel
fn capture_loop(
    audio_client: wasapi::AudioClient,
    capture_client: wasapi::AudioCaptureClient,
    handle: wasapi::Handle,
    tx_capt: Sender<(u64, Vec<u8>)>,
    tx_cb: Sender<DisconnectReason>,
    blockalign: usize,
    stop_signal: Arc<AtomicBool>,
) -> Res<()> {
    let mut chunk_nbr: u64 = 0;

    let mut callbacks = wasapi::EventCallbacks::new();
    callbacks.set_disconnected_callback(move |reason| {
        debug!("Capture disconnected, reason: {:?}", reason);
        let simplereason = match reason {
            wasapi::DisconnectReason::FormatChanged => DisconnectReason::FormatChange,
            _ => DisconnectReason::Error,
        };
        tx_cb.send(simplereason).unwrap_or(());
    });

    let callbacks_rc = Rc::new(callbacks);
    let callbacks_weak = Rc::downgrade(&callbacks_rc);

    let sessioncontrol = audio_client.get_audiosessioncontrol()?;
    sessioncontrol.register_session_notification(callbacks_weak)?;

    audio_client.start_stream()?;
    loop {
        trace!("capturing");
        if stop_signal.load(Ordering::Relaxed) {
            debug!("Stopping inner capture loop on request");
            audio_client.stop_stream()?;
            return Ok(());
        }
        if handle.wait_for_event(1000).is_err() {
            error!("Capture error, stopping stream");
            audio_client.stop_stream()?;
            return Err(DeviceError::new("Error capturing data").into());
        }
        let available_frames = match capture_client.get_next_nbr_frames()? {
            Some(frames) => frames,
            None => audio_client.get_bufferframecount()?,
        };
        trace!("Available frames from capture dev: {}", available_frames);
        // If no available frames, just skip the rest of this loop iteration
        if available_frames > 0 {
            let mut data = vec![0u8; available_frames as usize * blockalign as usize];
            let nbr_frames_read =
                capture_client.read_from_device(blockalign as usize, &mut data)?;
            if nbr_frames_read != available_frames {
                warn!(
                    "Expected {} frames, got {}",
                    available_frames, nbr_frames_read
                );
            }
            match tx_capt.try_send((chunk_nbr, data)) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {
                    error!("Error sending, channel disconnected");
                    audio_client.stop_stream()?;
                    return Err(DeviceError::new("Channel disconnected").into());
                }
            }
            chunk_nbr += 1;
        }
    }
}

/// Start a playback thread listening for AudioMessages via a channel.
impl PlaybackDevice for WasapiPlaybackDevice {
    fn start(
        &mut self,
        channel: mpsc::Receiver<AudioMessage>,
        barrier: Arc<Barrier>,
        status_channel: mpsc::Sender<StatusMessage>,
        playback_status: Arc<RwLock<PlaybackStatus>>,
    ) -> Res<Box<thread::JoinHandle<()>>> {
        let devname = self.devname.clone();
        let exclusive = self.exclusive;
        let samplerate = self.samplerate;
        let chunksize = self.chunksize;
        let channels = self.channels;
        let target_level = if self.target_level > 0 {
            self.target_level
        } else {
            self.chunksize
        };
        let adjust_period = self.adjust_period;
        let adjust = self.adjust_period > 0.0 && self.enable_rate_adjust;
        let sample_format = self.sample_format.clone();
        let sample_format_dev = self.sample_format.clone();
        let handle = thread::Builder::new()
            .name("WasapiPlayback".to_string())
            .spawn(move || {
                // Devices typically request around 1000 frames per buffer, set a reasonable capacity for the channel
                let channel_capacity = 8 * 1024 / chunksize + 1;
                debug!(
                    "Using a playback channel capacity of {} chunks.",
                    channel_capacity
                );
                let (tx_dev, rx_dev) = bounded(channel_capacity);
                let (tx_state_dev, rx_state_dev) = bounded(0);
                let (tx_cb_dev, rx_cb_dev) = unbounded();
                let buffer_fill = Arc::new(AtomicUsize::new(0));
                let buffer_fill_clone = buffer_fill.clone();
                let mut buffer_avg = countertimer::Averager::new();
                let mut timer = countertimer::Stopwatch::new();
                let mut chunk_stats;

                trace!("Build output stream");
                let mut conversion_result;

                // wasapi device loop
                let innerhandle = thread::Builder::new()
                    .name("WasapiPlaybackInner".to_string())
                    .spawn(move || {
                        let (_device, audio_client, render_client, handle, wave_format) =
                            match open_playback(
                                &devname,
                                samplerate,
                                channels,
                                &sample_format_dev,
                                exclusive,
                            ) {
                                Ok((_device, audio_client, render_client, handle, wave_format)) => {
                                    tx_state_dev.send(DeviceState::Ok).unwrap_or(());
                                    (_device, audio_client, render_client, handle, wave_format)
                                }
                                Err(err) => {
                                    let msg = format!("Playback error: {}", err);
                                    tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                                    return;
                                }
                            };
                        let blockalign = wave_format.get_blockalign();
                        let sync = PlaybackSync {
                            rx_play: rx_dev,
                            tx_cb: tx_cb_dev,
                            bufferfill: buffer_fill_clone,
                        };
                        let result = playback_loop(
                            audio_client,
                            render_client,
                            handle,
                            blockalign as usize,
                            chunksize,
                            sync,
                        );
                        if let Err(err) = result {
                            let msg = format!("Playback failed with error: {}", err);
                            //error!("{}", msg);
                            tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                        }
                    })
                    .unwrap();
                match rx_state_dev.recv() {
                    Ok(DeviceState::Ok) => {}
                    Ok(DeviceState::Error(err)) => {
                        status_channel
                            .send(StatusMessage::PlaybackError(err))
                            .unwrap_or(());
                        barrier.wait();
                        return;
                    }
                    Err(err) => {
                        status_channel
                            .send(StatusMessage::PlaybackError(err.to_string()))
                            .unwrap_or(());
                        barrier.wait();
                        return;
                    }
                }
                match status_channel.send(StatusMessage::PlaybackReady) {
                    Ok(()) => {}
                    Err(_err) => {}
                }
                debug!("Playback device ready and waiting");
                barrier.wait();
                debug!("Playback device starts now!");
                loop {
                    match rx_state_dev.try_recv() {
                        Ok(DeviceState::Ok) => {}
                        Ok(DeviceState::Error(err)) => {
                            send_error_or_playbackformatchange(&status_channel, &rx_cb_dev, err);
                            break;
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(err) => {
                            send_error_or_playbackformatchange(
                                &status_channel,
                                &rx_cb_dev,
                                err.to_string(),
                            );
                            break;
                        }
                    }
                    match channel.recv() {
                        Ok(AudioMessage::Audio(chunk)) => {
                            buffer_avg.add_value(buffer_fill.load(Ordering::Relaxed) as f64);
                            if adjust && timer.larger_than_millis((1000.0 * adjust_period) as u64) {
                                if let Some(av_delay) = buffer_avg.get_average() {
                                    let speed = calculate_speed(
                                        av_delay,
                                        target_level,
                                        adjust_period,
                                        samplerate as u32,
                                    );
                                    timer.restart();
                                    buffer_avg.restart();
                                    debug!(
                                        "Current buffer level {}, set capture rate to {}%",
                                        av_delay,
                                        100.0 * speed
                                    );
                                    status_channel
                                        .send(StatusMessage::SetSpeed(speed))
                                        .unwrap_or(());
                                    playback_status.write().unwrap().buffer_level =
                                        av_delay as usize;
                                }
                            }
                            chunk_stats = chunk.get_stats();
                            playback_status.write().unwrap().signal_rms = chunk_stats.rms_db();
                            playback_status.write().unwrap().signal_peak = chunk_stats.peak_db();
                            let mut buf =
                                vec![
                                    0u8;
                                    channels * chunk.frames * sample_format.bytes_per_sample()
                                ];
                            conversion_result =
                                chunk_to_buffer_rawbytes(&chunk, &mut buf, &sample_format);
                            match tx_dev.send(PlaybackDeviceMessage::Data(buf)) {
                                Ok(_) => {}
                                Err(err) => {
                                    error!("Playback device channel error: {}", err);
                                    send_error_or_playbackformatchange(
                                        &status_channel,
                                        &rx_cb_dev,
                                        err.to_string(),
                                    );
                                    break;
                                }
                            }
                            if conversion_result.1 > 0 {
                                playback_status.write().unwrap().clipped_samples +=
                                    conversion_result.1;
                            }
                        }
                        Ok(AudioMessage::EndOfStream) => {
                            status_channel
                                .send(StatusMessage::PlaybackDone)
                                .unwrap_or(());
                            break;
                        }
                        Err(err) => {
                            error!("Message channel error: {}", err);
                            send_error_or_playbackformatchange(
                                &status_channel,
                                &rx_cb_dev,
                                err.to_string(),
                            );
                            break;
                        }
                    }
                }
                match tx_dev.send(PlaybackDeviceMessage::Stop) {
                    Ok(_) => {
                        debug!("Wait for inner playback thread to exit");
                        innerhandle.join().unwrap_or(());
                    }
                    Err(_) => {
                        warn!("Inner playback thread already stopped")
                    }
                }
            })?;
        Ok(Box::new(handle))
    }
}

fn check_for_format_change(rx: &Receiver<DisconnectReason>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(DisconnectReason::Error) => {}
            Ok(DisconnectReason::FormatChange) => {
                return true;
            }
            Err(TryRecvError::Empty) => {
                return false;
            }
            Err(_) => {
                return false;
            }
        }
    }
}

fn send_error_or_playbackformatchange(
    tx: &mpsc::Sender<StatusMessage>,
    rx: &Receiver<DisconnectReason>,
    err: String,
) {
    if check_for_format_change(rx) {
        debug!("Send PlaybackFormatChange");
        tx.send(StatusMessage::PlaybackFormatChange(0))
            .unwrap_or(());
    } else {
        debug!("Send PlaybackError");
        tx.send(StatusMessage::PlaybackError(err)).unwrap_or(());
    }
}

fn send_error_or_captureformatchange(
    tx: &mpsc::Sender<StatusMessage>,
    rx: &Receiver<DisconnectReason>,
    err: String,
) {
    if check_for_format_change(rx) {
        debug!("Send CaptureFormatChange");
        tx.send(StatusMessage::CaptureFormatChange(0)).unwrap_or(());
    } else {
        debug!("Send CaptureError");
        tx.send(StatusMessage::CaptureError(err)).unwrap_or(());
    }
}

fn get_nbr_capture_frames(
    resampler: &Option<Box<dyn VecResampler<PrcFmt>>>,
    capture_frames: usize,
) -> usize {
    if let Some(resampl) = &resampler {
        #[cfg(feature = "debug")]
        trace!("Resampler needs {} frames", resampl.nbr_frames_needed());
        resampl.nbr_frames_needed()
    } else {
        capture_frames
    }
}

/// Start a capture thread providing AudioMessages via a channel
impl CaptureDevice for WasapiCaptureDevice {
    fn start(
        &mut self,
        channel: mpsc::SyncSender<AudioMessage>,
        barrier: Arc<Barrier>,
        status_channel: mpsc::Sender<StatusMessage>,
        command_channel: mpsc::Receiver<CommandMessage>,
        capture_status: Arc<RwLock<CaptureStatus>>,
    ) -> Res<Box<thread::JoinHandle<()>>> {
        let exclusive = self.exclusive;
        let loopback = self.loopback;
        let devname = self.devname.clone();
        let samplerate = self.samplerate;
        let capture_samplerate = self.capture_samplerate;
        let chunksize = self.chunksize;
        let channels = self.channels;
        let bytes_per_sample = self.sample_format.bytes_per_sample();
        let sample_format = self.sample_format.clone();
        let sample_format_dev = self.sample_format.clone();
        let enable_resampling = self.enable_resampling;
        let resampler_conf = self.resampler_conf.clone();
        let async_src = resampler_is_async(&resampler_conf);
        let silence_timeout = self.silence_timeout;
        let silence_threshold = self.silence_threshold;
        let stop_on_rate_change = self.stop_on_rate_change;
        let rate_measure_interval = (1000.0 * self.rate_measure_interval) as u64;
        let handle = thread::Builder::new()
            .name("WasapiCapture".to_string())
            .spawn(move || {
                let mut resampler = if enable_resampling {
                    debug!("Creating resampler");
                    get_resampler(
                        &resampler_conf,
                        channels,
                        samplerate,
                        capture_samplerate,
                        chunksize,
                    )
                } else {
                    None
                };
                // Devices typically give around 1000 frames per buffer, set a reasonable capacity for the channel
                let channel_capacity = 8*chunksize/1024 + 1;
                debug!("Using a capture channel capacity of {} buffers.", channel_capacity);
                let (tx_dev, rx_dev) = bounded(channel_capacity);
                let (tx_state_dev, rx_state_dev) = bounded(0);
                let (tx_cb_dev, rx_cb_dev) = unbounded();

                trace!("Build input stream");
                // wasapi device loop
                let stop_signal = Arc::new(AtomicBool::new(false));
                let stop_signal_inner = stop_signal.clone();
                let innerhandle = thread::Builder::new()
                    .name("WasapiCaptureInner".to_string())
                    .spawn(move || {
                        let (_device, audio_client, capture_client, handle, wave_format) =
                        match open_capture(&devname, capture_samplerate, channels, &sample_format_dev, exclusive, loopback) {
                            Ok((_device, audio_client, capture_client, handle, wave_format)) => {
                                tx_state_dev.send(DeviceState::Ok).unwrap_or(());
                                (_device, audio_client, capture_client, handle, wave_format)
                            },
                            Err(err) => {
                                let msg = format!("Capture error: {}", err);
                                tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                                return;
                            }
                        };
                        let blockalign = wave_format.get_blockalign();
                        let result = capture_loop(audio_client, capture_client, handle, tx_dev, tx_cb_dev, blockalign as usize, stop_signal_inner);
                        if let Err(err) = result {
                            let msg = format!("Capture failed with error: {}", err);
                            //error!("{}", msg);
                            tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                        }
                    }).unwrap();
                match rx_state_dev.recv() {
                    Ok(DeviceState::Ok) => {},
                    Ok(DeviceState::Error(err)) => {
                        channel.send(AudioMessage::EndOfStream).unwrap_or(());
                        status_channel.send(StatusMessage::CaptureError(err)).unwrap_or(());
                        barrier.wait();
                        return;
                     },
                    Err(err) => {
                        channel.send(AudioMessage::EndOfStream).unwrap_or(());
                        status_channel.send(StatusMessage::CaptureError(err.to_string() )).unwrap_or(());
                        barrier.wait();
                        return;
                    },
                }
                let chunksize_samples = channels * chunksize;
                let mut capture_frames = chunksize;
                let mut averager = countertimer::TimeAverage::new();
                let mut watcher_averager = countertimer::TimeAverage::new();
                let mut valuewatcher = countertimer::ValueWatcher::new(capture_samplerate as f32, RATE_CHANGE_THRESHOLD_VALUE, RATE_CHANGE_THRESHOLD_COUNT);
                let mut value_range = 0.0;
                let mut chunk_stats;
                let mut rate_adjust = 0.0;
                let mut silence_counter = countertimer::SilenceCounter::new(silence_threshold, silence_timeout, capture_samplerate, chunksize);
                let mut state = ProcessingState::Running;
                let blockalign = bytes_per_sample*channels;
                let mut data_queue: VecDeque<u8> = VecDeque::with_capacity(4 * blockalign * chunksize_samples );
                // TODO check if this ever needs to be resized
                let mut data_buffer = vec![0u8; 4 * blockalign * capture_frames];
                let mut expected_chunk_nbr = 0;
                debug!("Capture device ready and waiting");
                match status_channel.send(StatusMessage::CaptureReady) {
                    Ok(()) => {}
                    Err(_err) => {}
                }
                barrier.wait();
                debug!("Capture device starts now!");
                loop {
                    match command_channel.try_recv() {
                        Ok(CommandMessage::Exit) => {
                            debug!("Exit message received, sending EndOfStream");
                            let msg = AudioMessage::EndOfStream;
                            channel.send(msg).unwrap_or(());
                            status_channel.send(StatusMessage::CaptureDone).unwrap_or(());
                            break;
                        }
                        Ok(CommandMessage::SetSpeed { speed }) => {
                            rate_adjust = speed;
                            debug!("Requested to adjust capture speed to {}", speed);
                            if let Some(resampl) = &mut resampler {
                                debug!("Adjusting resampler rate to {}", speed);
                                if async_src {
                                    if resampl.set_resample_ratio_relative(speed).is_err() {
                                        debug!("Failed to set resampling speed to {}", speed);
                                    }
                                }
                                else {
                                    warn!("Requested rate adjust of synchronous resampler. Ignoring request.");
                                }
                            }
                        },
                        Err(_) => {},
                    };
                    match rx_state_dev.try_recv() {
                        Ok(DeviceState::Ok) => {},
                        Ok(DeviceState::Error(err)) => {
                            channel.send(AudioMessage::EndOfStream).unwrap_or(());
                            send_error_or_captureformatchange(&status_channel, &rx_cb_dev, err);
                            break;
                        },
                        Err(TryRecvError::Empty) => {}
                        Err(err) => {
                            channel.send(AudioMessage::EndOfStream).unwrap_or(());
                            send_error_or_captureformatchange(&status_channel, &rx_cb_dev, err.to_string());
                            break;
                        }
                    }
                    capture_frames = get_nbr_capture_frames(
                        &resampler,
                        capture_frames,
                    );
                    let capture_bytes = blockalign * capture_frames;
                    while data_queue.len() < (blockalign * capture_frames) {
                        trace!("capture device needs more samples to make chunk, reading from channel");
                        match rx_dev.recv() {
                            Ok((chunk_nbr, data)) => {
                                trace!("got chunk, length {} bytes", data.len());
                                expected_chunk_nbr += 1;
                                if chunk_nbr > expected_chunk_nbr {
                                    warn!("Samples were dropped, missing {} buffers", chunk_nbr-expected_chunk_nbr);
                                    expected_chunk_nbr = chunk_nbr;
                                }
                                for element in data.iter() {
                                    data_queue.push_back(*element);
                                }
                            }
                            Err(err) => {
                                error!("Channel is closed");
                                channel.send(AudioMessage::EndOfStream).unwrap_or(());
                                send_error_or_captureformatchange(&status_channel, &rx_cb_dev, err.to_string());
                                return;
                            }
                        }
                    }
                    for element in data_buffer.iter_mut().take(capture_bytes) {
                        *element = data_queue.pop_front().unwrap();
                    }
                    let mut chunk = buffer_to_chunk_rawbytes(
                        &data_buffer[0..capture_bytes],
                        channels,
                        &sample_format,
                        capture_bytes,
                        &capture_status.read().unwrap().used_channels,
                    );
                    averager.add_value(capture_frames);
                    if averager.larger_than_millis(capture_status.read().unwrap().update_interval as u64)
                    {
                        let samples_per_sec = averager.get_average();
                        averager.restart();
                        let measured_rate_f = samples_per_sec;
                        debug!(
                            "Measured sample rate is {} Hz",
                            measured_rate_f
                        );
                        let mut capture_status = capture_status.write().unwrap();
                        capture_status.measured_samplerate = measured_rate_f as usize;
                        capture_status.signal_range = value_range as f32;
                        capture_status.rate_adjust = rate_adjust as f32;
                        capture_status.state = state;
                    }
                    watcher_averager.add_value(capture_frames);
                    if watcher_averager.larger_than_millis(rate_measure_interval)
                    {
                        let samples_per_sec = watcher_averager.get_average();
                        watcher_averager.restart();
                        let measured_rate_f = samples_per_sec;
                        debug!(
                            "Measured sample rate is {} Hz",
                            measured_rate_f
                        );
                        let changed = valuewatcher.check_value(measured_rate_f as f32);
                        if changed {
                            warn!("sample rate change detected, last rate was {} Hz", measured_rate_f);
                            if stop_on_rate_change {
                                let msg = AudioMessage::EndOfStream;
                                channel.send(msg).unwrap_or(());
                                status_channel.send(StatusMessage::CaptureFormatChange(measured_rate_f as usize)).unwrap_or(());
                                break;
                            }
                        }
                    }
                    chunk_stats = chunk.get_stats();
                    //trace!("Capture rms {:?}, peak {:?}", chunk_stats.rms_db(), chunk_stats.peak_db());
                    capture_status.write().unwrap().signal_rms = chunk_stats.rms_db();
                    capture_status.write().unwrap().signal_peak = chunk_stats.peak_db();
                    value_range = chunk.maxval - chunk.minval;
                    state = silence_counter.update(value_range);
                    if state == ProcessingState::Running {
                        if let Some(resampl) = &mut resampler {
                            let new_waves = resampl.process(&chunk.waveforms).unwrap();
                            let mut chunk_frames = new_waves.iter().map(|w| w.len()).max().unwrap();
                            if chunk_frames == 0 {
                                chunk_frames = chunksize;
                            }
                            chunk.frames = chunk_frames;
                            chunk.valid_frames = chunk.frames;
                            chunk.waveforms = new_waves;
                        }
                        let msg = AudioMessage::Audio(chunk);
                        if channel.send(msg).is_err() {
                            info!("Processing thread has already stopped.");
                            break;
                        }
                    }
                }
                stop_signal.store(true, Ordering::Relaxed);
                debug!("Wait for inner capture thread to exit");
                innerhandle.join().unwrap_or(());
                let mut capt_stat = capture_status.write().unwrap();
                capt_stat.state = ProcessingState::Inactive;
            })?;
        Ok(Box::new(handle))
    }
}
