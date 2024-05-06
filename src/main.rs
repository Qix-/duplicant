//! Hi. This was written in a day. Please excuse the mess.

#![feature(slice_as_chunks)]
#![cfg_attr(windows, feature(windows_by_handle))]

use byteorder::{LittleEndian, WriteBytesExt};
use clap::Parser;
use crossbeam_channel::{Receiver, Sender};
use sha1::{Digest, Sha1};
use spin::mutex::SpinMutex;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::{
	collections::{HashMap, HashSet},
	io::{BufRead, BufReader, Write},
	path::PathBuf,
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc,
	},
	thread,
	time::Duration,
};

mod signature;

/// Duplicant finds blocks of text that are similar to each other
/// and shows where they exist in the files.
///
/// !!! WARNING !!!
/// This program is *very memory intensive* and will consume a lot of memory
/// if you have a lot of files or large files. Further, computing similarities
/// between files is a CPU-bound operation and will consume a lot of CPU time.
#[derive(Parser)]
struct Options {
	/// Globs to search for files
	#[clap(default_value = "*")]
	globs: Vec<String>,

	/// Output destination (- for stdout)
	#[clap(short = 'o', long = "output", default_value = "-")]
	output: String,

	/// Sorenson Dice coefficient threshold
	#[clap(short = 't', long = "threshold", default_value = "0.97")]
	threshold: f64,

	/// Minimum number of lines that constitute a block
	#[clap(short = 'l', long = "lines", default_value = "3")]
	lines: usize,

	/// Keep whitespace when comparing lines (default is to ignore)
	#[clap(short = 'w', long = "keep-ws")]
	keep_ws: bool,

	/// Minimum line length (after any whitespace removal) to consider
	/// when comparing blocks (default is 0)
	#[clap(short = 'm', long = "min", default_value = "10")]
	min_length: usize,

	/// Number of threads to use (default is number of CPUs)
	#[clap(short = 'n', long = "threads")]
	threads: Option<usize>,
}

enum WorkerMessage {
	Job(Box<dyn FnOnce(Sender<ManagerMessage>) + Send + 'static>),
	Shutdown,
}

enum ManagerMessage {
	Hashes(PathBuf, Vec<(usize, String, signature::Signature)>),
}

struct ThreadPool {
	workers: Vec<thread::JoinHandle<()>>,
	sender: Sender<WorkerMessage>,
	receiver: Receiver<ManagerMessage>,
	queued: Arc<AtomicUsize>,
}

impl ThreadPool {
	pub fn new() -> Self {
		Self::new_with_count((num_cpus::get() - 1).max(1))
	}

	pub fn new_with_count(count: usize) -> Self {
		assert!(count > 0, "thread count must be greater than 0");

		let (sender, receiver) = crossbeam_channel::bounded(count * 2);
		let (wsender, wreceiver) = crossbeam_channel::unbounded();
		let queued = Arc::new(AtomicUsize::new(0));

		let workers = (0..count)
			.map(|_| {
				let receiver = receiver.clone();
				let sender = wsender.clone();
				let queued = queued.clone();
				thread::spawn(move || {
					loop {
						match receiver.recv() {
							Ok(WorkerMessage::Job(job)) => {
								job(sender.clone());
								queued.fetch_sub(1, Ordering::Relaxed);
							}
							Ok(WorkerMessage::Shutdown) => break,
							Err(_) => break,
						}
					}
				})
			})
			.collect();

		Self {
			workers,
			sender,
			receiver: wreceiver,
			queued,
		}
	}

	pub fn send<F>(&self, job: F)
	where
		F: FnOnce(Sender<ManagerMessage>) + Send + 'static,
	{
		self.queued.fetch_add(1, Ordering::Relaxed);
		self.sender.send(WorkerMessage::Job(Box::new(job))).unwrap();
	}

	pub fn is_drained(&self) -> bool {
		self.queued.load(Ordering::Relaxed) == 0
	}

