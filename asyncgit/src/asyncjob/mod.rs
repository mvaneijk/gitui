//! provides `AsyncJob` trait and `AsyncSingleJob` struct

#![deny(clippy::expect_used)]

use crate::error::Result;
use crossbeam_channel::Sender;
use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

/// trait that defines an async task we can run on a threadpool
pub trait AsyncJob: Send + Sync + Clone {
	/// can run a synchronous time intensive task
	fn run(&mut self);

	/// can calculate a hash of its result
	fn get_hash(&mut self) -> u64;
}

type HHash = u64;

///
#[derive(Debug, Clone)]
pub struct StatusResult {
	hash: HHash,
	updated: Instant,
}

type LastType<J> = Arc<Mutex<Option<(J, Option<StatusResult>)>>>;

/// Abstraction for a FIFO task queue that will only queue up **one** `next` job.
/// It keeps overwriting the next job until it is actually taken to be processed
#[derive(Debug, Clone)]
pub struct AsyncSingleJob<J: AsyncJob, T: Copy + Send + 'static> {
	next: Arc<Mutex<Option<J>>>,
	last: LastType<J>,
	sender: Sender<T>,
	pending: Arc<Mutex<()>>,
	notification: T,
	unchanged_notification: Option<T>,
}

impl<J: 'static + AsyncJob, T: Copy + Send + 'static>
	AsyncSingleJob<J, T>
{
	///
	pub fn new(
		sender: Sender<T>,
		value: T,
		u_value: Option<T>,
	) -> Self {
		Self {
			next: Arc::new(Mutex::new(None)),
			last: Arc::new(Mutex::new(None)),
			pending: Arc::new(Mutex::new(())),
			notification: value,
			unchanged_notification: u_value,
			sender,
		}
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.try_lock().is_err()
	}

	///
	pub fn is_outdated(&self, dur: Duration) -> bool {
		match self.last.lock() {
			Ok(mut last) => last.take().as_ref().map_or(
				true,
				|(_, status_opt)| {
					status_opt.as_ref().map_or(true, |status| {
						status.updated.elapsed() > dur
					})
				},
			),
			Err(_) => true,
		}
	}

	/// makes sure `next` is cleared and returns `true` if it actually canceled something
	pub fn cancel(&mut self) -> bool {
		if let Ok(mut next) = self.next.lock() {
			if next.is_some() {
				*next = None;
				return true;
			}
		}

		false
	}

	/// take out last finished job
	pub fn take_last(&self) -> Option<(J, Option<StatusResult>)> {
		if let Ok(mut last) = self.last.lock() {
			last.take()
		} else {
			None
		}
	}

	/// spawns `task` if nothing is running currently, otherwise schedules as `next` overwriting if `next` was set before
	pub fn spawn(
		&mut self,
		task: J,
		dur_opt: Option<Duration>,
		force: bool,
	) -> bool {
		let sec_0 = Duration::from_secs(0);
		let dur = dur_opt.unwrap_or(sec_0);

		self.schedule_next(task);
		self.check_for_job(dur, force)
	}

	fn check_for_job(&self, dur: Duration, force: bool) -> bool {
		if !force && self.is_pending() {
			return false;
		}

		let outdated = self.is_outdated(dur);

		if !force && !outdated {
			return false;
		}

		if let Some(task) = self.take_next() {
			let self_arc = self.clone();

			rayon_core::spawn(move || {
				if let Err(e) = self_arc.run_job(task) {
					log::error!("async job error: {}", e);
				}
			});

			return true;
		}

		false
	}

	fn run_job(&self, mut task: J) -> Result<()> {
		//limit the pending scope
		{
			let _pending = self.pending.lock()?;

			task.run();

			if let Ok(mut last) = self.last.lock() {
				let curr_hash = task.get_hash();
				let last_clone = Arc::clone(&self.last);
				*last = Some((
					task,
					Some(StatusResult {
						hash: curr_hash,
						updated: Instant::now(),
					}),
				));

				// we  need to drop the mutex guard, becuase last_hash is
				// requesting another lock on it
				drop(last);
				// if the last result is the same as the current result ...
				let self_hash = Self::last_hash(&last_clone);
				if self_hash
					.map(|last_hash| last_hash == curr_hash)
					.unwrap_or_default()
				{
					match self.unchanged_notification {
						// and an "unchanged" notification is configured ...
						Some(notification) => {
							// send it!
							self.sender.send(notification)?;
						}
						None => {
							self.sender.send(self.notification)?;
						}
					};
				} else {
					self.sender.send(self.notification)?;
				}
			} else {
				self.sender.send(self.notification)?;
			}
		}

		self.check_for_job(Duration::from_secs(0), true);

		Ok(())
	}

	fn schedule_next(&mut self, task: J) {
		if let Ok(mut next) = self.next.lock() {
			*next = Some(task);
		}
	}

	fn take_next(&self) -> Option<J> {
		if let Ok(mut next) = self.next.lock() {
			next.take()
		} else {
			None
		}
	}

	fn last_hash(last: &LastType<J>) -> Option<u64> {
		last.lock().ok().and_then(|last| {
			last.as_ref().map_or(Some(0), |(_, last)| {
				last.as_ref().map(|x| x.hash)
			})
		})
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crossbeam_channel::unbounded;
	use pretty_assertions::assert_eq;
	// use std::collections::hash_map::DefaultHasher;
	use crate::hash;
	use std::hash::{Hash, Hasher};
	use std::{
		sync::atomic::{AtomicU32, Ordering},
		thread::sleep,
		time::Duration,
	};

	#[derive(Clone)]
	struct TestJob {
		v: Arc<AtomicU32>,
		value_to_add: u32,
	}

	impl Hash for TestJob {
		fn hash<H: Hasher>(&self, state: &mut H) {
			self.v.load(Ordering::Relaxed).hash(state);
			self.value_to_add.hash(state);
		}
	}

	impl AsyncJob for TestJob {
		fn run(&mut self) {
			sleep(Duration::from_millis(100));

			self.v.fetch_add(
				self.value_to_add,
				std::sync::atomic::Ordering::Relaxed,
			);
		}

		fn get_hash(&mut self) -> u64 {
			hash(self)
		}
	}

	type Notificaton = ();

	#[test]
	fn test_overwrite() {
		let (sender, receiver) = unbounded();

		let mut job: AsyncSingleJob<TestJob, Notificaton> =
			AsyncSingleJob::new(sender, (), None);

		let task = TestJob {
			v: Arc::new(AtomicU32::new(1)),
			value_to_add: 1,
		};

		assert!(job.spawn(task.clone(), None, false));
		sleep(Duration::from_millis(1));
		for _ in 0..5 {
			assert!(!job.spawn(task.clone(), None, false));
		}

		let _foo = receiver.recv().unwrap();
		let _foo = receiver.recv().unwrap();
		assert!(receiver.is_empty());

		assert_eq!(
			task.v.load(std::sync::atomic::Ordering::Relaxed),
			3
		);
	}

	#[test]
	fn test_cancel() {
		let (sender, receiver) = unbounded();

		let mut job: AsyncSingleJob<TestJob, Notificaton> =
			AsyncSingleJob::new(sender, (), None);

		let task = TestJob {
			v: Arc::new(AtomicU32::new(1)),
			value_to_add: 1,
		};

		assert!(job.spawn(task.clone(), None, false));
		sleep(Duration::from_millis(1));

		for _ in 0..5 {
			assert!(!job.spawn(task.clone(), None, false));
		}
		assert!(job.cancel());

		let _foo = receiver.recv().unwrap();

		assert_eq!(
			task.v.load(std::sync::atomic::Ordering::Relaxed),
			2
		);
	}
}
