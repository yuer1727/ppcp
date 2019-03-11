use failure::Error;
use clap::{Arg, SubCommand, ArgMatches};
use std::thread;
use std::path::PathBuf;
use std::sync::mpsc::*;
use std::time::*;
use indicatif::*;
use std::sync::*;
use std::ops::{Deref, DerefMut};

use copy::*;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Default, Clone)]
pub struct TrackChange<T: PartialEq> {
    val: T,
    changed: bool,
}

impl<T: PartialEq> TrackChange<T> {
    pub fn new(val: T) -> Self {
        TrackChange { val, changed: false, }
    }
    pub fn changed(&mut self) -> bool {
        let r = self.changed;
        self.changed = false;
        r
    }
    pub fn set(&mut self, val: T) {
        if val == self.val {
            return
        }
        self.changed = true;
        self.val = val;
    }
}
impl<T: PartialEq> Deref for TrackChange<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.val
    }
}
impl<T: PartialEq> DerefMut for TrackChange<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.changed = true; // XXX not checking prev value
        &mut self.val
    }
}

// #[derive(Default, Clone)]
pub struct OperationStats {
    files_done: usize,
    bytes_done: usize,
    files_total: TrackChange<usize>,
    bytes_total: TrackChange<usize>,
    current_total: TrackChange<usize>,
    current_done: usize,
    current_path: TrackChange<PathBuf>,
    current_start: Instant,
}

impl Default for OperationStats {
    fn default() -> Self {
        OperationStats {
            files_done: 0,
            bytes_done: 0,
            files_total: TrackChange::new(0),
            bytes_total: TrackChange::new(0),
            current_total: TrackChange::new(0),
            current_done: 0,
            current_path: TrackChange::new(PathBuf::new()),
            current_start: Instant::now(),
        }
    }
}

pub struct App {
    pb_curr: ProgressBar,
    pb_files: ProgressBar,
    pb_bytes: ProgressBar,
    pb_name: ProgressBar,
    last_update: Instant,
    pb_done: Arc<Mutex<()>>,
}

struct SourceWalker {
}

impl SourceWalker {
    fn run(tx: Sender<(PathBuf, PathBuf)>, sources: Vec<PathBuf>) {
        thread::spawn(move || {
            for src in sources {
                let src = src.canonicalize().unwrap();
                for entry in walkdir::WalkDir::new(src.clone()) {
                    match entry {
                        Ok(entry) => {
                            if entry.file_type().is_file() {
                                tx.send((src.clone(), entry.into_path())).expect("send");
                            }
                        }
                        Err(_) => {
                            // TODO
                        }
                    }
                }
            }
        });
    }
}

impl App {
    pub fn new(matches: &ArgMatches) -> Self {
        let pb_name = ProgressBar::with_draw_target(10, ProgressDrawTarget::stdout_nohz());
        pb_name.set_style(ProgressStyle::default_spinner()
            .template("{spinner} {wide_msg} \u{00A0}")
        );
        let pb_curr = ProgressBar::new(10);
        pb_curr.set_style(ProgressStyle::default_bar()
            .template("current {bar:40.} {bytes:>8}/{total_bytes:<8} {elapsed:>5} ETA {eta} {wide_msg} \u{00A0}")
        );
        let pb_files = ProgressBar::with_draw_target(10, ProgressDrawTarget::stdout_nohz());
        pb_files.set_style(ProgressStyle::default_bar()
            .template("files   {bar:40} {pos:>8}/{len:<8} {elapsed:>5} {wide_msg} \u{00A0}")
        );
        let pb_bytes = ProgressBar::with_draw_target(10, ProgressDrawTarget::stdout_nohz());
        pb_bytes.set_style(ProgressStyle::default_bar()
            .template("bytes   {bar:40} {bytes:>8}/{total_bytes:<8} {elapsed:>5} ETA {eta} {wide_msg} \u{00A0}")
        );
        let multi_pb = MultiProgress::new();
        let pb_name = multi_pb.add(pb_name);
        let pb_curr = multi_pb.add(pb_curr);
        let pb_files = multi_pb.add(pb_files);
        let pb_bytes = multi_pb.add(pb_bytes);
        multi_pb.set_move_cursor(true);
        let pb_done = Arc::new(Mutex::new(()));
        let pb_done2 = pb_done.clone();
        thread::spawn(move || {
            let _locked = pb_done2.lock().unwrap();
            multi_pb.join().expect("join");
        });
        
        App {
            pb_curr,
            pb_files,
            pb_bytes,
            pb_name,
            last_update: Instant::now(),
            pb_done,
        }
    }