	pub fn shutdown(&mut self) {
		for _ in 0..10000 {
			if self.is_drained() {
				break;
			}
			thread::sleep(Duration::from_millis(10));
		}

		for _ in &self.workers {
			self.sender.send(WorkerMessage::Shutdown).unwrap();
		}

		let taken_workers = self.workers.drain(..).collect::<Vec<_>>();
		for worker in taken_workers {
			worker.join().unwrap();
		}
	}
}

impl Drop for ThreadPool {
	fn drop(&mut self) {
		self.shutdown();
	}
}

struct Line {
	filepath: Arc<PathBuf>,
	line_no: usize,
	line: String,
	signature: signature::Signature,
	similarities: SpinMutex<HashSet<usize>>,
}

impl Line {
	pub fn new(
		line_no: usize,
		filepath: Arc<PathBuf>,
		line: String,
		signature: signature::Signature,
	) -> Self {
		Self {
			filepath,
			line,
			line_no,
			signature,
			similarities: SpinMutex::new(HashSet::new()),
		}
	}
}

fn main() {
	let options = Options::parse();

	if options.globs.is_empty() {
		eprintln!("error: no globs specified");
		std::process::exit(2);
	}

	let mut outstream: Box<dyn Write> = if options.output == "-" {
		if !atty::is(atty::Stream::Stdout) {
			Box::from(std::io::stdout())
		} else {
			eprintln!("error: stdout is a tty, refusing to write binary data");
			std::process::exit(2);
		}
	} else {
		match std::fs::File::create(&options.output) {
			Ok(file) => Box::from(file),
			Err(e) => {
				eprintln!(
					"error: failed to open output file '{}': {e}",
					options.output
				);
				std::process::exit(2);
			}
		}
	};

	let pool = match options.threads {
		Some(threads) => ThreadPool::new_with_count(threads),
		None => ThreadPool::new(),
	};

	let globs = options
		.globs
		.iter()
		.map(|g| {
			glob::glob(g).unwrap_or_else(|e| {
				eprintln!("error: failed to parse glob '{g}': {e}");
				std::process::exit(2);
			})
		})
		.collect::<Vec<_>>();

	let mut files = Vec::new();
	let mut seen_files = HashSet::new();
	for glob in globs {
		for entry in glob {
			let path = match entry {
				Ok(path) => path,
				Err(e) => {
					eprintln!("warning: failed to read path: {e}");
					continue;
				}
			};

			// Get the file's inode or equivalent
			let metadata = match path.metadata() {
				Ok(metadata) => metadata,
				Err(e) => {
					eprintln!("warning: failed to read metadata for '{path:?}': {e}");
					continue;
				}
			};

			if metadata.is_dir() {
				continue;
			}

			#[cfg(unix)]
			let inode = metadata.ino();
			#[cfg(windows)]
			let inode = metadata.file_index().unwrap();

			if !seen_files.insert(inode) {
				continue;
			}

			files.push(path);
		}
	}

	if files.is_empty() {
		eprintln!("error: no files found - check your globs and try again");
		std::process::exit(2);
	}

	eprintln!("discovered {} files", files.len());

	// Hash all the lines in the files
	for filepath in files {
		pool.send(move |sender| {
			// Make a line iterator over the contents of the file,
			// attempting not to read the entire contents all at once
			let file = match std::fs::File::open(&filepath) {
				Ok(file) => file,
				Err(e) => {
					eprintln!("warning: failed to open file '{filepath:?}': {e}");
					return;
				}
			};

			let reader = BufReader::new(file);
			let lines = reader.lines().map_while(Result::ok);

			let mut hashes = Vec::new();
			for (line_no, mut line) in lines.enumerate() {
				if !options.keep_ws {
					line = line.chars().filter(|&x| !char::is_whitespace(x)).collect();
				}

				if line.len() < options.min_length {
					continue;
				}

				let signature = signature::Signature::from(&line);
				hashes.push((line_no + 1, line, signature));
			}

			sender
				.send(ManagerMessage::Hashes(filepath, hashes))
				.unwrap();
		});
	}

	// Wait for all the files to be hashed
	while !pool.is_drained() {
		thread::sleep(Duration::from_millis(10));
	}

	// Collect all of the hashes
	// Yes, we leak here. Sue me!
	let corpus: &'static Vec<Line> = {
		let corpus: &mut Vec<_> = Box::leak(Box::default());

		while let Ok(ManagerMessage::Hashes(path, line_hashes)) = pool.receiver.try_recv() {
			let path = Arc::new(path);
			for (line_no, line, signature) in line_hashes {
				corpus.push(Line::new(line_no, path.clone(), line, signature));
			}
		}

		corpus
	};

