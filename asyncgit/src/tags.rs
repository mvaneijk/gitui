//!

use crate::{
	asyncjob::AsyncJob,
	error::Result,
	hash,
	sync::{self},
	CWD,
};

use sync::Tags;

use std::sync::{Arc, Mutex};

enum JobState {
	Request(),
	Response(Result<Tags>), // include hash in response
}

///
#[derive(Clone, Default)]
pub struct AsyncTagsJob {
	state: Arc<Mutex<Option<JobState>>>,
}

///
impl AsyncTagsJob {
	///
	pub fn new() -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request()))),
		}
	}

	///
	pub fn result(&self) -> Option<Result<Tags>> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(state) = state.take() {
				return match state {
					JobState::Request() => None,
					JobState::Response(result) => Some(result),
				};
			}
		}

		None
	}
}

impl AsyncJob for AsyncTagsJob {
	fn run(&mut self) {
		if let Ok(mut state) = self.state.lock() {
			*state = state.take().map(|state| match state {
				JobState::Request() => {
					let tags = sync::get_tags(CWD);
					JobState::Response(tags)
				}
				JobState::Response(result) => {
					JobState::Response(result)
				}
			});
		}
	}

	fn get_hash(&mut self) -> u64 {
		if let Ok(mut state) = self.state.lock() {
			state.take().map_or(0, |state| match state {
				JobState::Request() => 0,
				JobState::Response(result) => {
					if let Ok(res) = result {
						hash(&res)
					} else {
						0
					}
				}
			})
		} else {
			0
		}
	}
}
