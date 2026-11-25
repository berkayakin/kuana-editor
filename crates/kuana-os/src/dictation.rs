//! Dictation: speech to text while the user speaks, for a prompt, as
//! Claude's apps take one. macOS: Apple's Speech framework, on the Mac where
//! the language allows (most do; else Apple's servers), the words coming as
//! they're heard, the project's own words as hints (`Options::hints`).
//! Windows: Windows' own speech recognition (its online speech recognition
//! must be on: Settings → Privacy & security → Speech). Linux has neither:
//! `record` gives audio for a transcription service the app sends it to.
//! Either way, what's heard so far comes again and again as `Event::Words`
//! (each replacing the last), then final.

use std::path::PathBuf;
use std::sync::Arc;

/// What a dictation reports, on any thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Everything heard so far (each replaces the one before); `done`: final.
    Words { text: String, done: bool },
    /// How loud the microphone is, 0 to 1, a dozen times a second.
    Level(f32),
    /// Over: stopped (None), or why it couldn't go on.
    Ended(Option<String>),
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// A BCP 47 language (`en-US`, `tr-TR`), else the system's.
    pub language: Option<String>,
    /// Words to expect: the project's name, its files', its branch.
    pub hints: Vec<String>,
    /// An audio file to listen to in place of the microphone (tests).
    pub file: Option<PathBuf>,
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// A dictation going on: `stop` finishes it (the last words come, final),
/// `cancel` drops what's pending. Dropped, it's cancelled.
pub struct Dictation(imp::Handle);

impl Dictation {
    pub fn stop(&self) {
        self.0.stop();
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Whether the system dictates by itself (macOS, Windows); elsewhere audio
/// goes to a transcription service (`record`).
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Starts listening; events go to `sink` until `Event::Ended`. Where the
/// system wants the user's permission (the microphone, speech recognition)
/// it asks first, and the dictation starts once it's given.
pub fn start(options: Options, sink: impl Fn(Event) + Send + Sync + 'static) -> Result<Dictation, String> {
    imp::start(options, Arc::new(sink)).map(Dictation)
}

/// The level of 32-bit float samples, 0 to 1: their loudness in decibels,
/// from -50 dB (silence) to -10 dB (speaking up close).
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.;
    }
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    let db = 10. * mean.max(1e-10).log10();
    ((db + 50.) / 40.).clamp(0., 1.)
}