	eprintln!("corpus contains {} lines", corpus.len());

	let similiarities = Arc::new(AtomicUsize::new(0));

	// For reporting purposes, we start a thread that will occassionally
	// print out the number of lines that have been sent for processing,
	// and the number of lines still waiting to be processed.
	let (keep_reporting, submitted_jobs, join_handle) = {
		let queued_jobs = pool.queued.clone();
		let keep_reporting = Arc::new(AtomicBool::new(true));
		let submitted_jobs = Arc::new(AtomicUsize::new(0));
		let total_jobs = corpus.len();
		let start_time = std::time::Instant::now();
		let similarities = similiarities.clone();

		eprintln!();
		let join_handle = thread::spawn({
			let submitted_jobs = submitted_jobs.clone();
			let keep_reporting = keep_reporting.clone();

			move || {
				while keep_reporting.load(Ordering::Relaxed) {
					let submitted = submitted_jobs.load(Ordering::Relaxed);
					let queued = queued_jobs.load(Ordering::Relaxed);
					let similarities = similarities.load(Ordering::Relaxed);
					let elapsed_time = start_time.elapsed();
					let elapsed_secs = elapsed_time.as_secs() as f64
						+ f64::from(elapsed_time.subsec_nanos()) / 1_000_000_000.0;

					let eta = if submitted > 0 {
						let remaining = total_jobs - submitted;
						let remaining_secs = (elapsed_secs / submitted as f64) * remaining as f64;
						humantime::format_duration(Duration::from_secs_f64(remaining_secs))
							.to_string()
					} else {
						"??".to_string()
					};

					eprint!(
						"\x1b[2K\rsubmitted {submitted} / {total_jobs} jobs ({}%), {queued} still queued - {similarities} similarities found - {}% (ETA: {eta})",
						(submitted * 100) / total_jobs,
						(submitted.saturating_sub(queued) * 100) / total_jobs,
					);
					thread::sleep(Duration::from_secs(1));
				}
			}
		});

		(keep_reporting, submitted_jobs, join_handle)
	};

	// Now send each line off to be compared to all other lines.
	// Yes, this is O(N^2). No, I don't care.
	for (i, line) in corpus.iter().enumerate() {
		let similarities = similiarities.clone();

		pool.send(move |_| {
			for (j, other) in corpus.iter().enumerate() {
				if i == j {
					continue;
				}

				let score = line.signature.score_str(&other.line);
				if score >= options.threshold {
					similarities.fetch_add(1, Ordering::Relaxed);
					line.similarities.lock().insert(j);
				}
			}
		});

		submitted_jobs.fetch_add(1, Ordering::Relaxed);
	}

	// Drain the pool
	while !pool.is_drained() {
		thread::sleep(Duration::from_millis(10));
	}

	// Stop the reporting thread
	keep_reporting.store(false, Ordering::Relaxed);
	join_handle.join().expect("failed to join reporting thread");

	eprintln!();
	eprintln!(
		"done; found {} similarities",
		similiarities.load(Ordering::Relaxed)
	);

	// Construct a map of paths to their lines, filtering out
	// lines with no similarities.
	// We also keep track of the maximum number of similarities.
	let mut path_map = HashMap::new();
	let mut max_similarities = 0;
	for line in corpus.iter() {
		let similarities = line.similarities.lock().len();
		if similarities == 0 {
			continue;
		}

		if similarities > max_similarities {
			max_similarities = similarities;
		}

		let entry = path_map.entry(&line.filepath).or_insert_with(Vec::new);
		entry.push(line);
	}

