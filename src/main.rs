use anyhow::Result;
use clap::Parser;
use regex::Regex;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

const TRACE_PIPE_PATH: &str = "/sys/kernel/debug/tracing/trace_pipe";
const DEFAULT_DST_DIR_PATH: &str = "/home/lelloman/monnezza-2";
const DEFAULT_MAIN_LOOP_SLEEP_SECS: u64 = 1;
const DEFAULT_RECORDING_DURATION_SECS: u64 = 20;

type RecorderHandle = std::thread::JoinHandle<()>;

#[derive(Parser, Debug)]
struct CliArgs {
    #[clap(short, long, default_value = DEFAULT_DST_DIR_PATH)]
    pub dst_dir_path: String,

    #[clap(short, long, default_value_t = DEFAULT_MAIN_LOOP_SLEEP_SECS)]
    pub main_loop_sleep_secs: u64,

    #[clap(short, long, default_value_t = DEFAULT_RECORDING_DURATION_SECS)]
    pub recording_duration_secs: u64,
}

struct Recording {
    pub start_time: u128,
    pub enabled: bool,
    pub successful_allocations: u128,
    pub failed_allocations: u128,
    pub unparsed_allocations: u128,
}

#[derive(Debug)]
enum ParseResult {
    Successful,
    Failed,
    Unparsable,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn parse_line(line: &String, line_regex: &Regex) -> ParseResult {
    match line_regex.captures(&line) {
        None => {
            //println!("failed to capture line");
            ParseResult::Unparsable
        }
        Some(capture) => match capture.get(1) {
            None => {
                //println!("Failed to get parsed line group");
                ParseResult::Unparsable
            }
            Some(captured_group) => match u128::from_str_radix(captured_group.as_str(), 16) {
                Ok(value) => {
                    /*println!(
                        "Captured group: <{}> value: {}",
                        captured_group.as_str(),
                        value
                    );*/
                    if value == 0 {
                        ParseResult::Failed
                    } else {
                        ParseResult::Successful
                    }
                }
                Err(_) => {
                    //println!("Failed to parse value {}", captured_group.as_str());
                    ParseResult::Unparsable
                }
            },
        },
    }
}

fn save_recording_file(dst_dir_path: &str, recording: Recording) {
    let mut dst_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{}/{}", dst_dir_path, now_ms().to_string()))
        .expect("Could not create save file");
    let recording_string = format!(
        "start:{}\nend:{}\nenabled:{}\nsuccess:{}\nfailure:{}\nunparsable:{}\n",
        recording.start_time,
        now_ms(),
        recording.enabled,
        recording.successful_allocations,
        recording.failed_allocations,
        recording.unparsed_allocations
    );
    dst_file.write_all(recording_string.as_bytes());
}

fn start_recorder(
    dst_dir_path: String,
    boost_enabled: bool,
    running: Arc<AtomicBool>,
) -> Result<RecorderHandle> {
    let trace_pipe = File::open(TRACE_PIPE_PATH).expect("Could not open trace pipe file");
    let mut reader = BufReader::new(trace_pipe);

    let mut buf = String::from_utf8(vec![0u8; 4096]).unwrap();
    let line_regex = Regex::new(r".*page=([A-z0-9]+)\s.*").unwrap();
    return Ok(std::thread::spawn(move || {
        let mut recording = Recording {
            start_time: now_ms(),
            enabled: boost_enabled,
            successful_allocations: 0,
            failed_allocations: 0,
            unparsed_allocations: 0,
        };
        while running.load(Ordering::SeqCst) {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(_) => {
                    let parsed_result = parse_line(&buf, &line_regex);                    
                    match parsed_result {
                        ParseResult::Successful => recording.successful_allocations += 1,
                        ParseResult::Failed => recording.failed_allocations += 1,
                        ParseResult::Unparsable => recording.unparsed_allocations += 1,
                    }
                }
                Err(x) => {
                    println!("Error while reading line\n{}", x);
                    break;
                }
            }
        }

        println!("Recording stopped, saving file...");
        save_recording_file(&dst_dir_path, recording);
        println!("Saved file")
    }));
}

fn setup_ctrl(running: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        running.store(false, Ordering::SeqCst);
        println!("Shutdown signal received, wait for tear down...");
    })
    .expect("Error setting Ctrl-C handler");
}

fn set_eboost(enabled: bool) -> Result<()> {
    let arg = if enabled { "--on" } else { "--off" };
    Ok(std::process::Command::new("eboostctl")
        .arg(arg)
        .output()
        .map(|_| ())?)
}

fn main() {
    let cli_args = CliArgs::parse();
    let running = Arc::new(AtomicBool::new(true));
    setup_ctrl(running.clone());

    let recorder_running = Arc::new(AtomicBool::new(true));
    let mut boost_enabled = true;
    set_eboost(boost_enabled);

    let mut recorder_handle = start_recorder(
        cli_args.dst_dir_path.clone(),
        boost_enabled,
        recorder_running.clone(),
    )
    .expect("Could not start recorder");
    let mut start_time = Instant::now();

    let loop_sleep_interval = Duration::from_secs(cli_args.main_loop_sleep_secs);
    let recording_duration = Duration::from_secs(cli_args.recording_duration_secs);
    while running.load(Ordering::SeqCst) {
        sleep(loop_sleep_interval);
        let now = Instant::now();
        if now - start_time > recording_duration {
            boost_enabled = !boost_enabled;
            set_eboost(boost_enabled);
            recorder_running.store(false, Ordering::SeqCst);
            recorder_handle
                .join()
                .expect("Could not join recorder thread");
            recorder_running.store(true, Ordering::SeqCst);
            recorder_handle = start_recorder(
                cli_args.dst_dir_path.clone(),
                boost_enabled,
                recorder_running.clone(),
            )
            .expect("Could not start recorder");
            start_time = now;
            println!("Flip recording");
        }
    }
    recorder_running.store(false, Ordering::SeqCst);
    println!("Recorder shut down message sent, wait to join...");
    recorder_handle
        .join()
        .expect("Could not join recorder thread");
    println!("Recorder joined, bye bye.");
}