    fn error_ask(&self, err: String) -> OperationControl {
        OperationControl::Skip // TODO
    }

    fn update_progress(&mut self, stats: &mut OperationStats) {
        // return;
        if Instant::now().duration_since(self.last_update) < Duration::from_millis(97) {
            return
        }
        self.last_update = Instant::now();
        self.pb_name.tick(); // spin the spinner
        if stats.current_path.changed() {
            self.pb_name.set_message(&format!("{}", stats.current_path.display()));
            self.pb_curr.set_length(*stats.current_total as u64);
            stats.current_start = Instant::now();
            self.pb_curr.reset_elapsed();
            self.pb_curr.reset_eta();
        }
        self.pb_curr.set_draw_delta(0);
        self.pb_curr.set_position(stats.current_done as u64);
        // TODO show only measures of last N reads?
        let curr_duration = Instant::now().duration_since(stats.current_start);
        self.pb_curr.set_message(&format!("{}/s", self.fmt_speed(stats.current_done, &curr_duration)));

        if stats.files_total.changed() {
            self.pb_files.set_length(*stats.files_total as u64);
        }
        self.pb_files.set_position(stats.files_done as u64);
        
        if stats.bytes_total.changed() {
            self.pb_bytes.set_length(*stats.bytes_total as u64);
        }
        self.pb_bytes.set_position(stats.bytes_done as u64);
    }

    pub fn run(&mut self, matches: &ArgMatches) -> Result<()> {
        // let mut ui = cursive::Cursive::ncurses();
        // ui.set_fps(16);
        // let sender = ui.cb_sink().clone();
        if let Some(matches) = matches.subcommand_matches("cp") {
            // for sending errors, progress info and other events from worker to ui:
            let (worker_tx, worker_rx) = channel::<WorkerEvent>();
            // for sending user input (retry/skip/abort) to worker:
            let (user_tx, user_rx) = channel::<OperationControl>();
            // fs walker sends files to operation
            let (src_tx, src_rx) = channel();

            let operation = OperationCopy::new(&matches, user_rx, worker_tx, src_rx)?;
            
            let search_path = operation.search_path();
            assert!(!search_path.is_empty());
            SourceWalker::run(src_tx, search_path);

            let mut stats: OperationStats = Default::default();

            let start = Instant::now();

            while let Ok(event) = worker_rx.recv() {
                match event {
                    WorkerEvent::Stat(StatsChange::FilesDone) => { stats.files_done += 1 }
                    WorkerEvent::Stat(StatsChange::FilesTotal) => { *stats.files_total += 1 }
                    WorkerEvent::Stat(StatsChange::BytesTotal(n)) => { *stats.bytes_total += n },
                    WorkerEvent::Stat(StatsChange::Current(p, chunk, done, todo)) => {
                        stats.current_path.set(p);
                        stats.current_total.set(todo);
                        stats.current_done = done;
                        stats.bytes_done += chunk;
                    }
                    WorkerEvent::Status(OperationStatus::Error(err)) => {
                        let answer = self.error_ask(err);
                        user_tx.send(answer).expect("send");
                    },
                    _ => {},
                }
                self.update_progress(&mut stats);
            }
            self.pb_curr.finish();
            self.pb_files.finish();
            self.pb_bytes.finish();
            self.pb_name.finish();
            let ela = Instant::now().duration_since(start);
            let _locked = self.pb_done.lock().unwrap();
            println!("copied {} files ({}) in {} {}/s", *stats.files_total, HumanBytes(*stats.bytes_total as u64), HumanDuration(ela),
                     self.fmt_speed(*stats.bytes_total, &ela));
        }
        Ok(())
    }

    fn fmt_speed(&self, x: usize, ela: &Duration) -> String {
        let speed = if *ela > Duration::from_secs(1) {
            x / ela.as_secs() as usize
        }
        else if *ela > Duration::from_micros(1) && x < std::usize::MAX/1000 {
            x * 1000 / ela.as_micros() as usize
        }
        else if *ela > Duration::from_millis(1) && x < std::usize::MAX/1_000_000 {
            x * 1_000_000 / ela.as_millis() as usize
        }
        else if *ela > Duration::from_nanos(1) && x < std::usize::MAX/1_000_000_000 {
            x * 1_000_000_000 / ela.as_millis() as usize
        }
        else {
            // what the hell are you?
            0
        };
        format!("{}", HumanBytes(speed as u64))
    }

}