/// 16-bit little-endian mono samples as a WAV file (what transcription
/// services take).
pub fn wav(samples: &[u8], rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + samples.len());
    let data = samples.len() as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(samples);
    out
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObjectProtocol};
    use objc2::{AllocAnyThread, sel};
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    use objc2_avf_audio::{AVAudioEngine, AVAudioFile, AVAudioPCMBuffer, AVAudioTime};
    use objc2_foundation::{NSArray, NSError, NSLocale, NSString, NSURL};
    use objc2_speech::{
        SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask, SFSpeechRecognitionTaskHint, SFSpeechRecognizer,
        SFSpeechRecognizerAuthorizationStatus,
    };

    use super::{Event, Options, Sink};

    const SPEECH_OFF: &str = "Speech recognition is off for Kuana: turn it on in System Settings → Privacy & Security → Speech Recognition.";
    const MIC_OFF: &str = "Kuana may not use the microphone: allow it in System Settings → Privacy & Security → Microphone.";

    /// What a running dictation is made of.
    struct Parts {
        request: Retained<SFSpeechAudioBufferRecognitionRequest>,
        task: Retained<SFSpeechRecognitionTask>,
        engine: Option<Retained<AVAudioEngine>>,
        /// Kept so its (internal) queue outlives the task.
        _recognizer: Retained<SFSpeechRecognizer>,
    }

    // SAFETY: these are only touched while `Shared::parts` is locked, one
    // thread at a time, which the Speech framework and AVAudioEngine allow.
    unsafe impl Send for Parts {}

    struct Shared {
        sink: Sink,
        options: Options,
        parts: Mutex<Option<Parts>>,
        /// Stopped or cancelled before it got going: it doesn't start.
        stopped: AtomicBool,
        cancelled: AtomicBool,
        ended: AtomicBool,
    }

    impl Shared {
        /// Ends it once (a cancelled task's error mustn't say more).
        fn end(&self, why: Option<String>) {
            if !self.ended.swap(true, Ordering::SeqCst) {
                (self.sink)(Event::Ended(why));
            }
        }

        /// No more audio: the microphone let go, the request told so.
        fn finish_audio(&self) {
            let parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(parts) = parts.as_ref() {
                if let Some(engine) = &parts.engine {
                    // SAFETY: the engine and its input node live in `parts`.
                    unsafe {
                        if engine.isRunning() {
                            engine.stop();
                        }
                        engine.inputNode().removeTapOnBus(0);
                    }
                }
                // SAFETY: ending a request twice is harmless.
                unsafe { parts.request.endAudio() };
            }
        }
    }

    pub struct Handle(Arc<Shared>);

    impl Handle {
        pub fn stop(&self) {
            self.0.stopped.store(true, Ordering::SeqCst);
            self.0.finish_audio();
            // Not started yet (a permission question is open): `go` ends it.
        }

        pub fn cancel(&self) {
            if self.0.ended.load(Ordering::SeqCst) {
                return;
            }
            self.0.cancelled.store(true, Ordering::SeqCst);
            self.0.finish_audio();
            if let Some(parts) = self.0.parts.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                // SAFETY: the task lives in `parts`.
                unsafe { parts.task.cancel() };
            }
            self.0.end(None);
        }
    }

    pub fn start(options: Options, sink: Sink) -> Result<Handle, String> {
        // A permission said no before: why, at once (no question will come).
        let speech = unsafe { SFSpeechRecognizer::authorizationStatus() };
        if speech == SFSpeechRecognizerAuthorizationStatus::Denied || speech == SFSpeechRecognizerAuthorizationStatus::Restricted {
            return Err(SPEECH_OFF.into());
        }
        if options.file.is_none() {
            let mic = microphone_status();
            if mic == AVAuthorizationStatus::Denied || mic == AVAuthorizationStatus::Restricted {
                return Err(MIC_OFF.into());
            }
        }
        let shared = Arc::new(Shared {
            sink,
            options,
            parts: Mutex::new(None),
            stopped: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        go(shared.clone());
        Ok(Handle(shared))
    }

    fn microphone_status() -> AVAuthorizationStatus {
        // SAFETY: a constant the framework defines.
        match unsafe { AVMediaTypeAudio } {
            Some(audio) => unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) },
            None => AVAuthorizationStatus::NotDetermined,
        }
    }

    /// Asks for what isn't decided yet, one question after the other, then
    /// starts (or ends, if the user stopped it meanwhile or said no).
    fn go(shared: Arc<Shared>) {
        if unsafe { SFSpeechRecognizer::authorizationStatus() } == SFSpeechRecognizerAuthorizationStatus::NotDetermined {
            let asked = shared.clone();
            let answered = RcBlock::new(move |status: SFSpeechRecognizerAuthorizationStatus| {
                if status == SFSpeechRecognizerAuthorizationStatus::Authorized {
                    go(asked.clone());
                } else {
                    asked.end(Some(SPEECH_OFF.into()));
                }
            });
            // SAFETY: the block is copied by the framework.
            unsafe { SFSpeechRecognizer::requestAuthorization(&answered) };
            return;
        }
        if shared.options.file.is_none() && microphone_status() == AVAuthorizationStatus::NotDetermined {
            let asked = shared.clone();
            let answered = RcBlock::new(move |granted: Bool| {
                if granted.as_bool() {
                    go(asked.clone());
                } else {
                    asked.end(Some(MIC_OFF.into()));
                }
            });
            // SAFETY: a constant the framework defines; the block is copied.
            if let Some(audio) = unsafe { AVMediaTypeAudio } {
                unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &answered) };
                return;
            }
        }
        if shared.stopped.load(Ordering::SeqCst) || shared.cancelled.load(Ordering::SeqCst) {
            shared.end(None);
            return;
        }
        if let Err(why) = begin(&shared) {
            shared.end(Some(why));
        }
    }

    fn begin(shared: &Arc<Shared>) -> Result<(), String> {
        let recognizer = match &shared.options.language {
            Some(language) => {
                let locale = NSLocale::localeWithLocaleIdentifier(&NSString::from_str(language));
                unsafe { SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale) }.ok_or_else(|| format!("Dictation doesn't know the language {language}"))?
            }
            None => unsafe { SFSpeechRecognizer::new() },
        };
        if !unsafe { recognizer.isAvailable() } {
            return Err("Speech recognition isn't available right now (for this language, or without a network).".into());
        }
        let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
        // SAFETY: setting a fresh request's options before it's used.
        unsafe {
            request.setShouldReportPartialResults(true);
            request.setTaskHint(SFSpeechRecognitionTaskHint::Dictation);
            // On the Mac (private, offline, no time limit) where it can.
            if recognizer.supportsOnDeviceRecognition() {
                request.setRequiresOnDeviceRecognition(true);
            }
            // macOS 13 on.
            if request.respondsToSelector(sel!(setAddsPunctuation:)) {
                request.setAddsPunctuation(true);
            }
            let hints: Vec<Retained<NSString>> = shared.options.hints.iter().take(100).map(|h| NSString::from_str(h)).collect();
            if !hints.is_empty() {
                request.setContextualStrings(&NSArray::from_retained_slice(&hints));
            }
        }
        let weak = Arc::downgrade(shared);
        let results = RcBlock::new(move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
            let Some(shared) = weak.upgrade() else { return };
            if shared.ended.load(Ordering::SeqCst) {
                return;
            }
            // SAFETY: valid for the call when not null.
            if let Some(result) = unsafe { result.as_ref() } {
                let text = unsafe { result.bestTranscription().formattedString() }.to_string();
                let done = unsafe { result.isFinal() };
                (shared.sink)(Event::Words { text, done });
                if done {
                    shared.finish_audio();
                    shared.end(None);
                }
            } else if let Some(error) = unsafe { error.as_ref() } {
                shared.finish_audio();
                let message = error.localizedDescription().to_string();
                let why = if shared.cancelled.load(Ordering::SeqCst) || message.to_lowercase().contains("cancel") {
                    None
                } else if message.to_lowercase().contains("no speech") {
                    Some("No speech was heard.".to_string())
                } else {
                    Some(message)
                };
                shared.end(why);
            }
        });
        let task = unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &results) };
        let engine = match &shared.options.file {
            Some(path) => {
                read_file(path, &request, shared)?;
                None
            }
            None => Some(listen(&request, shared)?),
        };
        *shared.parts.lock().unwrap_or_else(|e| e.into_inner()) = Some(Parts { request, task, engine, _recognizer: recognizer });
        // Stopped while starting: no more audio.
        if shared.stopped.load(Ordering::SeqCst) {
            shared.finish_audio();
        }
        Ok(())
    }

    /// The microphone's sound into the request, its level to the sink.
    fn listen(request: &Retained<SFSpeechAudioBufferRecognitionRequest>, shared: &Arc<Shared>) -> Result<Retained<AVAudioEngine>, String> {
        let engine = unsafe { AVAudioEngine::new() };
        let input = unsafe { engine.inputNode() };
        let format = unsafe { input.outputFormatForBus(0) };
        if unsafe { format.sampleRate() } <= 0. || unsafe { format.channelCount() } == 0 {
            return Err("There's no microphone to listen to.".into());
        }
        let (request, sink) = (request.clone(), shared.sink.clone());
        let started = Instant::now();
        let last_level = AtomicU64::new(0);
        let tap = RcBlock::new(move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            // SAFETY: the buffer is valid for the call.
            let buffer = unsafe { buffer.as_ref() };
            unsafe { request.appendAudioPCMBuffer(buffer) };
            let now = started.elapsed().as_millis() as u64;
            if now.saturating_sub(last_level.load(Ordering::Relaxed)) >= 80 {
                last_level.store(now, Ordering::Relaxed);
                sink(Event::Level(buffer_level(buffer)));
            }
        });
        // SAFETY: the engine copies the block; bus 0 is the input's only one.
        unsafe {
            input.installTapOnBus_bufferSize_format_block(0, 1024, Some(&format), RcBlock::as_ptr(&tap));
            engine.prepare();
        }
        unsafe { engine.startAndReturnError() }.map_err(|e| {
            unsafe { input.removeTapOnBus(0) };
            format!("The microphone didn't start: {}", e.localizedDescription())
        })?;
        Ok(engine)
    }

    fn buffer_level(buffer: &AVAudioPCMBuffer) -> f32 {
        // SAFETY: float data for the buffer's frames of its first channel, when it has float data.
        unsafe {
            let channels = buffer.floatChannelData();
            if channels.is_null() {
                return 0.;
            }
            let first = *channels;
            super::level(std::slice::from_raw_parts(first.as_ptr(), buffer.frameLength() as usize))
        }
    }

    /// A file's sound into the request at the pace it plays (as from a
    /// microphone), then the end of audio.
    fn read_file(path: &std::path::Path, request: &Retained<SFSpeechAudioBufferRecognitionRequest>, shared: &Arc<Shared>) -> Result<(), String> {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let file = unsafe { AVAudioFile::initForReading_error(AVAudioFile::alloc(), &url) }.map_err(|e| format!("Couldn't read {}: {}", path.display(), e.localizedDescription()))?;
        struct Reading(Retained<AVAudioFile>, Retained<SFSpeechAudioBufferRecognitionRequest>);
        // SAFETY: moved to the reading thread, which alone uses them.
        unsafe impl Send for Reading {}
        let reading = Reading(file, request.clone());
        let shared = shared.clone();
        // A function taking the pair whole: a closure would capture its
        // fields one by one, which aren't `Send` themselves.
        fn play(reading: Reading, shared: Arc<Shared>) {
            let Reading(file, request) = reading;
            let format = unsafe { file.processingFormat() };
            let rate = unsafe { format.sampleRate() }.max(1.);
            while !shared.stopped.load(Ordering::SeqCst) && !shared.cancelled.load(Ordering::SeqCst) {
                let Some(buffer) = (unsafe { AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &format, 4096) }) else { break };
                if unsafe { file.readIntoBuffer_error(&buffer) }.is_err() || unsafe { buffer.frameLength() } == 0 {
                    break;
                }
                unsafe { request.appendAudioPCMBuffer(&buffer) };
                (shared.sink)(Event::Level(buffer_level(&buffer)));
                std::thread::sleep(Duration::from_secs_f64(unsafe { buffer.frameLength() } as f64 / rate));
            }
            unsafe { request.endAudio() };
        }
        std::thread::spawn(move || play(reading, shared));
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use windows::Foundation::TypedEventHandler;
    use windows::Globalization::Language;
    use windows::Media::SpeechRecognition::{
        SpeechContinuousRecognitionCompletedEventArgs, SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession, SpeechRecognitionHypothesisGeneratedEventArgs,
        SpeechRecognitionResultStatus, SpeechRecognitionScenario, SpeechRecognitionTopicConstraint, SpeechRecognizer,
    };
    use windows::core::{HSTRING, Ref};

    use super::{Event, Options, Sink};

    /// Windows' error when its online speech recognition is off.
    const PRIVACY_DECLINED: i32 = 0x8004_5509_u32 as i32;

    struct Shared {
        sink: Sink,
        /// The phrases heard whole so far, and the one being heard.
        heard: Mutex<(String, String)>,
        session: Mutex<Option<(SpeechRecognizer, SpeechContinuousRecognitionSession)>>,
        cancelled: AtomicBool,
        ended: AtomicBool,
    }

    impl Shared {
        fn end(&self, why: Option<String>) {
            if !self.ended.swap(true, Ordering::SeqCst) {
                (self.sink)(Event::Ended(why));
            }
        }

        fn text(&self) -> String {
            let heard = self.heard.lock().unwrap_or_else(|e| e.into_inner());
            match (heard.0.is_empty(), heard.1.is_empty()) {
                (_, true) => heard.0.clone(),
                (true, false) => heard.1.clone(),
                (false, false) => format!("{} {}", heard.0, heard.1),
            }
        }
    }

    pub struct Handle(Arc<Shared>);

    impl Handle {
        pub fn stop(&self) {
            if let Some((_, session)) = self.0.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                // Completed comes after what's pending, with the final words.
                let _ = session.StopAsync();
            } else {
                self.0.cancelled.store(true, Ordering::SeqCst);
            }
        }

        pub fn cancel(&self) {
            if self.0.ended.load(Ordering::SeqCst) {
                return;
            }
            self.0.cancelled.store(true, Ordering::SeqCst);
            if let Some((_, session)) = self.0.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let _ = session.CancelAsync();
            }
            self.0.end(None);
        }
    }

    pub fn start(options: Options, sink: Sink) -> Result<Handle, String> {
        if options.file.is_some() {
            return Err("Dictating from a file is for macOS only".into());
        }
        let shared = Arc::new(Shared { sink, heard: Mutex::new((String::new(), String::new())), session: Mutex::new(None), cancelled: AtomicBool::new(false), ended: AtomicBool::new(false) });
        let starting = shared.clone();
        // Compiling the dictation grammar and starting wait on Windows: not
        // on the caller's (UI) thread.
        std::thread::spawn(move || {
            if let Err(why) = begin(&starting, &options) {
                starting.end(Some(why));
            }
        });
        Ok(Handle(shared))
    }

    fn begin(shared: &Arc<Shared>, options: &Options) -> Result<(), String> {
        let failed = |e: windows::core::Error| {
            if e.code().0 == PRIVACY_DECLINED {
                "Windows' online speech recognition is off: turn it on in Settings → Privacy & security → Speech.".to_string()
            } else {
                format!("Windows' speech recognition didn't start: {}", e.message())
            }
        };
        let recognizer = match &options.language {
            Some(language) => {
                let language = Language::CreateLanguage(&HSTRING::from(language.as_str())).map_err(failed)?;
                SpeechRecognizer::Create(&language).map_err(failed)?
            }
            None => SpeechRecognizer::new().map_err(failed)?,
        };
        let dictation = SpeechRecognitionTopicConstraint::Create(SpeechRecognitionScenario::Dictation, &HSTRING::from("dictation")).map_err(failed)?;
        recognizer.Constraints().map_err(failed)?.Append(&dictation).map_err(failed)?;
        let compiled = recognizer.CompileConstraintsAsync().map_err(failed)?.get().map_err(failed)?;
        if compiled.Status().map_err(failed)? != SpeechRecognitionResultStatus::Success {
            return Err("Windows can't take dictation in this language (its speech pack may be missing).".into());
        }
        // A phrase being heard: shown after the ones heard whole.
        let hearing = Arc::downgrade(shared);
        recognizer
            .HypothesisGenerated(&TypedEventHandler::new(move |_, args: Ref<'_, SpeechRecognitionHypothesisGeneratedEventArgs>| {
                if let (Some(shared), Some(args)) = (hearing.upgrade(), args.as_ref()) {
                    let text = args.Hypothesis()?.Text()?.to_string();
                    shared.heard.lock().unwrap_or_else(|e| e.into_inner()).1 = text;
                    (shared.sink)(Event::Words { text: shared.text(), done: false });
                }
                Ok(())
            }))
            .map_err(failed)?;
        let session = recognizer.ContinuousRecognitionSession().map_err(failed)?;
        let heard = Arc::downgrade(shared);
        session
            .ResultGenerated(&TypedEventHandler::new(move |_, args: Ref<'_, SpeechContinuousRecognitionResultGeneratedEventArgs>| {
                if let (Some(shared), Some(args)) = (heard.upgrade(), args.as_ref()) {
                    let phrase = args.Result()?.Text()?.to_string();
                    {
                        let mut heard = shared.heard.lock().unwrap_or_else(|e| e.into_inner());
                        if !phrase.is_empty() {
                            if !heard.0.is_empty() {
                                heard.0.push(' ');
                            }
                            heard.0.push_str(&phrase);
                        }
                        heard.1.clear();
                    }
                    (shared.sink)(Event::Words { text: shared.text(), done: false });
                }
                Ok(())
            }))
            .map_err(failed)?;
        let ending = Arc::downgrade(shared);
        session
            .Completed(&TypedEventHandler::new(move |_, args: Ref<'_, SpeechContinuousRecognitionCompletedEventArgs>| {
                if let Some(shared) = ending.upgrade() {
                    let status = args.as_ref().and_then(|a| a.Status().ok());
                    if !shared.cancelled.load(Ordering::SeqCst) {
                        (shared.sink)(Event::Words { text: shared.text(), done: true });
                    }
                    let why = match status {
                        Some(SpeechRecognitionResultStatus::Success) | Some(SpeechRecognitionResultStatus::UserCanceled) | None => None,
                        Some(SpeechRecognitionResultStatus::TimeoutExceeded) => None,
                        Some(SpeechRecognitionResultStatus::MicrophoneUnavailable) => Some("There's no microphone Windows lets Kuana use (Settings → Privacy & security → Microphone).".to_string()),
                        Some(SpeechRecognitionResultStatus::NetworkFailure) => Some("Windows' speech recognition needs the network.".to_string()),
                        Some(other) => Some(format!("Windows' speech recognition stopped ({})", other.0)),
                    };
                    shared.end(why);
                }
                Ok(())
            }))
            .map_err(failed)?;
        session.StartAsync().map_err(failed)?.get().map_err(failed)?;
        *shared.session.lock().unwrap_or_else(|e| e.into_inner()) = Some((recognizer, session.clone()));
        // Stopped while starting.
        if shared.cancelled.load(Ordering::SeqCst) {
            let _ = session.StopAsync();
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::{Options, Sink};

    pub struct Handle;

    impl Handle {
        pub fn stop(&self) {}

        pub fn cancel(&self) {}
    }

    pub fn start(_: Options, _: Sink) -> Result<Handle, String> {
        Err("This system has no speech recognition of its own: set a transcription service for dictation".into())
    }
}