	// Sort the lines in each path by line number
	// We also calculate some metrics here for writing
	// the file in a single pass.
	for lines in path_map.values_mut() {
		lines.sort_by_key(|line| line.line_no);
	}

	eprintln!("{} files have similarities", path_map.len());

	// Now we need to output the results. All values are LE.
	//
	// Header:
	//
	//    MAGIC: u32 = 0x4C505544
	//    MAX_SIMILARITY_COUNT: u32
	//    #PATHS: u64
	//    PATHS_OFFSET: u64
	//
	// Section Types:
	//
	//   PATHS: [(sha1: [u8; 20], line_count: u32, lines_offset: u64); #PATHS]
	//   LINES: [(line_no: u32, similarity_count: u32, similarities_offset: u64); line_count]
	//   SIMILARITIES: (path_offset: u64, line_number: u32)

	const MAGIC: u32 = 0x4C505544;

	// Calculate the size of each type
	const HEADER_SIZE: usize = 4 + 4 + 8 + 8;
	const PATH_SIZE: usize = 20 + 4 + 8;
	const LINE_SIZE: usize = 4 + 4 + 8;
	const SIMILARITY_SIZE: usize = 8 + 4;

	// Calculate the size of each section
	let paths_size = path_map.len() * PATH_SIZE;
	let lines_size = path_map
		.values()
		.map(|lines| LINE_SIZE * lines.len())
		.sum::<usize>();

	let paths_offset = HEADER_SIZE;
	let lines_offset = paths_offset + paths_size;
	let similarities_offset = lines_offset + lines_size;

	// Make a reverse map of paths to their index
	let path_index_map = path_map
		.keys()
		.enumerate()
		.map(|(i, path)| (*path, i))
		.collect::<HashMap<_, _>>();

	let res = move || -> Result<(), std::io::Error> {
		outstream.write_u32::<LittleEndian>(MAGIC)?;
		outstream.write_u32::<LittleEndian>(max_similarities as u32)?;
		outstream.write_u64::<LittleEndian>(path_map.len() as u64)?;
		outstream.write_u64::<LittleEndian>(paths_offset as u64)?;

		let mut current_lines_offset = lines_offset;
		for (path, lines) in &path_map {
			let mut hasher = Sha1::new();
			hasher.update(path.to_string_lossy().as_bytes());
			let sha1 = hasher.finalize();
			debug_assert!(sha1.len() == 20);

			outstream.write_all(&sha1)?;
			outstream.write_u32::<LittleEndian>(lines.len() as u32)?;
			outstream.write_u64::<LittleEndian>(current_lines_offset as u64)?;

			current_lines_offset += LINE_SIZE * lines.len();
		}

		let mut current_similarities_offset = similarities_offset;
		for lines in path_map.values() {
			for line in lines {
				outstream.write_u32::<LittleEndian>(line.line_no as u32)?;
				outstream.write_u32::<LittleEndian>(line.similarities.lock().len() as u32)?;
				outstream.write_u64::<LittleEndian>(current_similarities_offset as u64)?;

				current_similarities_offset += SIMILARITY_SIZE * line.similarities.lock().len();
			}
		}

		for lines in path_map.values() {
			for line in lines {
				for similarity in line.similarities.lock().iter() {
					outstream.write_u64::<LittleEndian>(
						((path_index_map.get(&corpus[*similarity].filepath).unwrap() * PATH_SIZE)
							+ paths_offset) as u64,
					)?;
					outstream.write_u32::<LittleEndian>(*similarity as u32)?;
				}
			}
		}

		Ok(())
	}();

	if let Err(e) = res {
		eprintln!("error: failed to write output file: {e}");
		std::process::exit(1);
	}

	eprintln!("OK; wrote '{}'", options.output);
}