/// Recording for a transcription service (Linux): the microphone's sound as
/// 16 kHz, 16-bit mono, from whichever recorder the system has
/// (`pw-record`, `parecord`, `arecord`), its level to the sink.
#[cfg(unix)]
pub mod record {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use super::{Event, Sink};

    pub const RATE: u32 = 16_000;

    /// The recorders tried, in order, and their arguments for raw 16 kHz
    /// 16-bit mono on stdout.
    pub const RECORDERS: [(&str, &[&str]); 3] = [
        ("pw-record", &["--rate", "16000", "--channels", "1", "--format", "s16", "-"]),
        ("parecord", &["--raw", "--rate=16000", "--channels=1", "--format=s16le"]),
        ("arecord", &["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-t", "raw"]),
    ];

    pub struct Recording {
        child: Child,
        audio: Arc<Mutex<Vec<u8>>>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    /// Starts recording with the first recorder on PATH (`device` for
    /// arecord's `-D`, tests: `null`).
    pub fn start(sink: Sink, device: Option<&str>) -> Result<Recording, String> {
        let mut last_error = None;
        for (program, args) in RECORDERS {
            let mut command = Command::new(program);
            command.args(args);
            if let (Some(device), "arecord") = (device, program) {
                command.args(["-D", device]);
            }
            match command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
                Ok(mut child) => {
                    let mut stdout = child.stdout.take().ok_or("no recorder output")?;
                    let audio = Arc::new(Mutex::new(Vec::new()));
                    let into = audio.clone();
                    let reader = std::thread::spawn(move || {
                        let mut chunk = [0u8; 3200]; // 100 ms
                        while let Ok(n) = stdout.read(&mut chunk) {
                            if n == 0 {
                                break;
                            }
                            let samples: Vec<f32> = chunk[..n].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.).collect();
                            sink(Event::Level(super::level(&samples)));
                            into.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
                        }
                    });
                    return Ok(Recording { child, audio, reader: Some(reader) });
                }
                Err(e) => last_error = Some(format!("{program}: {e}")),
            }
        }
        Err(format!("No recorder to listen with: install PipeWire's pw-record, PulseAudio's parecord or ALSA's arecord ({})", last_error.unwrap_or_default()))
    }

    impl Recording {
        /// Stops and gives what was recorded as a WAV file.
        pub fn finish(mut self) -> Vec<u8> {
            self.halt();
            let audio = std::mem::take(&mut *self.audio.lock().unwrap_or_else(|e| e.into_inner()));
            super::wav(&audio, RATE)
        }

        fn halt(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    impl Drop for Recording {
        fn drop(&mut self) {
            self.halt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_go_from_silence_to_loud() {
        assert_eq!(level(&[]), 0.);
        assert_eq!(level(&[0.; 160]), 0.);
        let quiet = level(&[0.003; 160]);
        let loud = level(&[0.3; 160]);
        assert!(quiet < 0.2 && loud > 0.9, "{quiet} {loud}");
    }

    #[test]
    fn wav_files_have_a_proper_header() {
        let samples = [1u8, 0, 255, 127];
        let file = wav(&samples, 16_000);
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()), 36 + 4);
        assert_eq!(&file[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(file[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(file[28..32].try_into().unwrap()), 32_000);
        assert_eq!(&file[36..40], b"data");
        assert_eq!(u32::from_le_bytes(file[40..44].try_into().unwrap()), 4);
        assert_eq!(&file[44..], &samples);
    }

    /// The recorder's plumbing on a machine with ALSA's arecord: its null
    /// device gives silence (no microphone needed).
    #[cfg(unix)]
    #[test]
    #[ignore = "needs arecord (Linux)"]
    fn a_recording_comes_back_as_wav() {
        let levels = Arc::new(std::sync::Mutex::new(0));
        let counted = levels.clone();
        let recording = record::start(Arc::new(move |e| if matches!(e, Event::Level(_)) { *counted.lock().unwrap() += 1 }), Some("null")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        let file = recording.finish();
        assert_eq!(&file[0..4], b"RIFF");
        assert!(file.len() > 44 + 16_000, "about half a second of audio: {}", file.len());
        assert!(*levels.lock().unwrap() > 2);
    }
}
